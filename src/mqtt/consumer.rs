//! Is anything on the other side actually *reading* what we publish? (#610)
//!
//! #607 made "we reached the broker" honest. This module answers the next
//! question up, which turned out to be the one that actually bit: UHC can be
//! connected to the broker, publishing discovery messages perfectly, and
//! still produce zero Home Assistant entities - because Home Assistant's own
//! MQTT integration was never added. Discovery messages then sit in the
//! broker with nothing consuming them, while every status UHC showed said
//! "success".
//!
//! Two independent sources of evidence feed one [`ConsumerMonitor`], and
//! [`apply`] - a pure function - is the whole decision:
//!
//! 1. **The Supervisor's proxy to the Home Assistant core API.** Under the
//!    add-on, `GET http://supervisor/core/api/config` returns the running
//!    core's `components` list, and `mqtt` is in it exactly when the MQTT
//!    integration is set up. This is the only source that can say *no*, and
//!    it needs `homeassistant_api: true` in the add-on's `config.yaml`.
//! 2. **Home Assistant's birth announcement.** HA's MQTT integration
//!    publishes `online` to `homeassistant/status` whenever it connects to
//!    the broker (see [`DEFAULT_STATUS_TOPIC`] - that topic is *not* derived
//!    from the discovery prefix). UHC is already connected to that broker, so
//!    subscribing to it upgrades the state the moment the user adds the
//!    integration - no add-on restart, no waiting for the next poll. Home
//!    Assistant's own integration docs name this message as the intended
//!    trigger for publishers to re-send discovery, which is what makes the
//!    zones appear seconds after the integration is added.
//!
//! Neither source is complete on its own. The core API is unavailable when
//! UHC is not running as an add-on (or when `homeassistant_api` is missing).
//! The birth message, for its part, only tells us anything when Home
//! Assistant happens to connect while we are subscribed: an HA that was
//! already connected before UHC started may not announce itself again for
//! hours, and its retain flag is configurable rather than guaranteed, so
//! silence on that topic proves nothing either way.
//!
//! Hence three states, not two - see [`HomeAssistantState`]. "We cannot tell"
//! is a real answer and gets said out loud rather than being rounded to
//! either good news or an accusation.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Add-on environment variable carrying the Supervisor API token. Its
/// absence is the signal that UHC is not running as a Home Assistant add-on,
/// in which case there is no core API to ask.
pub const SUPERVISOR_TOKEN_ENV: &str = "SUPERVISOR_TOKEN";

/// The Supervisor's proxy to the Home Assistant core REST API.
pub const CORE_CONFIG_URL: &str = "http://supervisor/core/api/config";

/// The integration domain that has to be loaded for our discovery messages
/// to be read by anybody.
pub const MQTT_COMPONENT: &str = "mqtt";

/// How often the core API is re-asked. Slow on purpose: this is a
/// configuration fact that changes when a human clicks something, and the
/// birth-message subscription already covers the moment it changes.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// A single request should not be able to wedge the poll loop.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether Home Assistant is actually consuming the discovery messages UHC
/// publishes (#610).
///
/// Deliberately three-valued, and deliberately *not* folded into
/// [`crate::mqtt::MqttConnectionState`]: "the broker took our messages" and
/// "Home Assistant read them" are different facts with different fixes, and
/// #610 is precisely the case where the first is true and the second is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HomeAssistantState {
    /// We have not established either answer. The honest default, and where
    /// UHC stays for a non-add-on install that has never seen Home Assistant
    /// announce itself. Must never be rendered as a problem: it is an
    /// absence of evidence, not evidence of absence.
    #[default]
    Unknown,
    /// Positively established that Home Assistant's MQTT integration is not
    /// set up: the core API answered, and `mqtt` was not among its loaded
    /// components. This is the only state that earns the "add the MQTT
    /// integration" call to action.
    NotConfigured,
    /// Home Assistant's MQTT integration is loaded and reading the broker.
    Consuming,
}

