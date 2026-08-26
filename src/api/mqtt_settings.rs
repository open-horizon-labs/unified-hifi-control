//! HTTP settings surface for the optional MQTT/Home Assistant publisher
//! (#508). Broker connection details (including the password) live in the
//! encrypted [`crate::api::credentials::MqttCredentialStore`], the same
//! pattern Spotify and Music Assistant use; the on/off switch lives in
//! `AdapterSettings.mqtt` (`app-settings.json`) alongside every other
//! adapter toggle.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::credentials::{MqttConfigSource, MqttCredentialRecord, MqttCredentialStore};
use super::AppState;
use crate::mqtt::{DEFAULT_BASE_TOPIC, DEFAULT_DISCOVERY_PREFIX, DEFAULT_PORT, DEFAULT_TLS_PORT};

/// Full replacement of the broker connection settings, except the password:
/// omit or submit blank to retain whatever is already stored (the
/// `MusicAssistantConfigureRequest` convention in `provider_auth.rs`).
#[derive(Debug, Deserialize)]
pub struct MqttConfigureRequest {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub base_topic: Option<String>,
    #[serde(default)]
    pub discovery_prefix: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct MqttStatusResponse {
    pub configured: bool,
    pub enabled: bool,
    /// The publisher's background task exists. NOT connectivity: the task
    /// stays alive while `rumqttc` retries a broker that never answers
    /// (#607). Use `connection` for "are we actually publishing".
    pub running: bool,
    /// `"disconnected"` (no task), `"connecting"` (task running, broker has
    /// not accepted us - or dropped us and we are retrying), or
    /// `"connected"` (#607).
    pub connection: String,
    /// Why the last connection attempt failed, if one has. Present while
    /// retrying, absent once connected. Carried through to Settings because
    /// "wrong password" and "wrong host" are entirely different fixes.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    /// from the Supervisor, which Settings renders as managed rather than as
    /// something the user should fill in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl From<crate::mqtt::MqttStatus> for MqttStatusResponse {
    fn from(status: crate::mqtt::MqttStatus) -> Self {
        Self {
            configured: status.configured,
            enabled: status.enabled,
            running: status.running,
            connection: status.connection.as_str().to_string(),
            last_error: status.last_error,
            host: status.host,
            port: status.port,
            tls: status.tls,
            base_topic: status.base_topic,
            discovery_prefix: status.discovery_prefix,
            has_username: status.has_username,
            has_password: status.has_password,
            source: status.source.map(|source| {
                match source {
                    MqttConfigSource::User => "user",
                    MqttConfigSource::Environment => "environment",
                }
                .to_string()
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
}

/// GET /api/mqtt/status - configuration presence and liveness, no secrets.
pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    Json(MqttStatusResponse::from(state.mqtt.status().await))
}

/// POST /api/mqtt/configure - persist broker settings and adopt them live.
pub async fn configure(
    State(state): State<AppState>,
    Json(request): Json<MqttConfigureRequest>,
) -> Result<Json<MqttStatusResponse>, (StatusCode, Json<ErrorBody>)> {
    if request.host.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "MQTT broker host is required",
        ));
    }

    let store = MqttCredentialStore::from_env().map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("MQTT credential store unavailable: {error}"),
        )
    })?;

    let previous = store.load().unwrap_or(None);
    let tls = request.tls;
    let record = MqttCredentialRecord {
        host: request.host.trim().to_string(),
        port: request
            .port
            .unwrap_or(if tls { DEFAULT_TLS_PORT } else { DEFAULT_PORT }),
        tls,
        username: request.username.filter(|value| !value.is_empty()),
        password: request
            .password
            .filter(|value| !value.is_empty())
            .or_else(|| previous.as_ref().and_then(|record| record.password.clone())),
        base_topic: request
            .base_topic
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_TOPIC.to_string()),
        discovery_prefix: request
            .discovery_prefix
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_DISCOVERY_PREFIX.to_string()),
        // Saving through this endpoint is what makes the configuration the
        // user's own: from here on the startup environment bootstrap (#605)
        // leaves it alone, even under the Home Assistant add-on.
        source: MqttConfigSource::User,
    };

    store.save(&record).map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to save MQTT settings: {error}"),
        )
    })?;

    state.mqtt.configure(record).await;
    Ok(Json(MqttStatusResponse::from(state.mqtt.status().await)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::{MqttConnectionState, MqttStatus};

    fn sample_response() -> MqttStatusResponse {
        MqttStatusResponse {
            configured: true,
            enabled: true,
            running: true,
            connection: "connected".to_string(),
            last_error: None,
            host: Some("broker.local".to_string()),
            port: Some(1883),
            tls: Some(false),
            base_topic: Some("unified-hifi".to_string()),
            discovery_prefix: Some("homeassistant".to_string()),
            has_username: true,
            has_password: true,
            source: Some("user".to_string()),
        }
    }

    #[test]
    fn status_response_never_serializes_the_password() {
        let json = serde_json::to_string(&sample_response()).expect("serialize");
        // `has_password` (a bool) is expected; the literal secret is not -
        // `MqttStatusResponse` has no field that could carry it, and this
        // pins that down structurally rather than trusting the field list.
        assert!(!json.contains("\"password\":"));
    }

    /// The regression #607 is about: `running` alone said "true" for a
    /// broker whose hostname does not resolve. `connection` and
    /// `last_error` have to carry the truth alongside it, and `running`
    /// has to keep its old meaning rather than being redefined.
    #[test]
    fn a_retrying_publisher_serializes_as_running_but_not_connected() {
        let status = MqttStatus {
            configured: true,
            enabled: true,
            running: true,
            connection: MqttConnectionState::Connecting,
            last_error: Some(
                "I/O: failed to lookup address information: nodename nor servname provided, \
                 or not known"
                    .to_string(),
            ),
            host: Some("core-mosquitto".to_string()),
            port: Some(1883),
            tls: Some(false),
            base_topic: Some("unified-hifi".to_string()),
            discovery_prefix: Some("homeassistant".to_string()),
            has_username: false,
            has_password: false,
            source: Some(MqttConfigSource::Environment),
        };
        let response = MqttStatusResponse::from(status);
        assert!(response.running, "the task is alive - that much was true");
        assert_eq!(response.connection, "connecting");
        assert!(response
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("lookup address information")));
    }

    /// A connected publisher reports no error, so Settings has nothing
    /// stale to display next to a working broker.
    #[test]
    fn connected_status_carries_no_error_and_omits_the_field() {
        let json = serde_json::to_value(sample_response()).expect("serialize");
        assert_eq!(json["connection"], "connected");
        assert!(
            json.get("last_error").is_none(),
            "an absent error must not serialize as null: {json}"
        );
    }

    #[test]
    fn an_idle_publisher_reports_disconnected() {
        let response = MqttStatusResponse::from(MqttStatus {
            configured: false,
            enabled: false,
            running: false,
            connection: MqttConnectionState::default(),
            last_error: None,
            host: None,
            port: None,
            tls: None,
            base_topic: None,
            discovery_prefix: None,
            has_username: false,
            has_password: false,
            source: None,
        });
        assert_eq!(response.connection, "disconnected");
    }
}
