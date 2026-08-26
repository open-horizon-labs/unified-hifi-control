//! Where the Home Assistant custom integration stands (#613).
//!
//! An add-on cannot register entities - only an integration running inside
//! Home Assistant core can. What an add-on *can* do is put that integration
//! where core will find it, which is exactly what the add-on's `run.sh`
//! does on every start: it copies `custom_components/unified_hifi_control`
//! (baked into the UHC image) into Home Assistant's config directory.
//!
//! Copying the files is not the end of it. Home Assistant only loads custom
//! integrations at startup, so a fresh install sits on disk doing nothing
//! until the user restarts Home Assistant once - and nothing anywhere tells
//! them that. This module is what lets Settings say it, from two pieces of
//! evidence:
//!
//! 1. **What `run.sh` did**, handed over in `UHC_HA_INTEGRATION_*`. The
//!    add-on already had to decide between install/update/no-op/refuse, so
//!    it reports its own verdict rather than making UHC re-derive it from
//!    a filesystem it may not even have mapped.
//! 2. **Whether Home Assistant has loaded it**, from the same
//!    `GET /core/api/config` `components` list that #610 uses for MQTT. Our
//!    domain appears in that list exactly when core has the integration
//!    loaded, so "files are there but the domain is missing" is precisely
//!    the state that means "restart Home Assistant".
//!
//! Both halves degrade to "cannot tell" rather than to a guess: outside the
//! add-on there is no report and no Supervisor token, and Settings must say
//! nothing rather than invent a restart instruction for someone running
//! UHC standalone.

use std::sync::{Arc, OnceLock, RwLock};

use tokio_util::sync::CancellationToken;

use crate::mqtt::consumer::{core_api_credentials, POLL_INTERVAL, PROBE_TIMEOUT};

/// Set by the add-on's `run.sh` and by nothing else. Its presence is what
/// makes add-on-specific behaviour (integration messaging, MQTT staying off
/// unless asked for) apply to add-on installs only.
pub const ADDON_ENV: &str = "UHC_ADDON";
/// The add-on's own verdict for this start; one of [`InstallOutcome`].
pub const STATUS_ENV: &str = "UHC_HA_INTEGRATION_STATUS";
/// Version of the integration that is now in Home Assistant's config
/// directory, whether this start put it there or a previous one did.
pub const VERSION_ENV: &str = "UHC_HA_INTEGRATION_VERSION";
/// Why, in words, when the outcome alone is not enough to act on.
pub const DETAIL_ENV: &str = "UHC_HA_INTEGRATION_DETAIL";

/// The integration's Home Assistant domain, and therefore the string that
/// appears in core's `components` list once it is loaded.
pub const COMPONENT: &str = "unified_hifi_control";

/// What the add-on did with the integration on this start.
///
/// Deliberately a closed set shared with `run.sh`: the shell script decides,
/// UHC only reports. Anything unrecognised becomes [`InstallOutcome::Unknown`]
/// so an add-on newer than this binary can never make Settings claim
/// something false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Freshly copied in; Home Assistant has never seen it.
    Installed,
    /// Replaced an older copy the add-on had installed itself.
    Updated,
    /// Already the same version; nothing was written.
    Current,
    /// `install_integration: false`.
    SkippedDisabled,
    /// A copy the add-on did not install (HACS, or copied by hand).
    SkippedForeign,
    /// The add-on's own copy, but edited since. Never overwritten.
    SkippedModified,
    /// What is installed is newer than what this image carries.
    SkippedNewer,
    /// Home Assistant's config directory is not mapped into the add-on.
    SkippedUnmapped,
    /// Mapped, but read-only.
    SkippedReadOnly,
    /// This UHC image does not carry the integration at all.
    Unavailable,
    /// Something went wrong; `detail` says what.
    Failed,
    /// A value this binary does not know.
    Unknown,
}