impl HomeAssistantState {
    /// Wire form for `/api/mqtt/status`, alongside `connection` (#607).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotConfigured => "not_configured",
            Self::Consuming => "consuming",
        }
    }
}

/// One observation about Home Assistant, from either source.
///
/// Modelled as data rather than as direct mutations so that [`apply`] can
/// hold the precedence rules in one testable place - the same shape
/// [`crate::api::mqtt_bootstrap::plan`] uses for the broker-config
/// precedence rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// The core API answered and `mqtt` was among its components.
    CoreReportsMqttLoaded,
    /// The core API answered and `mqtt` was *not* among its components.
    CoreReportsMqttAbsent,
    /// The core API could not be asked, did not answer, or answered in a
    /// shape we do not understand. Carries the reason.
    CoreUnavailable(String),
    /// Home Assistant announced itself `online` on the discovery status
    /// topic - it has just connected to the very broker we publish into.
    BirthAnnouncement,
}

/// What UHC currently believes, and why.
///
/// `detail` mirrors [`crate::mqtt::MqttStatus::last_error`]: a raw,
/// diagnosable reason string that the UI renders its own copy from rather
/// than showing verbatim. It is only ever populated for [`
/// HomeAssistantState::Unknown`], where "we cannot tell" is useless without
/// "because...".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsumerSnapshot {
    pub state: HomeAssistantState,
    pub detail: Option<String>,
}

/// Fold one observation into what we already believed.
///
/// The rules, in the order they matter:
///
/// * **The core API is authoritative when it answers.** It is the only
///   source that can establish a negative, and it re-answers every
///   [`POLL_INTERVAL`], so a stale positive cannot outlive the integration
///   being removed by more than one poll.
/// * **A birth announcement only ever upgrades.** Seeing Home Assistant
///   connect to our broker is proof it is consuming; *not* seeing one proves
///   nothing at all, because the message is not retained. So `online` sets
///   [`HomeAssistantState::Consuming`] and nothing else on the MQTT side
///   ever downgrades. In particular an `offline` will-message is ignored
///   entirely: Home Assistant restarts routinely, and turning a restart into
///   "your integration is missing" would be exactly the false alarm #610
///   exists to stop us shipping.
/// * **A failed probe erases nothing.** Losing the ability to ask is not
///   news about Home Assistant, so an unavailable core API keeps whatever
///   was last established and only explains itself when we had nothing to
///   begin with.
pub fn apply(previous: ConsumerSnapshot, evidence: Evidence) -> ConsumerSnapshot {
    match evidence {
        Evidence::CoreReportsMqttLoaded | Evidence::BirthAnnouncement => ConsumerSnapshot {
            state: HomeAssistantState::Consuming,
            detail: None,
        },
        Evidence::CoreReportsMqttAbsent => ConsumerSnapshot {
            state: HomeAssistantState::NotConfigured,
            detail: None,
        },
        Evidence::CoreUnavailable(reason) => {
            if previous.state == HomeAssistantState::Unknown {
                ConsumerSnapshot {
                    state: HomeAssistantState::Unknown,
                    detail: Some(reason),
                }
            } else {
                previous
            }
        }
    }
}

/// Shared belief about Home Assistant, written by the core-API poll task and
/// by the publisher's event loop, read by `/api/mqtt/status`.
///
/// A `std::sync::RwLock` for the same reason [`crate::mqtt::ConnectionMonitor`]
/// uses one: the publisher records observations inline in its `select!` arms,
/// where awaiting the runtime lock would tie the MQTT event loop to whatever
/// `configure`/`set_enabled` is doing.
///
/// Lives on [`crate::mqtt::MqttPublisher`] rather than on the publisher task,
/// because "Home Assistant's MQTT integration is set up" is a fact about Home
/// Assistant, not about our connection - it survives the publisher being
/// reconfigured or restarted.
#[derive(Debug, Default)]
pub struct ConsumerMonitor {
    inner: RwLock<ConsumerSnapshot>,
}

