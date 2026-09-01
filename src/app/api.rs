//! Client-side API functions for fetching data.
//!
//! These functions use Dioxus server functions to fetch data
//! without causing SSR deadlocks.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

// =============================================================================
// Status Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppStatus {
    pub version: String,
    #[serde(default)]
    pub git_sha: String,
    pub uptime_secs: u64,
    pub bus_subscribers: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RoonStatus {
    pub connected: bool,
    pub core_name: Option<String>,
    pub core_version: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpStatus {
    pub connected: bool,
    pub host: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LmsStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub port: Option<u16>,
}

// =============================================================================
// Controller-auth types (#570)
//
// Client-side mirrors of `src/api/controller_auth.rs`'s wire types. That
// module gates provider/credential routes behind a one-time owner bootstrap
// token; these types and helpers let the Settings UI (and any other
// owner-gated call site) drive `GET /api/controller/status` and
// `POST /api/controller/bootstrap` without duplicating field names that
// could drift from the server shape.
// =============================================================================

/// The exact `error` text `src/api/controller_auth.rs::unauthorized()` uses
/// for a `controller_unauthorized` 401. A shared constant (referenced
/// directly from that server-side function) rather than two independently
/// typed literals, so detection here can never silently drift from the
/// message it is matching against.
pub const CONTROLLER_UNAUTHORIZED_MESSAGE: &str = "Controller authentication required";

/// True when a fetch helper's formatted `Err(String)` came from the
/// controller-auth 401 gate. `response_error` below already routes this case
/// into the bootstrap prompt as a side effect; callers that render their own
/// error text should pass their `Err` through
/// [`suppress_controller_unauthorized`] so the raw HTTP text is not also
/// shown next to the prompt.
pub fn is_controller_unauthorized_error(message: &str) -> bool {
    message.ends_with(CONTROLLER_UNAUTHORIZED_MESSAGE)
}

/// Drop a `controller_unauthorized` error (it already opened the bootstrap
/// prompt) and pass every other error through unchanged, for call sites that
/// display fetch errors directly.
pub fn suppress_controller_unauthorized(message: String) -> Option<String> {
    if is_controller_unauthorized_error(&message) {
        None
    } else {
        Some(message)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ControllerStatus {
    pub authenticated: bool,
    #[serde(default)]
    pub auth_required: bool,
    pub bootstrap_required: bool,
    #[serde(default)]
    pub expires_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ControllerBootstrapRequest {
    pub token: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ControllerBootstrapResponse {
    pub authenticated: bool,
    pub csrf_token: String,
    pub expires_at: u64,
}

// =============================================================================
// HiPhi Cloud installation pairing
// =============================================================================

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct HiphiPrepareResponse {
    pub installation_public_key: String,
    pub installation_fingerprint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HiphiEnrollmentHandoff {
    pub enrollment_capability: String,
    pub installation_audience: String,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct HiphiInitiateRequest {
    pub enrollment: HiphiEnrollmentHandoff,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct HiphiInitiateResponse {
    pub pairing_id: String,
    pub installation_id: String,
    pub pairing_secret: String,
    pub installation_fingerprint: String,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct HiphiCompleteRequest {
    pub account_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct HiphiCompleteResponse {
    pub paired: bool,
    pub installation_id: String,
    pub relay_endpoint: String,
    pub restart_required: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct HiphiPairingStatus {
    pub paired: bool,
    pub installation_id: Option<String>,
    pub connector_state: String,
}

impl HiphiPairingStatus {
    pub fn display_state(&self) -> &'static str {
        match self.connector_state.as_str() {
            "online" => "Connected to HiPhi Cloud",
            "connecting" => "Paired · connecting",
            "offline" => "Paired · reconnecting",
            "revoked" => "Cloud access revoked",
            _ => "Not paired",
        }
    }
}

/// `GET /api/controller/status`.
pub async fn fetch_controller_status() -> Result<ControllerStatus, String> {
    fetch_json("/api/controller/status").await
}

/// `POST /api/controller/bootstrap`.
pub async fn bootstrap_controller(token: &str) -> Result<ControllerBootstrapResponse, String> {
    post_json(
        "/api/controller/bootstrap",
        &ControllerBootstrapRequest {
            token: token.to_string(),
        },
    )
    .await
}

// =============================================================================
// Settings Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AdapterSettings {
    pub roon: bool,
    pub lms: bool,
    pub openhome: bool,
    pub upnp: bool,
    #[serde(default)]
    pub hqplayer: bool,
    #[serde(default)]
    pub spotify: bool,
    #[serde(default)]
    pub applemusic: bool,
    #[serde(default)]
    pub musicassistant: bool,
    #[serde(default)]
    pub mqtt: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppSettings {
    pub adapters: AdapterSettings,
    #[serde(default)]
    pub hide_knobs_page: bool,
    #[serde(default)]
    pub hide_hqp_page: bool,
    #[serde(default)]
    pub hide_lms_page: bool,
}

// =============================================================================
// Provider onboarding types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderOAuthStart {
    pub provider: String,
    pub authorization_url: String,
    pub state: String,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderAuthResponse {
    pub provider: String,
    pub authorized: bool,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyConfigureRequest {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyConfigureResponse {
    pub provider: String,
    pub configured: bool,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub has_client_secret: bool,
}

/// Status of the temporary HTTPS tunnel to the Spotify OAuth callback
/// (#538). `phase` is one of `"idle"`, `"starting"`, `"active"`, `"error"`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyTunnelStatus {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub seconds_remaining: Option<u64>,
    /// Post-allocation self-probe through the public URL: `None` while the
    /// check is still running, `Some(true)` when a real HTTP round trip
    /// succeeded, `Some(false)` when the address did not answer.
    #[serde(default)]
    pub verified: Option<bool>,
    #[serde(default)]
    pub message: Option<String>,
}

impl SpotifyTunnelStatus {
    pub fn is_active(&self) -> bool {
        self.phase == "active"
    }

    pub fn is_starting(&self) -> bool {
        self.phase == "starting"
    }

    pub fn is_error(&self) -> bool {
        self.phase == "error"
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MusicAssistantConfigureRequest {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub tls: bool,
    pub allow_insecure_http: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MusicAssistantEndpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub allow_insecure_http: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MusicAssistantStatusResponse {
    pub provider: String,
    pub configured: bool,
    pub enabled: bool,
    pub running: bool,
    pub endpoint: Option<MusicAssistantEndpoint>,
    pub has_token: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MqttConfigureRequest {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub tls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_prefix: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MqttStatusResponse {
    pub configured: bool,
    pub enabled: bool,
    /// The publisher task exists. Not the same as reaching the broker
    /// (#607) - see [`MqttStatusResponse::is_connected`].
    pub running: bool,
    /// `"disconnected"`, `"connecting"`, or `"connected"` (#607). Defaults
    /// to empty against a server that predates the field, which
    /// [`MqttStatusResponse::is_connected`] treats as "not connected".
    #[serde(default)]
    pub connection: String,
    /// Raw reason the last connection attempt failed (#607). Rendered
    /// through [`MqttStatusResponse::connection_problem`] rather than shown
    /// verbatim.
    #[serde(default)]
    pub last_error: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub tls: Option<bool>,
    pub base_topic: Option<String>,
    pub discovery_prefix: Option<String>,
    pub has_username: bool,
    pub has_password: bool,
    /// `"user"`, `"environment"`, or absent while unconfigured (#605).
    /// `"environment"` means the Home Assistant add-on supplied the broker
    /// from the Supervisor, so Settings shows it as managed rather than
    /// inviting the user to fill in details they never entered.
    #[serde(default)]
    pub source: Option<String>,
    /// Whether Home Assistant is actually reading what we publish (#610):
    /// `"consuming"`, `"not_configured"`, or `"unknown"`. Defaults to empty
    /// against a server that predates the field, which every helper below
    /// treats as "cannot tell" - the safe direction.
    #[serde(default)]
    pub home_assistant: String,
    /// Why we cannot tell, when we cannot (#610). Diagnostic, not copy.
    #[serde(default)]
    pub home_assistant_detail: Option<String>,
    /// Whether a person deliberately chose to publish over MQTT, rather than
    /// the Home Assistant add-on having switched it on for them (#613).
    ///
    /// Defaults to `false` against a server that predates the field, which
    /// is the quiet direction: an old server pairs with old behaviour, where
    /// the add-on auto-enabled MQTT, and that is precisely the case that
    /// should not produce a warning.
    #[serde(default)]
    pub user_opted_in: bool,
}

/// Where UHC's own Home Assistant integration stands (#613), from
/// `GET /api/home-assistant/integration`.
///
/// Every field is inert outside the Home Assistant add-on, so a standalone
/// install renders none of this rather than instructions that make no sense
/// there.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HomeAssistantIntegrationStatus {
    /// Whether UHC is running as the Home Assistant add-on.
    #[serde(default)]
    pub addon: bool,
    /// What the add-on did on this start: `"installed"`, `"updated"`,
    /// `"current"`, `"skipped_*"`, `"unavailable"`, `"failed"`.
    #[serde(default)]
    pub install: Option<String>,
    /// Version now in Home Assistant's config directory.
    #[serde(default)]
    pub version: Option<String>,
    /// Why, when the outcome alone is not actionable. Diagnostic, not copy.
    #[serde(default)]
    pub detail: Option<String>,
    /// `"loaded"`, `"not_loaded"`, or `"unknown"`.
    #[serde(default)]
    pub loaded: String,
    /// The integration is installed and Home Assistant has not loaded it:
    /// one restart away from working. The server never sets this on a guess.
    #[serde(default)]
    pub needs_restart: bool,
}

impl HomeAssistantIntegrationStatus {
    /// Home Assistant has the integration running. Nothing to tell the user.
    pub fn is_loaded(&self) -> bool {
        self.loaded == "loaded"
    }

    /// The add-on could not put the integration in place, and the user has
    /// to do something about it. Not the same as `!needs_restart`: skipping
    /// because a copy is already there, or because the user opted out, is
    /// not a problem.
    pub fn install_blocked(&self) -> bool {
        matches!(
            self.install.as_deref(),
            Some("skipped_readonly" | "skipped_unmapped" | "unavailable" | "failed")
        )
    }
}

impl MqttStatusResponse {
    /// Whether the active broker came from the Home Assistant add-on rather
    /// than from something the user typed in.
    pub fn is_environment_managed(&self) -> bool {
        self.source.as_deref() == Some("environment")
    }

    /// The broker really answered and entities are being published (#607).
    /// This - not `running` - is the only honest basis for saying so.
    pub fn is_connected(&self) -> bool {
        self.connection == "connected"
    }

    /// The publisher is up but has not reached the broker: either the first
    /// attempt has not landed yet, or it keeps failing. `connection_problem`
    /// tells those two apart.
    pub fn is_connecting(&self) -> bool {
        self.connection == "connecting"
    }

    /// Home Assistant's MQTT integration is set up and reading the broker,
    /// so the zones we publish really do become entities (#610).
    pub fn home_assistant_is_consuming(&self) -> bool {
        self.home_assistant == "consuming"
    }

    /// The #610 case, and the only one that earns a call to action: we are
    /// genuinely publishing, and Home Assistant has positively been found
    /// *not* to have its MQTT integration set up.
    ///
    /// Gated on [`Self::is_connected`] on purpose. An unreachable broker is
    /// its own, louder problem with its own fix (#607), and stacking a
    /// second alarm on top of it would send the user to configure an
    /// integration that could not have helped yet.
    ///
    /// Gated on the *positive* `not_configured` on purpose too: `unknown`
    /// deliberately does not reach here, because accusing someone of not
    /// having set up an integration we simply could not check would be the
    /// same dishonesty in the opposite direction.
    pub fn home_assistant_missing(&self) -> bool {
        self.is_connected() && self.home_assistant == "not_configured"
    }

    /// Whether to *warn* about [`Self::home_assistant_missing`] (#613).
    ///
    /// The fact and the warning came apart when the add-on started
    /// installing UHC's own Home Assistant integration. MQTT is then
    /// optional, and an add-on that switched it on by itself would otherwise
    /// have UHC scolding the user for not finishing a setup they never
    /// started. So the warning needs a person behind it: someone who saved
    /// broker settings, or who flipped the toggle.
    ///
    /// Standalone installs are unaffected in practice - they have no add-on
    /// to auto-enable anything, so anyone publishing there opted in.
    pub fn should_warn_home_assistant_missing(&self) -> bool {
        self.home_assistant_missing() && self.user_opted_in
    }

    /// We are publishing but cannot establish whether anyone reads it -
    /// typically a UHC that is not running as a Home Assistant add-on, so
    /// there is no Supervisor to ask. Worth one quiet, non-accusatory line;
    /// never a warning.
    pub fn home_assistant_undetermined(&self) -> bool {
        self.is_connected() && !self.home_assistant_is_consuming() && !self.home_assistant_missing()
    }

    /// The broker is filled in and waiting, but publishing is off (#613).
    /// The one-click opt-in state an add-on user lands in.
    pub fn ready_to_opt_in(&self) -> bool {
        self.configured && !self.enabled && self.is_environment_managed()
    }

    /// `mqtt://host:port` for the configured broker, so an error can name
    /// the address it is about instead of leaving the user to guess.
    pub fn broker_address(&self) -> Option<String> {
        let host = self.host.as_ref()?;
        let scheme = if self.tls.unwrap_or(false) {
            "mqtts"
        } else {
            "mqtt"
        };
        Some(format!(
            "{scheme}://{host}:{}",
            self.port.unwrap_or_default()
        ))
    }

    /// One plain sentence explaining `last_error`, or `None` when there is
    /// nothing wrong to report (#607).
    ///
    /// The raw `rumqttc` text is accurate but unreadable ("I/O: failed to
    /// lookup address information: nodename nor servname provided, or not
    /// known"). Each branch below names the *fix*, because that is the
    /// whole reason the issue asked for the error to reach the user:
    /// "wrong password" and "wrong hostname" look identical otherwise.
    /// Anything unrecognised falls through verbatim rather than being
    /// flattened into a useless generic - an unknown failure the user can
    /// paste into a search beats a reassuring lie.
    pub fn connection_problem(&self) -> Option<String> {
        // A live connection supersedes whatever went wrong before it.
        if self.is_connected() {
            return None;
        }
        let error = self.last_error.as_ref()?;
        let lowered = error.to_lowercase();
        // Credentials are tested FIRST and deliberately: `rumqttc` renders a
        // rejected CONNACK as "Connection refused, return code
        // `NotAuthorized`", which also matches the dead-port branch below.
        // Matching that one first would tell a user with a wrong password to
        // go and check their port number.
        let explanation = if lowered.contains("not authorized")
            || lowered.contains("notauthorized")
            || lowered.contains("bad user name")
            || lowered.contains("badusername")
        {
            "The broker turned us away. Check the username and password."
        } else if lowered.contains("lookup address")
            || lowered.contains("nodename nor servname")
            || lowered.contains("name or service not known")
            || lowered.contains("no such host")
        {
            "That address could not be found. Check the broker's name for a typo."
        } else if lowered.contains("connection refused") {
            "Nothing answered there. Check the port, and that the broker is switched on."
        } else if lowered.contains("no route to host") || lowered.contains("network unreachable") {
            "That address could not be reached from here. Check they are on the same network."
        } else if lowered.contains("timed out") || lowered.contains("timeout") {
            "The broker did not answer in time."
        } else if lowered.contains("certificate") || lowered.contains("tls") {
            "The secure connection was refused. Check the Use TLS setting matches the broker."
        } else {
            return Some(error.clone());
        };
        Some(explanation.to_string())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpotifyAccount {
    pub id: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpotifyAccountResponse {
    pub account: Option<SpotifyAccount>,
    #[serde(default)]
    pub configured: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppleBridgeStatus {
    #[serde(default)]
    pub companions: Vec<AppleBridgeCompanionStatus>,
    pub paired: bool,
    #[serde(default)]
    pub live: bool,
    pub bridge_id: Option<String>,
    pub last_seen: Option<u64>,
    pub has_snapshot: bool,
    #[serde(default)]
    pub pending_pairings: Vec<AppleBridgePendingPairing>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppleBridgePendingPairing {
    pub bridge_id: String,
    pub pairing_code: String,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppleBridgeCompanionStatus {
    pub bridge_id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub paired: bool,
    #[serde(default)]
    pub live: bool,
    pub last_seen: u64,
    pub has_snapshot: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppleBridgePairingResponse {
    pub bridge_id: String,
    pub pairing_code: String,
    pub expires_at: u64,
}

// =============================================================================
// Zone Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Zone {
    pub zone_id: String,
    pub zone_name: String,
    pub source: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    pub dsp: Option<ZoneDsp>,
    /// Whether `/api/collections` implements this zone's provider (#531).
    /// `default`s to `false` so an older cached response (or a field this
    /// build predates) hides the browse panel rather than showing one that
    /// will refuse every call.
    #[serde(default)]
    pub browse_supported: bool,
    /// Which Library-page tabs this zone's provider serves (#573 defect 6):
    /// a subset of `["browse", "playlists", "favorites", "radio"]`, derived
    /// server-side from the capability matrix. `default`s to empty, which
    /// the Library page treats as "no information -- show every tab" so a
    /// response from a build predating this field degrades to the old
    /// behavior rather than hiding everything.
    #[serde(default)]
    pub library_tabs: Vec<String>,
}

// =============================================================================
// Zone management types (`/api/zones/visibility`, `/api/zones/order`)
// =============================================================================

/// One row of the zone management list. Unlike [`Zone`], this includes zones the user has hidden —
/// the management view is the one place that must show them, or unhiding would be impossible.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ManagedZone {
    pub zone_id: String,
    /// The effective display name: the user's override when set, otherwise the provider's.
    pub zone_name: String,
    /// What Roon/LMS/etc. calls this zone, so a renamed zone can still be identified.
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub renamed: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub hidden: bool,
}

impl ManagedZone {
    /// Name plus provider, for accessible labels. Zone names are not unique — the same room name on
    /// two providers is common in this product — so a bare name would produce two controls with
    /// identical accessible names and no way to tell them apart by ear.
    pub fn qualified_label(&self) -> String {
        format!("{} ({})", self.zone_name, source_label(&self.source))
    }
}

/// Provider ids as people know them, matching the Features table's spelling.
pub fn source_label(source: &str) -> &str {
    match source {
        "roon" => "Roon",
        "lms" => "LMS",
        "hqplayer" => "HQPlayer",
        "openhome" => "OpenHome",
        "upnp" => "UPnP",
        "musicassistant" => "Music Assistant",
        "spotify" => "Spotify",
        "applemusic" => "Apple Music",
        other => other,
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ManagedZonesResponse {
    pub zones: Vec<ManagedZone>,
}

/// `POST /api/zones/visibility`.
///
/// The zone request types are defined once and used by both sides: `src/app` serialises them and
/// `src/api` deserialises them in its handlers. `src/app` is not feature-gated, so the server
/// compiles this module too. Two parallel definitions would let a field name or an enum spelling
/// drift into a 422 that the UI has no way to explain.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ZoneVisibilityRequest {
    pub zone_id: String,
    pub hidden: bool,
}

/// `POST /api/zones/order`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ZoneOrderRequest {
    pub zone_id: String,
    /// Step one place — the up/down buttons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<MoveDirection>,
    /// Take the slot this zone currently occupies — a drag-and-drop drop.
    ///
    /// A target *zone* rather than an index: the client's row indices can go stale between render
    /// and drop if a zone appears or disappears, and a stale index would land the zone somewhere
    /// the user did not point at, silently. A stale zone id simply resolves to nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_zone_id: Option<String>,
}

impl ZoneOrderRequest {
    pub fn step(zone_id: String, direction: MoveDirection) -> Self {
        Self {
            zone_id,
            direction: Some(direction),
            target_zone_id: None,
        }
    }

    pub fn drop_onto(zone_id: String, target_zone_id: String) -> Self {
        Self {
            zone_id,
            direction: None,
            target_zone_id: Some(target_zone_id),
        }
    }
}

/// Which way a step-reorder moves a zone. Re-exported by `crate::zone_list` for the server side.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MoveDirection {
    Up,
    Down,
}

/// `POST /api/zones/name`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ZoneNameRequest {
    pub zone_id: String,
    /// `None`, empty, or whitespace-only clears the override and restores the provider's name — so
    /// there is always a way back without a separate "reset" endpoint.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ZoneDsp {
    pub r#type: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ZonesResponse {
    pub zones: Vec<Zone>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NowPlaying {
    pub line1: Option<String>,
    pub line2: Option<String>,
    pub line3: Option<String>,
    pub image_url: Option<String>,
    /// Image key for cache busting (changes when track changes)
    pub image_key: Option<String>,
    pub is_playing: bool,
    pub volume: Option<f32>,
    pub volume_type: Option<String>,
    #[serde(default)]
    pub volume_min: Option<f32>,
    #[serde(default)]
    pub volume_max: Option<f32>,
    /// Volume step size (e.g., 0.5 for Roon, 2.5 for LMS)
    pub volume_step: Option<f32>,
    pub is_previous_allowed: bool,
    pub is_next_allowed: bool,
    #[serde(default)]
    pub seek_position: Option<i64>,
    #[serde(default)]
    pub length: Option<u32>,
    #[serde(default)]
    pub is_play_allowed: bool,
    #[serde(default)]
    pub is_pause_allowed: bool,
}

// =============================================================================
// Collections / queue types (#507)
//
// These mirror the `hifi_collections`/`hifi_queue`/`hifi_play_ref` MCP tool
// wire shapes exactly, because `/api/collections`, `/api/queue` and
// `/api/play_ref` (src/api/browse.rs) forward those tools' own envelope
// verbatim -- see that module's doc comment for why. Only the fields this UI
// renders are modelled; unknown fields are ignored by serde's default
// behavior, so extra envelope fields (`scope`, `observed`, `params`, ...)
// deserialize away harmlessly.
// =============================================================================

/// One item from a `hifi_collections` page: either a folder (`path` set,
/// browse only) or a playable entry (`r#ref` set, usable with `/api/play_ref`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CollectionItem {
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(rename = "ref", default)]
    pub item_ref: Option<String>,
    /// Artwork URL, used verbatim as an `<img src>`. The server sends the
    /// complete same-origin path (`/api/collections/image?ref=...`) --
    /// #573 defect 2: the UI must never prepend anything to it (an earlier
    /// draft documented this as a bare ref and the UI double-prefixed it,
    /// 404ing every image).
    #[serde(default)]
    pub image: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CollectionPage {
    #[serde(default)]
    pub items: Vec<CollectionItem>,
    #[serde(default)]
    pub next_offset: Option<u32>,
}

/// The `reason`-tagged refusal shape from `crate::mcp::envelope::Refusal`.
/// Every variant carries `detail`, which is all this UI shows.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct EnvelopeRefusal {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub detail: String,
}

/// The envelope every `hifi_*` tool result carries
/// (`crate::mcp::envelope::Envelope`), as forwarded verbatim by
/// `src/api/browse.rs`. Generic over `data`'s shape.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Envelope<T> {
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub data: Option<T>,
    #[serde(default)]
    pub refusal: Option<EnvelopeRefusal>,
}

impl<T> Envelope<T> {
    pub fn is_ok(&self) -> bool {
        matches!(self.outcome.as_str(), "ok" | "accepted")
    }

    /// A human-readable reason for a non-ok outcome, for display.
    pub fn error_detail(&self) -> String {
        self.refusal
            .as_ref()
            .map(|r| r.detail.clone())
            .filter(|detail| !detail.is_empty())
            .unwrap_or_else(|| format!("request did not succeed ({})", self.outcome))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CollectionsRequest {
    pub zone_id: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct QueueRequest {
    pub zone_id: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_zone_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlayRefRequest {
    #[serde(rename = "ref")]
    pub item_ref: String,
    pub zone_id: String,
    pub action: String,
}

/// `POST /api/collections`.
pub async fn fetch_collections(
    req: &CollectionsRequest,
) -> Result<Envelope<CollectionPage>, String> {
    post_json("/api/collections", req).await
}

/// `POST /api/queue`, for actions this UI drives without reading the queue
/// contents back (transfer). The response body is ignored beyond outcome.
pub async fn post_queue_action(req: &QueueRequest) -> Result<Envelope<serde_json::Value>, String> {
    post_json("/api/queue", req).await
}

/// `POST /api/play_ref`.
pub async fn post_play_ref(req: &PlayRefRequest) -> Result<Envelope<serde_json::Value>, String> {
    post_json("/api/play_ref", req).await
}

/// One `hifi_search` hit -- mirrors `crate::mcp::types::McpSearchResult`.
/// `item_ref` is `Some` only when the result has a durable-enough handle to
/// play later via `/api/play_ref`; a `None` ref is still worth showing (it
/// carries title/subtitle) but has no Play/Queue affordance.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(rename = "ref", default)]
    pub item_ref: Option<String>,
    /// Opaque browse-continuation ref (#566), present when this result is
    /// navigable -- same convention as `CollectionItem::path`, consumed the
    /// same way (open into browse, push a breadcrumb).
    #[serde(default)]
    pub path: Option<String>,
    /// Artwork URL (#573 defect 10) -- same contract as
    /// `CollectionItem::image`: a complete same-origin path, used verbatim
    /// as an `<img src>`.
    #[serde(default)]
    pub image: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// `POST /api/search` -- the Library page's "Everywhere" results (#550), a
/// thin mirror of the `hifi_search` MCP tool (see `src/api/browse.rs`).
pub async fn fetch_search(req: &SearchRequest) -> Result<Envelope<Vec<SearchResult>>, String> {
    post_json("/api/search", req).await
}

// =============================================================================
// LMS Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LmsConfig {
    pub configured: bool,
    pub connected: bool,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Whether CLI subscription is active (real-time events vs polling-only)
    #[serde(default)]
    pub cli_subscription_active: bool,
    /// Current poll interval in seconds (2s when CLI down, 30s when CLI up)
    #[serde(default)]
    pub poll_interval_secs: u64,
}

/// Wrapper for /lms/players response
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LmsPlayersResponse {
    pub players: Vec<LmsPlayer>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LmsPlayer {
    /// Player ID (MAC address) - API returns "playerid" field
    #[serde(alias = "playerid")]
    pub player_id: String,
    pub name: String,
    pub mode: String,
    /// Current track title - API returns "title" field
    #[serde(alias = "title")]
    pub current_title: Option<String>,
    pub artist: Option<String>,
    pub volume: i32,
}

// =============================================================================
// HQPlayer Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub web_port: Option<u16>,
    #[serde(default)]
    pub has_web_credentials: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpPipeline {
    pub status: Option<HqpPipelineStatus>,
    pub volume: Option<HqpVolume>,
    pub settings: Option<HqpSettings>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpPipelineStatus {
    pub state: Option<String>,
    pub mode: Option<String>,
    pub active_mode: Option<String>,
    pub active_filter: Option<String>,
    pub active_shaper: Option<String>,
    pub active_rate: Option<u64>,
    pub convolution: Option<bool>,
    pub invert: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpVolume {
    pub value: Option<i32>,
    pub min: Option<i32>,
    pub max: Option<i32>,
    pub is_fixed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpSettings {
    pub mode: Option<HqpSettingOptions>,
    pub samplerate: Option<HqpSettingOptions>,
    pub filter1x: Option<HqpSettingOptions>,
    #[serde(rename = "filterNx")]
    pub filter_nx: Option<HqpSettingOptions>,
    pub shaper: Option<HqpSettingOptions>,
    #[serde(rename = "shaperLabel")]
    pub shaper_label: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpSettingOptions {
    pub options: Vec<HqpOption>,
    pub selected: Option<HqpOption>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpOption {
    pub value: String,
    pub label: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpProfile {
    pub name: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    #[serde(default)]
    pub active: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpProfilesResponse {
    pub profiles: Vec<HqpProfile>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpMatrixProfile {
    pub index: u32,
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpMatrixProfilesResponse {
    pub profiles: Vec<HqpMatrixProfile>,
    pub current: Option<HqpMatrixProfile>,
    #[serde(default)]
    pub junk_filters: Vec<HqpNativeChoice>,
    pub junk_filter: Option<u32>,
    pub convolution: Option<bool>,
    pub adaptive_volume: Option<bool>,
    pub repeat: Option<u8>,
    pub random: Option<bool>,
    pub native_state: Option<HqpNativeState>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpNativeChoice {
    pub index: u32,
    pub name: String,
    pub value: i32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpNativeState {
    pub state: u8,
    pub mode: u8,
    pub filter: u32,
    pub filter1x: Option<u32>,
    pub filter_nx: Option<u32>,
    pub shaper: u32,
    pub rate: u32,
    pub volume: i32,
    pub active_mode: u8,
    pub active_rate: u32,
    pub invert: bool,
    pub convolution: bool,
    pub repeat: u8,
    pub random: bool,
    pub adaptive: bool,
    pub filter_20k: bool,
    pub matrix_profile: String,
}

// =============================================================================
// Knob Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobDevicesResponse {
    pub knobs: Vec<KnobDevice>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobDevice {
    pub knob_id: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub last_seen: Option<String>,
    pub status: Option<KnobStatus>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobStatus {
    pub battery_level: Option<i32>,
    pub battery_charging: Option<bool>,
    pub zone_id: Option<String>,
    pub ip: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobConfigResponse {
    pub config: Option<KnobConfig>,
}

/// Power mode configuration for knob timeout-based state transitions
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PowerModeConfig {
    pub enabled: bool,
    pub timeout_sec: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobConfig {
    pub name: Option<String>,
    pub rotation_charging: Option<i32>,
    pub rotation_not_charging: Option<i32>,
    // Power modes when charging
    pub art_mode_charging: Option<PowerModeConfig>,
    pub dim_charging: Option<PowerModeConfig>,
    pub sleep_charging: Option<PowerModeConfig>,
    pub deep_sleep_charging: Option<PowerModeConfig>,
    // Power modes when on battery
    pub art_mode_battery: Option<PowerModeConfig>,
    pub dim_battery: Option<PowerModeConfig>,
    pub sleep_battery: Option<PowerModeConfig>,
    pub deep_sleep_battery: Option<PowerModeConfig>,
    // Advanced settings
    pub wifi_power_save_enabled: Option<bool>,
    pub cpu_freq_scaling_enabled: Option<bool>,
    /// Poll interval when playback stopped (seconds)
    pub sleep_poll_stopped_sec: Option<u32>,
    /// Volume step override (None/0 = use zone default)
    pub volume_step_override: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FirmwareVersion {
    pub version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FetchFirmwareResponse {
    pub version: Option<String>,
    pub error: Option<String>,
}

// =============================================================================
// Client-side fetch helpers (for use in effects/resources)
// =============================================================================

/// Fetch JSON from a URL (client-side only)
#[cfg(target_arch = "wasm32")]
pub async fn fetch_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Request, RequestInit, Response};

    let window = web_sys::window().ok_or("No window")?;
    let opts = RequestInit::new();
    opts.set_method("GET");

    // Ingress/subpath deployments (#581): every fetch flows through this
    // helper and its POST siblings below, so this is the one place request
    // URLs pick up the runtime base path. Identity in direct mode.
    let url = crate::app::base_path::href(url);
    let request = Request::new_with_str_and_init(&url, &opts).map_err(|e| format!("{:?}", e))?;

    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("{:?}", e))?;

    let resp: Response = resp_value.dyn_into().map_err(|_| "Not a Response")?;

    if !resp.ok() {
        return Err(response_error(resp).await);
    }

    let json = JsFuture::from(resp.json().map_err(|e| format!("{:?}", e))?)
        .await
        .map_err(|e| format!("{:?}", e))?;

    serde_wasm_bindgen::from_value(json).map_err(|e| format!("{:?}", e))
}

/// SSR stub - returns error (should not be called during SSR)
#[cfg(not(target_arch = "wasm32"))]
pub async fn fetch_json<T: for<'de> Deserialize<'de>>(_url: &str) -> Result<T, String> {
    Err("fetch_json is only available in browser".to_string())
}

/// POST JSON to a URL (client-side only)
#[cfg(target_arch = "wasm32")]
pub async fn post_json<T: Serialize, R: for<'de> Deserialize<'de>>(
    url: &str,
    body: &T,
) -> Result<R, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Headers, Request, RequestInit, Response};

    let window = web_sys::window().ok_or("No window")?;

    let headers = Headers::new().map_err(|e| format!("{:?}", e))?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| format!("{:?}", e))?;
    // Attach the double-submit CSRF token from a completed bootstrap (#570).
    // The server's controller-auth middleware requires this header on every
    // non-GET/HEAD request once a session cookie exists -- see
    // `csrf_matches` in `src/api/controller_auth.rs`. Before bootstrap (or
    // when controller auth is off entirely) there is no token to attach and
    // the header is simply omitted.
    if let Some(token) = crate::app::controller_auth::current_csrf_token() {
        headers
            .set("x-uhc-csrf-token", &token)
            .map_err(|e| format!("{:?}", e))?;
    }

    let body_str = serde_json::to_string(body).map_err(|e| e.to_string())?;

    let opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_headers(&headers);
    opts.set_body(&wasm_bindgen::JsValue::from_str(&body_str));

    // See the base-path comment in `fetch_json` (#581).
    let url = crate::app::base_path::href(url);
    let request = Request::new_with_str_and_init(&url, &opts).map_err(|e| format!("{:?}", e))?;

    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("{:?}", e))?;

    let resp: Response = resp_value.dyn_into().map_err(|_| "Not a Response")?;

    if !resp.ok() {
        return Err(response_error(resp).await);
    }

    let json = JsFuture::from(resp.json().map_err(|e| format!("{:?}", e))?)
        .await
        .map_err(|e| format!("{:?}", e))?;

    serde_wasm_bindgen::from_value(json).map_err(|e| format!("{:?}", e))
}

/// SSR stub - returns error (should not be called during SSR)
#[cfg(not(target_arch = "wasm32"))]
pub async fn post_json<T: Serialize, R: for<'de> Deserialize<'de>>(
    _url: &str,
    _body: &T,
) -> Result<R, String> {
    Err("post_json is only available in browser".to_string())
}

/// POST JSON without expecting response body
#[cfg(target_arch = "wasm32")]
pub async fn post_json_no_response<T: Serialize>(url: &str, body: &T) -> Result<(), String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Headers, Request, RequestInit, Response};

    let window = web_sys::window().ok_or("No window")?;

    let headers = Headers::new().map_err(|e| format!("{:?}", e))?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| format!("{:?}", e))?;
    // See the matching comment in `post_json` above (#570).
    if let Some(token) = crate::app::controller_auth::current_csrf_token() {
        headers
            .set("x-uhc-csrf-token", &token)
            .map_err(|e| format!("{:?}", e))?;
    }

    let body_str = serde_json::to_string(body).map_err(|e| e.to_string())?;

    let opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_headers(&headers);
    opts.set_body(&wasm_bindgen::JsValue::from_str(&body_str));

    // See the base-path comment in `fetch_json` (#581).
    let url = crate::app::base_path::href(url);
    let request = Request::new_with_str_and_init(&url, &opts).map_err(|e| format!("{:?}", e))?;

    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("{:?}", e))?;

    let resp: Response = resp_value.dyn_into().map_err(|_| "Not a Response")?;
    if !resp.ok() {
        return Err(response_error(resp).await);
    }

    Ok(())
}

#[cfg(target_arch = "wasm32")]
async fn response_error(resp: web_sys::Response) -> String {
    use wasm_bindgen_futures::JsFuture;

    let status = resp.status();
    let body = match resp.text() {
        Ok(text) => JsFuture::from(text)
            .await
            .ok()
            .and_then(|value| value.as_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| {
            json.get("error")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            if body.is_empty() {
                "Request failed".to_string()
            } else {
                body
            }
        });
    // Route every fetch helper's controller-auth 401 into the bootstrap
    // prompt instead of leaving the caller to render this raw string
    // (#570). This is the single interception point: `fetch_json`,
    // `post_json`, and `post_json_no_response` all funnel their non-ok
    // responses through here.
    if status == 401 && detail == CONTROLLER_UNAUTHORIZED_MESSAGE {
        crate::app::controller_auth::open_bootstrap_prompt();
    }
    format!("HTTP {status}: {detail}")
}

/// SSR stub - returns error (should not be called during SSR)
#[cfg(not(target_arch = "wasm32"))]
pub async fn post_json_no_response<T: Serialize>(_url: &str, _body: &T) -> Result<(), String> {
    Err("post_json_no_response is only available in browser".to_string())
}

#[cfg(test)]
mod mqtt_status_tests {
    use super::MqttStatusResponse;

    fn retrying(last_error: &str) -> MqttStatusResponse {
        MqttStatusResponse {
            configured: true,
            enabled: true,
            // The #607 shape: the task is alive, the broker is not there.
            running: true,
            connection: "connecting".to_string(),
            last_error: Some(last_error.to_string()),
            host: Some("core-mosquitto".to_string()),
            port: Some(1883),
            tls: Some(false),
            ..MqttStatusResponse::default()
        }
    }

    #[test]
    fn running_alone_is_never_read_as_connected() {
        let status = retrying("I/O: failed to lookup address information");
        assert!(status.running);
        assert!(!status.is_connected());
        assert!(status.is_connecting());
    }

    #[test]
    fn an_unknown_host_is_explained_as_a_typo_to_check() {
        let status = retrying(
            "I/O: failed to lookup address information: nodename nor servname provided, or not known",
        );
        assert_eq!(
            status.connection_problem().as_deref(),
            Some("That address could not be found. Check the broker's name for a typo.")
        );
    }

    #[test]
    fn rejected_credentials_are_explained_as_credentials() {
        let status = retrying("Connection refused, return code `NotAuthorized`");
        // Deliberately not the "nothing answered there" branch: the raw
        // text contains "refused" too, and telling the user to check the
        // port when the password is wrong sends them the wrong way.
        assert_eq!(
            status.connection_problem().as_deref(),
            Some("The broker turned us away. Check the username and password.")
        );
    }

    #[test]
    fn a_dead_port_is_explained_as_a_port_or_a_stopped_broker() {
        let status = retrying("I/O: Connection refused (os error 61)");
        assert_eq!(
            status.connection_problem().as_deref(),
            Some("Nothing answered there. Check the port, and that the broker is switched on.")
        );
    }

    #[test]
    fn an_unrecognised_failure_is_passed_through_rather_than_swallowed() {
        let status = retrying("Mqtt state: Invalid state for a given operation");
        assert_eq!(
            status.connection_problem().as_deref(),
            Some("Mqtt state: Invalid state for a given operation")
        );
    }

    #[test]
    fn a_connected_publisher_reports_no_problem() {
        let mut status = retrying("I/O: Connection refused (os error 61)");
        status.connection = "connected".to_string();
        status.last_error = None;
        assert!(status.is_connected());
        assert_eq!(status.connection_problem(), None);
    }

    /// A server that predates #607 sends no `connection` field. Absent
    /// evidence of a connection must not read as one.
    #[test]
    fn a_status_without_the_field_is_not_treated_as_connected() {
        let status: MqttStatusResponse = serde_json::from_str(
            r#"{"configured":true,"enabled":true,"running":true,"host":"a","port":1883,
                "tls":false,"base_topic":"unified-hifi","discovery_prefix":"homeassistant",
                "has_username":false,"has_password":false}"#,
        )
        .expect("deserialize");
        assert!(status.running);
        assert!(!status.is_connected());
        assert_eq!(status.connection_problem(), None);
    }

    #[test]
    fn the_broker_address_names_the_scheme_and_port() {
        let mut status = retrying("boom");
        assert_eq!(
            status.broker_address().as_deref(),
            Some("mqtt://core-mosquitto:1883")
        );
        status.tls = Some(true);
        status.port = Some(8883);
        assert_eq!(
            status.broker_address().as_deref(),
            Some("mqtts://core-mosquitto:8883")
        );
    }
}