impl InstallOutcome {
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "installed" => Self::Installed,
            "updated" => Self::Updated,
            "current" => Self::Current,
            "skipped_disabled" => Self::SkippedDisabled,
            "skipped_foreign" => Self::SkippedForeign,
            "skipped_modified" => Self::SkippedModified,
            "skipped_newer" => Self::SkippedNewer,
            "skipped_unmapped" => Self::SkippedUnmapped,
            "skipped_readonly" => Self::SkippedReadOnly,
            "unavailable" => Self::Unavailable,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Updated => "updated",
            Self::Current => "current",
            Self::SkippedDisabled => "skipped_disabled",
            Self::SkippedForeign => "skipped_foreign",
            Self::SkippedModified => "skipped_modified",
            Self::SkippedNewer => "skipped_newer",
            Self::SkippedUnmapped => "skipped_unmapped",
            Self::SkippedReadOnly => "skipped_readonly",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// Whether integration files are sitting in Home Assistant's config
    /// directory as a result of this outcome.
    ///
    /// This is the gate on the "restart Home Assistant" message, so it errs
    /// towards silence: [`InstallOutcome::Failed`] leaves whatever was there
    /// before in place, which may be nothing, and telling someone to restart
    /// for a copy that might not exist is worse than saying nothing.
    pub fn integration_is_present(self) -> bool {
        matches!(
            self,
            Self::Installed
                | Self::Updated
                | Self::Current
                | Self::SkippedForeign
                | Self::SkippedModified
                | Self::SkippedNewer
        )
    }

    /// Whether this start is what put a *new* version on disk. Only these
    /// need the user to do anything at all.
    pub fn changed_on_disk(self) -> bool {
        matches!(self, Self::Installed | Self::Updated)
    }
}

/// The add-on's report for this start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub outcome: InstallOutcome,
    pub version: Option<String>,
    pub detail: Option<String>,
}

/// Read the report from the process environment. `None` when UHC is not
/// running as the add-on, which is also the answer for a standalone install
/// that happens to have an integration installed some other way - UHC has no
/// business reporting on a copy it did not place.
pub fn install_report() -> Option<InstallReport> {
    install_report_from_lookup(|key| std::env::var(key).ok())
}

/// [`install_report`] against an arbitrary lookup, so the parsing rules are
/// testable without mutating the process-global environment.
pub fn install_report_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<InstallReport> {
    if !running_as_addon_from_lookup(&lookup) {
        return None;
    }
    let non_empty = |key: &str| {
        lookup(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    Some(InstallReport {
        outcome: non_empty(STATUS_ENV)
            .map_or(InstallOutcome::Unknown, |value| {
                InstallOutcome::parse(&value)
            }),
        version: non_empty(VERSION_ENV),
        detail: non_empty(DETAIL_ENV),
    })
}

/// Whether this process was started by the Home Assistant add-on's `run.sh`.
pub fn running_as_addon() -> bool {
    running_as_addon_from_lookup(|key| std::env::var(key).ok())
}

pub fn running_as_addon_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> bool {
    matches!(
        lookup(ADDON_ENV).as_deref().map(str::trim),
        Some("1" | "true" | "TRUE" | "yes" | "on")
    )
}

/// Whether Home Assistant core has our integration loaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LoadState {
    /// Core lists our domain: the integration is running.
    Loaded,
    /// Core answered, and our domain is not in the list. The only state that
    /// justifies telling someone to restart Home Assistant.
    NotLoaded,
    /// We could not ask, or could not understand the answer.
    #[default]
    Unknown,
}

impl LoadState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::NotLoaded => "not_loaded",
            Self::Unknown => "unknown",
        }
    }
}

/// Turn a `GET /core/api/config` body into a [`LoadState`].
///
/// Pure, and unfamiliar shapes land on [`LoadState::Unknown`] rather than on
/// `NotLoaded` for the same reason #610 does it: "the answer looked
/// unfamiliar" and "the integration is missing" must not be conflated when
/// only one of them should send the user off to restart Home Assistant.
pub fn classify_components(body: &serde_json::Value) -> LoadState {
    let Some(components) = body.get("components").and_then(|value| value.as_array()) else {
        return LoadState::Unknown;
    };
    if components
        .iter()
        .filter_map(|value| value.as_str())
        // `components` carries platform entries like `media_player.foo`
        // alongside bare domains; the bare domain is the one that means the
        // integration itself is set up.
        .any(|component| component == COMPONENT)
    {
        LoadState::Loaded
    } else {
        LoadState::NotLoaded
    }
}

/// The latest answer, shared between the poll task and the settings API.
#[derive(Debug, Default)]
pub struct LoadMonitor {
    state: RwLock<LoadState>,
}