impl ConsumerMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> ConsumerSnapshot {
        self.inner
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Record an observation, resolving it against the current belief via
    /// [`apply`].
    pub fn observe(&self, evidence: Evidence) {
        if let Ok(mut guard) = self.inner.write() {
            let previous = guard.clone();
            let next = apply(previous.clone(), evidence);
            if next != previous {
                tracing::debug!(
                    from = previous.state.as_str(),
                    to = next.state.as_str(),
                    "Home Assistant consumer state changed"
                );
            }
            *guard = next;
        }
    }
}

/// Home Assistant's default birth/will topic.
///
/// Deliberately a constant rather than something derived from the discovery
/// prefix. In Home Assistant core the birth/will topic is built from its own
/// `DEFAULT_PREFIX` at module load and is configured separately from the
/// discovery prefix - the two merely share the same default string. Deriving
/// it from the prefix would silently subscribe to the wrong topic for anyone
/// who changed the prefix but left birth/will alone, which is the default
/// behaviour.
pub const DEFAULT_STATUS_TOPIC: &str = "homeassistant/status";

/// Every topic worth listening on for Home Assistant's birth announcement.
///
/// Always includes Home Assistant's own default, because that is where the
/// message actually goes unless the user moved it. A non-default discovery
/// prefix additionally contributes `<prefix>/status`, since moving both
/// together is the obvious convention for someone who renamed the prefix -
/// and an extra subscription to a topic nobody publishes on costs nothing.
pub fn status_topics(discovery_prefix: &str) -> Vec<String> {
    let mut topics = vec![DEFAULT_STATUS_TOPIC.to_string()];
    let derived = format!("{}/status", discovery_prefix.trim_end_matches('/'));
    if derived != DEFAULT_STATUS_TOPIC {
        topics.push(derived);
    }
    topics
}

/// Whether a topic is one Home Assistant announces itself on, so the
/// publisher's inbound handler can tell a birth message apart from the
/// command topics it otherwise routes.
pub fn is_status_topic(discovery_prefix: &str, topic: &str) -> bool {
    status_topics(discovery_prefix)
        .iter()
        .any(|candidate| candidate == topic)
}

/// Read a payload on the status topic as evidence, or as nothing.
///
/// Only `online` means anything. See [`apply`] for why `offline` is
/// deliberately not treated as bad news.
pub fn parse_status_payload(payload: &[u8]) -> Option<Evidence> {
    let text = String::from_utf8_lossy(payload);
    if text.trim().eq_ignore_ascii_case("online") {
        Some(Evidence::BirthAnnouncement)
    } else {
        None
    }
}

/// Turn a `GET /core/api/config` body into evidence.
///
/// Pure, so the shape-handling is testable without a Supervisor: an answer
/// we cannot parse has to land on [`Evidence::CoreUnavailable`] rather than
/// on "no mqtt", because "the response looked unfamiliar" and "the
/// integration is missing" would otherwise be indistinguishable - and only
/// one of them should tell the user to go and add an integration.
pub fn classify_core_config(body: &serde_json::Value) -> Evidence {
    let Some(components) = body.get("components").and_then(|value| value.as_array()) else {
        return Evidence::CoreUnavailable(
            "the Home Assistant API answered without a components list".to_string(),
        );
    };
    let loaded = components
        .iter()
        .filter_map(|value| value.as_str())
        // `components` carries both `mqtt` and platform entries like
        // `sensor.mqtt`; the bare domain is the one that means the
        // integration itself is set up.
        .any(|component| component == MQTT_COMPONENT);
    if loaded {
        Evidence::CoreReportsMqttLoaded
    } else {
        Evidence::CoreReportsMqttAbsent
    }
}

