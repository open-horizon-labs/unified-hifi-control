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
    pub running: bool,
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

    #[test]
    fn status_response_never_serializes_the_password() {
        let response = MqttStatusResponse {
            configured: true,
            enabled: true,
            running: true,
            host: Some("broker.local".to_string()),
            port: Some(1883),
            tls: Some(false),
            base_topic: Some("unified-hifi".to_string()),
            discovery_prefix: Some("homeassistant".to_string()),
            has_username: true,
            has_password: true,
            source: Some("user".to_string()),
        };
        let json = serde_json::to_string(&response).expect("serialize");
        // `has_password` (a bool) is expected; the literal secret is not -
        // `MqttStatusResponse` has no field that could carry it, and this
        // pins that down structurally rather than trusting the field list.
        assert!(!json.contains("\"password\":"));
    }
}