impl LoadMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> LoadState {
        self.state.read().map(|guard| *guard).unwrap_or_default()
    }

    pub fn set(&self, state: LoadState) {
        if let Ok(mut guard) = self.state.write() {
            *guard = state;
        }
    }
}

static LOAD_MONITOR: OnceLock<Arc<LoadMonitor>> = OnceLock::new();

/// Process-wide, like the environment report it sits next to: whether Home
/// Assistant has loaded our integration is a fact about the machine UHC runs
/// on, not about any one request or `AppState`.
pub fn load_monitor() -> Arc<LoadMonitor> {
    LOAD_MONITOR
        .get_or_init(|| Arc::new(LoadMonitor::new()))
        .clone()
}

/// Poll Home Assistant for whether our integration is loaded yet.
///
/// Only meaningful under the add-on, where the Supervisor supplies a token;
/// with no token there is nothing to ask and no task is spawned, leaving the
/// monitor on its honest [`LoadState::Unknown`] default.
///
/// `shutdown` is the server's own token - a sleeping ticker would otherwise
/// outlive Ctrl+C and hang graceful shutdown
/// (`tests/spawn_cancellation_lint.rs`).
pub fn spawn_load_poll(monitor: Arc<LoadMonitor>, shutdown: CancellationToken) {
    let Some((url, token)) = core_api_credentials() else {
        tracing::debug!(
            "No Home Assistant API token available; \
             not polling for the UHC integration's presence"
        );
        return;
    };

    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(
                "Could not build an HTTP client for the Home Assistant API: {error}; \
                 not polling for the UHC integration's presence"
            );
            return;
        }
    };

    tokio::spawn(async move {
        // The first tick fires immediately: a user who just installed the
        // add-on is looking at Settings now, not in a minute.
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut previous: Option<LoadState> = None;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let state = probe_load(&client, &token, &url).await;
            if previous != Some(state) {
                match state {
                    LoadState::Loaded => tracing::info!(
                        "Home Assistant has loaded the Unified Hi-Fi Control integration"
                    ),
                    LoadState::NotLoaded => tracing::info!(
                        "Home Assistant has not loaded the Unified Hi-Fi Control integration yet; \
                         it needs one restart"
                    ),
                    LoadState::Unknown => tracing::debug!(
                        "Could not determine whether Home Assistant has loaded the \
                         Unified Hi-Fi Control integration"
                    ),
                }
                previous = Some(state);
            }
            monitor.set(state);
        }
    });
}

/// One `GET /core/api/config`, classified. Every failure is
/// [`LoadState::Unknown`]: this drives a "restart Home Assistant" message,
/// and a transient network error must never produce one.
pub async fn probe_load(client: &reqwest::Client, token: &str, url: &str) -> LoadState {
    let Ok(response) = client
        .get(url)
        .bearer_auth(token)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
    else {
        return LoadState::Unknown;
    };
    if !response.status().is_success() {
        return LoadState::Unknown;
    }
    match response.json::<serde_json::Value>().await {
        Ok(body) => classify_components(&body),
        Err(_) => LoadState::Unknown,
    }
}

/// `GET /api/home-assistant/integration` - what Settings needs to say
/// whether the integration is there and whether Home Assistant has picked it
/// up yet.
///
/// Every field is `"unknown"`/`false`/absent outside the add-on, so a
/// standalone install renders nothing rather than an instruction that makes
/// no sense there.
#[derive(Debug, serde::Serialize, PartialEq, Eq)]
pub struct IntegrationStatusResponse {
    /// Whether UHC is running as the Home Assistant add-on.
    pub addon: bool,
    /// The add-on's verdict for this start; see [`InstallOutcome::as_str`].
    /// Absent when UHC is not the add-on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
    /// Version now sitting in Home Assistant's config directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Why, when the outcome alone is not actionable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `"loaded"`, `"not_loaded"`, or `"unknown"`.
    pub loaded: String,
    /// The one thing Settings acts on: files are in place, Home Assistant
    /// has not loaded them, so the user needs to restart Home Assistant once.
    /// Never true on a guess - [`LoadState::Unknown`] does not qualify.
    pub needs_restart: bool,
}