/// Ask the Home Assistant core API, through the Supervisor proxy, whether
/// the MQTT integration is loaded.
///
/// Every failure path yields [`Evidence::CoreUnavailable`] with a reason
/// rather than a guess. A 401 in particular means the add-on is missing
/// `homeassistant_api: true`, which is worth saying in as many words: it is
/// a packaging mistake, not a user mistake.
pub async fn probe_core(client: &reqwest::Client, token: &str, url: &str) -> Evidence {
    let response = match client
        .get(url)
        .bearer_auth(token)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return Evidence::CoreUnavailable(format!(
                "could not reach the Home Assistant API: {error}"
            ))
        }
    };

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Evidence::CoreUnavailable(
            "the Home Assistant API refused the add-on's token; the add-on needs \
             homeassistant_api: true"
                .to_string(),
        );
    }
    if !status.is_success() {
        return Evidence::CoreUnavailable(format!(
            "the Home Assistant API answered with HTTP {}",
            status.as_u16()
        ));
    }

    match response.json::<serde_json::Value>().await {
        Ok(body) => classify_core_config(&body),
        Err(error) => Evidence::CoreUnavailable(format!(
            "the Home Assistant API answer could not be read: {error}"
        )),
    }
}

/// Override for [`CORE_CONFIG_URL`], so an install that is not an add-on can
/// still be pointed at a Home Assistant instance (and so both detection
/// outcomes can be exercised against a stand-in during development).
pub const CORE_URL_ENV: &str = "UHC_HA_API_URL";

/// Override for [`SUPERVISOR_TOKEN_ENV`]: a Home Assistant long-lived access
/// token, for the same non-add-on case.
pub const CORE_TOKEN_ENV: &str = "UHC_HA_API_TOKEN";

/// Why we cannot tell, when UHC is simply not running inside Home Assistant.
/// Not a fault, and Settings must not render it as one.
pub const NOT_AN_ADDON: &str =
    "UHC is not running as a Home Assistant add-on, so it cannot ask Home Assistant \
     whether the MQTT integration is set up";

/// Resolve the core API endpoint and token from the environment.
///
/// Separated from [`spawn_core_poll`] so the precedence is visible: an
/// explicit override always wins over the Supervisor's own token, because
/// someone who set one is deliberately pointing at a specific instance.
fn core_api_credentials() -> Option<(String, String)> {
    let non_empty = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let url = non_empty(CORE_URL_ENV).unwrap_or_else(|| CORE_CONFIG_URL.to_string());
    let token = non_empty(CORE_TOKEN_ENV).or_else(|| non_empty(SUPERVISOR_TOKEN_ENV))?;
    Some((url, token))
}

/// Start the background poll that asks Home Assistant whether its MQTT
/// integration is loaded (#610).
///
/// Polls immediately and then every [`POLL_INTERVAL`], so Settings has a real
/// answer seconds after startup rather than a minute later. Runs regardless
/// of whether the publisher is switched on: the question is about Home
/// Assistant, and the answer is worth having ready before the user turns
/// publishing on.
///
/// With no token available there is nothing to poll, so no task is spawned at
/// all - the monitor keeps its honest default and records why.
///
/// `shutdown` is the server's own token: the poll would otherwise outlive a
/// Ctrl+C and hang graceful shutdown, since nothing else can interrupt a
/// sleeping ticker (`tests/spawn_cancellation_lint.rs`).
pub fn spawn_core_poll(monitor: Arc<ConsumerMonitor>, shutdown: CancellationToken) {
    let Some((url, token)) = core_api_credentials() else {
        monitor.observe(Evidence::CoreUnavailable(NOT_AN_ADDON.to_string()));
        tracing::debug!(
            "No Home Assistant API token available; \
             not polling for the MQTT integration's presence"
        );
        return;
    };

    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            monitor.observe(Evidence::CoreUnavailable(format!(
                "could not build an HTTP client for the Home Assistant API: {error}"
            )));
            return;
        }
    };

    tokio::spawn(async move {
        // `interval` fires its first tick immediately, which is the point:
        // the startup answer is the one that decides what Settings says to a
        // user who just installed the add-on.
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut previous: Option<HomeAssistantState> = None;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let evidence = probe_core(&client, &token, &url).await;
            monitor.observe(evidence);
            let current = monitor.snapshot().state;
            // Log transitions only. At one poll a minute, logging every
            // answer would bury the add-on log in noise, but the moment the
            // answer changes is exactly what someone diagnosing "no entities"
            // needs to see.
            if previous != Some(current) {
                match current {
                    HomeAssistantState::Consuming => tracing::info!(
                        "Home Assistant's MQTT integration is set up; discovery is being consumed"
                    ),
                    HomeAssistantState::NotConfigured => tracing::warn!(
                        "Home Assistant's MQTT integration is NOT set up, so no entities will \
                         appear however well UHC publishes. Add it in Home Assistant under \
                         Settings > Devices & services > Add integration > MQTT."
                    ),
                    HomeAssistantState::Unknown => tracing::debug!(
                        "Cannot determine whether Home Assistant's MQTT integration is set up"
                    ),
                }
                previous = Some(current);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown() -> ConsumerSnapshot {
        ConsumerSnapshot::default()
    }

    fn consuming() -> ConsumerSnapshot {
        ConsumerSnapshot {
            state: HomeAssistantState::Consuming,
            detail: None,
        }
    }

    fn not_configured() -> ConsumerSnapshot {
        ConsumerSnapshot {
            state: HomeAssistantState::NotConfigured,
            detail: None,
        }
    }

    #[test]
    fn the_default_belief_is_that_we_cannot_tell() {
        // The honest starting point, and the one a non-add-on install stays
        // at. Anything else would be a claim we have not earned.
        assert_eq!(unknown().state, HomeAssistantState::Unknown);
        assert_eq!(HomeAssistantState::default().as_str(), "unknown");
    }

    #[test]
    fn the_core_api_establishes_both_answers() {
        assert_eq!(
            apply(unknown(), Evidence::CoreReportsMqttLoaded),
            consuming()
        );
        assert_eq!(
            apply(unknown(), Evidence::CoreReportsMqttAbsent),
            not_configured()
        );
    }

    /// The live-upgrade path #610 asks for: the user adds the MQTT
    /// integration minutes after starting the add-on, Home Assistant
    /// connects to the broker, and UHC corrects itself without a restart.
    #[test]
    fn a_birth_announcement_upgrades_a_negative_without_a_restart() {
        let after_probe = apply(unknown(), Evidence::CoreReportsMqttAbsent);
        assert_eq!(after_probe.state, HomeAssistantState::NotConfigured);
        assert_eq!(
            apply(after_probe, Evidence::BirthAnnouncement),
            consuming(),
            "watching HA connect to our own broker outranks a poll taken before it did"
        );
    }

    /// The false alarm this whole module exists to avoid. Home Assistant
    /// restarts routinely and fires its will-message every time; if that
    /// downgraded us, Settings would accuse the user of not having set up an
    /// integration they set up weeks ago.
    #[test]
    fn an_offline_will_message_is_not_evidence_of_anything() {
        assert_eq!(parse_status_payload(b"offline"), None);
        assert_eq!(parse_status_payload(b""), None);
        assert_eq!(parse_status_payload(b"something else"), None);
    }

    #[test]
    fn an_online_birth_message_is_recognised_whatever_its_spelling() {
        for payload in [&b"online"[..], b"ONLINE", b" online\n"] {
            assert_eq!(
                parse_status_payload(payload),
                Some(Evidence::BirthAnnouncement),
                "{:?} should read as a birth announcement",
                String::from_utf8_lossy(payload)
            );
        }
    }

    /// Losing the ability to ask is news about us, not about Home Assistant.
    /// Clobbering a established answer with "cannot tell" on one dropped
    /// request would make Settings flap between an answer and a shrug.
    #[test]
    fn an_unavailable_core_api_never_erases_what_we_already_established() {
        let unavailable = Evidence::CoreUnavailable("boom".to_string());
        assert_eq!(
            apply(consuming(), unavailable.clone()),
            consuming(),
            "a dropped request does not un-consume Home Assistant"
        );
        assert_eq!(
            apply(not_configured(), unavailable),
            not_configured(),
            "nor does it retract a positively established negative"
        );
    }

    #[test]
    fn an_unavailable_core_api_explains_itself_when_we_had_nothing() {
        let resolved = apply(
            unknown(),
            Evidence::CoreUnavailable("no SUPERVISOR_TOKEN".to_string()),
        );
        assert_eq!(resolved.state, HomeAssistantState::Unknown);
        assert_eq!(resolved.detail.as_deref(), Some("no SUPERVISOR_TOKEN"));
    }

    /// The core API's own answer is what retracts a stale positive - within
    /// one poll interval, which is the bound this design accepts.
    #[test]
    fn removing_the_integration_is_picked_up_by_the_next_poll() {
        let believed = apply(unknown(), Evidence::BirthAnnouncement);
        assert_eq!(believed.state, HomeAssistantState::Consuming);
        assert_eq!(
            apply(believed, Evidence::CoreReportsMqttAbsent),
            not_configured()
        );
    }

    #[test]
    fn a_components_list_containing_mqtt_reads_as_loaded() {
        let body = serde_json::json!({
            "components": ["sensor", "mqtt", "sensor.mqtt", "media_player"],
        });
        assert_eq!(classify_core_config(&body), Evidence::CoreReportsMqttLoaded);
    }

    /// The exact live failure from #610: a working Home Assistant with no
    /// MQTT integration. Note `sensor.mqtt`-style platform entries are
    /// absent too, but the bare domain is what we key on.
    #[test]
    fn a_components_list_without_mqtt_reads_as_not_configured() {
        let body = serde_json::json!({
            "components": ["sensor", "media_player", "zone", "person"],
        });
        assert_eq!(classify_core_config(&body), Evidence::CoreReportsMqttAbsent);
    }

    /// A platform entry alone must not be mistaken for the integration. This
    /// is the substring bug the `==` in `classify_core_config` avoids: a
    /// `contains("mqtt")` would call this configured.
    #[test]
    fn a_platform_entry_alone_is_not_the_integration() {
        let body = serde_json::json!({ "components": ["sensor.mqtt_eventstream"] });
        assert_eq!(classify_core_config(&body), Evidence::CoreReportsMqttAbsent);
    }

    /// An answer we do not understand is not a negative. Reporting "your
    /// integration is missing" because Home Assistant changed its response
    /// shape would be the same class of lie as #607's.
    #[test]
    fn an_unfamiliar_response_shape_is_unavailable_not_absent() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "components": "mqtt" }),
            serde_json::json!([]),
            serde_json::json!("nope"),
        ] {
            assert!(
                matches!(classify_core_config(&body), Evidence::CoreUnavailable(_)),
                "{body} should be unavailable, not a claim about Home Assistant"
            );
        }
    }

    /// The default prefix must yield exactly one subscription, not a
    /// duplicate pair - subscribing twice to one topic delivers every birth
    /// message twice.
    #[test]
    fn the_default_prefix_yields_one_status_topic() {
        assert_eq!(status_topics("homeassistant"), vec!["homeassistant/status"]);
        // A trailing slash would otherwise produce `homeassistant//status`,
        // a different topic that would both duplicate and never match.
        assert_eq!(
            status_topics("homeassistant/"),
            vec!["homeassistant/status"]
        );
    }

    /// Home Assistant's birth topic does not move when the discovery prefix
    /// does, so the default has to stay subscribed either way - listening
    /// only to `<prefix>/status` would miss every real birth message.
    #[test]
    fn a_renamed_prefix_adds_a_topic_without_dropping_the_default() {
        assert_eq!(
            status_topics("ha-prod"),
            vec!["homeassistant/status", "ha-prod/status"]
        );
        assert!(is_status_topic("ha-prod", "homeassistant/status"));
        assert!(is_status_topic("ha-prod", "ha-prod/status"));
        assert!(!is_status_topic(
            "ha-prod",
            "unified-hifi/media_player/x/y/set"
        ));
    }
}