pub async fn status() -> axum::Json<IntegrationStatusResponse> {
    axum::Json(build_status(install_report(), load_monitor().get()))
}

/// The pure half of [`status`], so the "needs restart" rule is testable
/// without an environment or a Home Assistant.
pub fn build_status(report: Option<InstallReport>, loaded: LoadState) -> IntegrationStatusResponse {
    let Some(report) = report else {
        return IntegrationStatusResponse {
            addon: false,
            install: None,
            version: None,
            detail: None,
            loaded: LoadState::Unknown.as_str().to_string(),
            needs_restart: false,
        };
    };
    IntegrationStatusResponse {
        addon: true,
        needs_restart: report.outcome.integration_is_present() && loaded == LoadState::NotLoaded,
        install: Some(report.outcome.as_str().to_string()),
        version: report.version,
        detail: report.detail,
        loaded: loaded.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn no_report_outside_the_addon() {
        assert!(install_report_from_lookup(lookup(&[(
            "UHC_HA_INTEGRATION_STATUS",
            "installed"
        )]))
        .is_none());
    }

    #[test]
    fn reads_the_addons_report() {
        let report = install_report_from_lookup(lookup(&[
            ("UHC_ADDON", "1"),
            ("UHC_HA_INTEGRATION_STATUS", "installed"),
            ("UHC_HA_INTEGRATION_VERSION", "1.2.0"),
        ]))
        .expect("running as the add-on");
        assert_eq!(report.outcome, InstallOutcome::Installed);
        assert_eq!(report.version.as_deref(), Some("1.2.0"));
        assert_eq!(report.detail, None);
    }

    #[test]
    fn an_unrecognised_status_never_claims_success() {
        let report = install_report_from_lookup(lookup(&[
            ("UHC_ADDON", "1"),
            ("UHC_HA_INTEGRATION_STATUS", "teleported"),
        ]))
        .expect("running as the add-on");
        assert_eq!(report.outcome, InstallOutcome::Unknown);
        assert!(!report.outcome.integration_is_present());
        assert!(!report.outcome.changed_on_disk());
    }

    #[test]
    fn a_missing_status_is_unknown_not_installed() {
        let report =
            install_report_from_lookup(lookup(&[("UHC_ADDON", "1")])).expect("running as add-on");
        assert_eq!(report.outcome, InstallOutcome::Unknown);
    }

    #[test]
    fn empty_strings_are_absent_not_empty_values() {
        let report = install_report_from_lookup(lookup(&[
            ("UHC_ADDON", "1"),
            ("UHC_HA_INTEGRATION_STATUS", "current"),
            ("UHC_HA_INTEGRATION_VERSION", "   "),
            ("UHC_HA_INTEGRATION_DETAIL", ""),
        ]))
        .expect("running as the add-on");
        assert_eq!(report.version, None);
        assert_eq!(report.detail, None);
    }

    #[test]
    fn outcomes_round_trip_through_their_wire_strings() {
        for outcome in [
            InstallOutcome::Installed,
            InstallOutcome::Updated,
            InstallOutcome::Current,
            InstallOutcome::SkippedDisabled,
            InstallOutcome::SkippedForeign,
            InstallOutcome::SkippedModified,
            InstallOutcome::SkippedNewer,
            InstallOutcome::SkippedUnmapped,
            InstallOutcome::SkippedReadOnly,
            InstallOutcome::Unavailable,
            InstallOutcome::Failed,
        ] {
            assert_eq!(InstallOutcome::parse(outcome.as_str()), outcome);
        }
    }

    #[test]
    fn presence_is_about_files_not_about_success() {
        // Refusing to touch someone else's copy still leaves a copy there.
        assert!(InstallOutcome::SkippedForeign.integration_is_present());
        assert!(InstallOutcome::SkippedModified.integration_is_present());
        assert!(InstallOutcome::SkippedNewer.integration_is_present());
        // These leave nothing we can vouch for.
        assert!(!InstallOutcome::SkippedDisabled.integration_is_present());
        assert!(!InstallOutcome::SkippedReadOnly.integration_is_present());
        assert!(!InstallOutcome::SkippedUnmapped.integration_is_present());
        assert!(!InstallOutcome::Unavailable.integration_is_present());
        assert!(!InstallOutcome::Failed.integration_is_present());
    }

    #[test]
    fn only_a_fresh_write_asks_the_user_to_do_anything() {
        assert!(InstallOutcome::Installed.changed_on_disk());
        assert!(InstallOutcome::Updated.changed_on_disk());
        assert!(!InstallOutcome::Current.changed_on_disk());
        assert!(!InstallOutcome::SkippedForeign.changed_on_disk());
    }

    #[test]
    fn addon_flag_accepts_the_spellings_a_shell_script_produces() {
        for value in ["1", "true", "yes", "on", " 1 "] {
            assert!(running_as_addon_from_lookup(|_| Some(value.to_string())));
        }
        for value in ["0", "false", "no", "off", ""] {
            assert!(!running_as_addon_from_lookup(|_| Some(value.to_string())));
        }
        assert!(!running_as_addon_from_lookup(|_| None));
    }

    #[test]
    fn our_domain_in_components_means_loaded() {
        let body = serde_json::json!({
            "components": ["sensor", "mqtt", "unified_hifi_control", "media_player"]
        });
        assert_eq!(classify_components(&body), LoadState::Loaded);
    }

    #[test]
    fn our_domain_absent_means_not_loaded() {
        let body = serde_json::json!({ "components": ["sensor", "mqtt"] });
        assert_eq!(classify_components(&body), LoadState::NotLoaded);
    }

    #[test]
    fn a_platform_entry_is_not_the_integration() {
        // `media_player.unified_hifi_control` appears once entities exist,
        // but the bare domain is what says core loaded the integration.
        let body = serde_json::json!({ "components": ["media_player.unified_hifi_control"] });
        assert_eq!(classify_components(&body), LoadState::NotLoaded);
    }

    fn report(outcome: InstallOutcome) -> Option<InstallReport> {
        Some(InstallReport {
            outcome,
            version: Some("1.2.0".to_string()),
            detail: None,
        })
    }

    #[test]
    fn standalone_says_nothing_at_all() {
        let status = build_status(None, LoadState::NotLoaded);
        assert!(!status.addon);
        assert!(!status.needs_restart);
        assert_eq!(status.install, None);
        assert_eq!(status.loaded, "unknown");
    }

    #[test]
    fn installed_but_not_loaded_is_the_restart_case() {
        let status = build_status(report(InstallOutcome::Installed), LoadState::NotLoaded);
        assert!(status.needs_restart);
        assert_eq!(status.install.as_deref(), Some("installed"));
        assert_eq!(status.version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn once_home_assistant_has_it_there_is_nothing_to_do() {
        assert!(!build_status(report(InstallOutcome::Installed), LoadState::Loaded).needs_restart);
        assert!(!build_status(report(InstallOutcome::Current), LoadState::Loaded).needs_restart);
    }

    #[test]
    fn never_asks_for_a_restart_on_a_guess() {
        // Cannot reach the Home Assistant API: saying "restart Home
        // Assistant" would be inventing a problem.
        assert!(!build_status(report(InstallOutcome::Installed), LoadState::Unknown).needs_restart);
    }

    #[test]
    fn never_asks_for_a_restart_when_there_is_nothing_to_load() {
        for outcome in [
            InstallOutcome::SkippedDisabled,
            InstallOutcome::SkippedReadOnly,
            InstallOutcome::SkippedUnmapped,
            InstallOutcome::Unavailable,
            InstallOutcome::Failed,
        ] {
            assert!(
                !build_status(report(outcome), LoadState::NotLoaded).needs_restart,
                "{} should not ask for a restart",
                outcome.as_str()
            );
        }
    }

    #[test]
    fn a_copy_we_refused_to_touch_still_needs_loading() {
        // HACS put it there, or the user edited ours. Either way the files
        // exist and Home Assistant still has to be restarted to see them.
        assert!(
            build_status(report(InstallOutcome::SkippedForeign), LoadState::NotLoaded)
                .needs_restart
        );
    }

    #[test]
    fn an_unfamiliar_answer_is_unknown_not_missing() {
        assert_eq!(
            classify_components(&serde_json::json!({ "version": "2026.8.0" })),
            LoadState::Unknown
        );
        assert_eq!(
            classify_components(&serde_json::json!({ "components": "everything" })),
            LoadState::Unknown
        );
    }
}
