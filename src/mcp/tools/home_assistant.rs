//! Narrow Home Assistant entity reads and light/switch control.
//!
//! Add-on credentials are injected by Home Assistant and are never accepted as
//! an MCP argument, persisted, or returned. Standalone credentials come from
//! runtime configuration. Reads return an allowlist of ordinary state attributes so
//! entity metadata cannot accidentally expose integration credentials.

use crate::mcp::envelope::{Envelope, Refusal};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

const SUPERVISOR_STATES_URL: &str = "http://supervisor/core/api/states";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 100;
const MAX_ENTITY_IDS: usize = 64;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct HaControlEntityTool {
    /// Exact entity ID returned by `ha_read_states`; only `light` and `switch` entities are accepted.
    pub entity_id: String,
    /// The bounded service action to apply to that one entity.
    pub action: HaEntityAction,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HaEntityAction {
    TurnOn,
    TurnOff,
}

impl HaEntityAction {
    fn service(self) -> &'static str {
        match self {
            Self::TurnOn => "turn_on",
            Self::TurnOff => "turn_off",
        }
    }

    fn expected_state(self) -> &'static str {
        match self {
            Self::TurnOn => "on",
            Self::TurnOff => "off",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct HaState {
    entity_id: String,
    state: String,
    #[serde(default)]
    attributes: Value,
    #[serde(default)]
    last_changed: Option<String>,
    #[serde(default)]
    last_updated: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SafeHaState {
    entity_id: String,
    friendly_name: String,
    state: String,
    unit: Option<String>,
    device_class: Option<String>,
    state_class: Option<String>,
    current_temperature: Option<Value>,
    temperature: Option<Value>,
    humidity: Option<Value>,
    last_changed: Option<String>,
    last_updated: Option<String>,
}

impl From<HaState> for SafeHaState {
    fn from(row: HaState) -> Self {
        let attrs = row.attributes.as_object();
        let attr_text = |key: &str| {
            attrs
                .and_then(|map| map.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let attr_value = |key: &str| attrs.and_then(|map| map.get(key)).cloned();
        let friendly_name = attr_text("friendly_name").unwrap_or_else(|| row.entity_id.clone());
        Self {
            entity_id: row.entity_id,
            friendly_name,
            state: row.state,
            unit: attr_text("unit_of_measurement"),
            device_class: attr_text("device_class"),
            state_class: attr_text("state_class"),
            current_temperature: attr_value("current_temperature"),
            temperature: attr_value("temperature"),
            humidity: attr_value("humidity"),
            last_changed: row.last_changed,
            last_updated: row.last_updated,
        }
    }
}

#[derive(Debug, Serialize)]
struct HaReadResult {
    status: &'static str,
    observed_at: String,
    matched: usize,
    returned: usize,
    truncated: bool,
    missing_entity_ids: Vec<String>,
    states: Vec<SafeHaState>,
}

#[derive(Debug, Serialize)]
struct HaControlResult {
    status: &'static str,
    entity_id: String,
    action: &'static str,
    accepted: bool,
    confirmed: bool,
    observed_at: String,
    observed_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HaControlFailure {
    NotSent(String),
    NotFound(String),
    Rejected(String),
    Uncertain(String),
}

#[derive(Debug, Serialize)]
struct HaControlFailureResult {
    status: &'static str,
    entity_id: String,
    action: &'static str,
    dispatch_attempted: bool,
    accepted: Option<bool>,
    confirmed: bool,
    retry_guidance: &'static str,
}

impl HaControlFailure {
    fn detail(&self) -> &str {
        match self {
            Self::NotSent(detail)
            | Self::NotFound(detail)
            | Self::Rejected(detail)
            | Self::Uncertain(detail) => detail,
        }
    }

    fn payload(&self, args: &HASSControlEntityTool) -> HaControlFailureResult {
        match self {
            Self::NotSent(_) => HaControlFailureResult {
                status: "not_sent",
                entity_id: args.entity_id.clone(),
                action: args.action.service(),
                dispatch_attempted: false,
                accepted: Some(false),
                confirmed: false,
                retry_guidance:
                    "Resolve the read/configuration error before deciding whether to try again.",
            },
            Self::NotFound(_) => HaControlFailureResult {
                status: "not_found",
                entity_id: args.entity_id.clone(),
                action: args.action.service(),
                dispatch_attempted: false,
                accepted: Some(false),
                confirmed: false,
                retry_guidance:
                    "Read current Home Assistant states and select an existing exact entity ID.",
            },
            Self::Rejected(_) => HaControlFailureResult {
                status: "rejected",
                entity_id: args.entity_id.clone(),
                action: args.action.service(),
                dispatch_attempted: true,
                accepted: Some(false),
                confirmed: false,
                retry_guidance:
                    "Inspect the rejection; do not repeat without correcting its cause.",
            },
            Self::Uncertain(_) => HaControlFailureResult {
                status: "uncertain",
                entity_id: args.entity_id.clone(),
                action: args.action.service(),
                dispatch_attempted: true,
                accepted: None,
                confirmed: false,
                retry_guidance: "Read the entity state before considering another service call.",
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct HaUnavailableResult {
    status: &'static str,
    reason: &'static str,
}

#[mcp_tool(
    name = "ha_read_states",
    description = "Read current Home Assistant entity states through the Supervisor API when running as an add-on, or through explicitly configured UHC_HA_API_URL and UHC_HA_API_TOKEN when standalone. Query by exact entity ID(s), domain, or name; at least one filter is required. Returns stable entity IDs, friendly names, state, selected safe attributes, and source timestamps. Results are capped and report truncation. This is read-only and does not call services.",
    read_only_hint = true
)]
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct HASSReadStatesTool {
    /// One exact Home Assistant entity ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    /// Exact entity IDs (maximum 64).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_ids: Option<Vec<String>>,
    /// Match one entity domain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Case-insensitive substring of the friendly name or entity ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Maximum matching states returned (1–100; defaults to 50).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u8>,
}

#[mcp_tool(
    name = "ha_control_entity",
    description = "Turn one exact Home Assistant light or switch entity on or off, using the Supervisor API when running as an add-on or explicitly configured UHC_HA_API_URL and UHC_HA_API_TOKEN when standalone. Use an entity_id previously returned by ha_read_states. The only actions are turn_on and turn_off; no other domains, services, targets, scripts, or URLs are accepted. A successful service response is reported as accepted, and a separate state readback reports whether the requested state was observed.",
    destructive_hint = true
)]
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct HASSControlEntityTool {
    /// Exact Home Assistant entity ID from ha_read_states (light.* or switch.* only).
    pub entity_id: String,
    /// Only turn_on or turn_off.
    pub action: HaEntityAction,
}

/// Resolve the state endpoint and credential from runtime configuration.
/// Add-ons use the Supervisor proxy; standalone installs require both
/// UHC_HA_API_URL and UHC_HA_API_TOKEN. The URL is never an MCP argument.
fn ha_api_credentials() -> Option<(String, String)> {
    use crate::mqtt::consumer::{CORE_TOKEN_ENV, CORE_URL_ENV, SUPERVISOR_TOKEN_ENV};
    credentials_for(
        std::env::var("UHC_ADDON").ok().as_deref() == Some("1"),
        std::env::var(SUPERVISOR_TOKEN_ENV).ok(),
        std::env::var(CORE_URL_ENV).ok(),
        std::env::var(CORE_TOKEN_ENV).ok(),
    )
}

fn credentials_for(
    is_addon: bool,
    supervisor_token: Option<String>,
    configured_url: Option<String>,
    configured_token: Option<String>,
) -> Option<(String, String)> {
    if is_addon {
        let token = supervisor_token.filter(|token| !token.trim().is_empty())?;
        return Some((SUPERVISOR_STATES_URL.to_string(), token));
    }
    let url = states_endpoint(configured_url.as_deref()?)?;
    let token = configured_token.filter(|token| !token.trim().is_empty())?;
    Some((url, token))
}

fn states_endpoint(configured_url: &str) -> Option<String> {
    let mut url = url::Url::parse(configured_url).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    if !matches!(path, "" | "/api" | "/api/config") {
        return None;
    }
    url.set_path("/api/states");
    Some(url.to_string().trim_end_matches('/').to_string())
}

fn validate_read(args: &HASSReadStatesTool) -> Result<usize, (&'static str, String)> {
    if args.entity_id.is_none()
        && args.entity_ids.is_none()
        && args.domain.is_none()
        && args.name.is_none()
    {
        return Err((
            "entity_id",
            "provide an entity_id, entity_ids, domain, or name filter".into(),
        ));
    }
    if args.entity_id.is_some() && args.entity_ids.is_some() {
        return Err(("entity_ids", "use entity_id or entity_ids, not both".into()));
    }
    if (args.entity_id.is_some() || args.entity_ids.is_some())
        && (args.domain.is_some() || args.name.is_some())
    {
        return Err((
            "entity_id",
            "use exact entity IDs by themselves, or use domain/name discovery filters".into(),
        ));
    }
    if let Some(id) = args.entity_id.as_deref() {
        if !valid_entity_id(id) {
            return Err((
                "entity_id",
                "use a Home Assistant entity ID such as sensor.garage_temperature".into(),
            ));
        }
    }
    if let Some(ids) = args.entity_ids.as_ref() {
        if ids.is_empty() || ids.len() > MAX_ENTITY_IDS || ids.iter().any(|id| !valid_entity_id(id))
        {
            return Err((
                "entity_ids",
                "provide 1–64 valid Home Assistant entity IDs".into(),
            ));
        }
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err((
                "entity_ids",
                "entity_ids must not contain duplicates".into(),
            ));
        }
    }
    if let Some(domain) = args.domain.as_deref() {
        if domain.is_empty()
            || domain.len() > 64
            || !domain
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(("domain", "use one lowercase Home Assistant domain".into()));
        }
    }
    if let Some(name) = args.name.as_deref() {
        if name.trim().is_empty() || name.len() > 200 {
            return Err((
                "name",
                "use a non-empty name filter of at most 200 characters".into(),
            ));
        }
    }
    let exact_count = args.entity_ids.as_ref().map(Vec::len).unwrap_or(0);
    let default_limit = if args.limit.is_none() && exact_count > DEFAULT_LIMIT {
        exact_count
    } else {
        DEFAULT_LIMIT
    };
    let limit = args.limit.map(usize::from).unwrap_or(default_limit);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(("limit", format!("use an integer from 1 to {MAX_LIMIT}")));
    }
    Ok(limit)
}

fn valid_entity_id(id: &str) -> bool {
    if id.len() > 255 {
        return false;
    }
    let Some((domain, object)) = id.split_once('.') else {
        return false;
    };
    !domain.is_empty()
        && !object.is_empty()
        && domain
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && object
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn observed_at() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Fetch and normalize state rows against the fixed Supervisor endpoint.
/// `url` is only parameterized inside this module for loopback tests; tools
/// never accept an endpoint from their callers.
async fn fetch_rows_at(
    client: &reqwest::Client,
    token: &str,
    url: &str,
) -> Result<Vec<HaState>, String> {
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| "Home Assistant state API could not be reached".to_string())?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED
        || response.status() == reqwest::StatusCode::FORBIDDEN
    {
        return Err(
            "Home Assistant refused the configured API credential; check its access and endpoint"
                .into(),
        );
    }
    if !response.status().is_success() {
        return Err(format!(
            "Home Assistant state API returned HTTP {}",
            response.status().as_u16()
        ));
    }
    let value = bounded_json(response, "Home Assistant state").await?;
    let rows = value
        .as_array()
        .ok_or_else(|| "Home Assistant returned an invalid state list".to_string())?;
    rows.iter()
        .cloned()
        .map(|row| {
            serde_json::from_value(row)
                .map_err(|_| "Home Assistant returned an invalid entity state".to_string())
        })
        .collect()
}

async fn bounded_json(response: reqwest::Response, label: &str) -> Result<Value, String> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err(format!("{label} response exceeded the 8 MiB safety limit"));
    }
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| format!("{label} response was interrupted"))?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(format!("{label} response exceeded the 8 MiB safety limit"));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| format!("{label} returned invalid JSON"))
}

async fn read_states_at(
    client: &reqwest::Client,
    token: &str,
    url: &str,
    args: &HASSReadStatesTool,
    limit: usize,
) -> Result<HaReadResult, String> {
    let requested: Option<HashSet<&str>> = args
        .entity_id
        .as_deref()
        .map(|one| [one].into_iter().collect())
        .or_else(|| {
            args.entity_ids
                .as_ref()
                .map(|many| many.iter().map(String::as_str).collect())
        });
    let rows = if let Some(entity_id) = args.entity_id.as_deref() {
        fetch_one_at(client, token, url, entity_id)
            .await?
            .into_iter()
            .collect()
    } else if let Some(entity_ids) = args.entity_ids.as_ref() {
        use futures::StreamExt;
        let mut results = futures::stream::iter(entity_ids.iter().cloned().map(|entity_id| {
            let client = client.clone();
            let token = token.to_owned();
            let url = url.to_owned();
            async move { fetch_one_at(&client, &token, &url, &entity_id).await }
        }))
        .buffer_unordered(8);
        let mut rows = Vec::new();
        while let Some(result) = results.next().await {
            if let Some(row) = result? {
                rows.push(row);
            }
        }
        rows
    } else {
        fetch_rows_at(client, token, url).await?
    };
    let domain = args.domain.as_deref();
    let name = args.name.as_deref().map(str::to_lowercase);
    let mut matches: Vec<HaState> = rows
        .into_iter()
        .filter(|row| {
            requested
                .as_ref()
                .is_none_or(|ids| ids.contains(row.entity_id.as_str()))
        })
        .filter(|row| {
            domain.is_none_or(|domain| {
                row.entity_id
                    .split_once('.')
                    .is_some_and(|(d, _)| d == domain)
            })
        })
        .filter(|row| {
            name.as_ref().is_none_or(|needle| {
                let friendly = row
                    .attributes
                    .get("friendly_name")
                    .and_then(Value::as_str)
                    .unwrap_or(&row.entity_id);
                friendly.to_lowercase().contains(needle)
                    || row.entity_id.to_lowercase().contains(needle)
            })
        })
        .collect();
    matches.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
    let matched = matches.len();
    let truncated = matched > limit;
    let found: HashSet<&str> = matches.iter().map(|row| row.entity_id.as_str()).collect();
    let mut missing_entity_ids: Vec<String> = requested
        .as_ref()
        .into_iter()
        .flat_map(|ids| ids.iter())
        .filter(|id| !found.contains(**id))
        .map(|id| (*id).to_string())
        .collect();
    missing_entity_ids.sort();
    let states: Vec<SafeHaState> = matches.into_iter().take(limit).map(Into::into).collect();
    let status = if matched == 0 {
        "not_found"
    } else if !missing_entity_ids.is_empty() {
        "partial"
    } else {
        "ok"
    };
    Ok(HaReadResult {
        status,
        observed_at: observed_at(),
        matched,
        returned: states.len(),
        truncated,
        missing_entity_ids,
        states,
    })
}

pub async fn handle_read_states(args: HASSReadStatesTool) -> Result<CallToolResult, CallToolError> {
    let env = Envelope::read("ha_read_states", "read_states")
        .param_opt("entity_id", args.entity_id.as_deref())
        .param_opt(
            "entity_ids",
            args.entity_ids
                .as_ref()
                .map(|ids| serde_json::to_value(ids).unwrap_or(Value::Null)),
        )
        .param_opt("domain", args.domain.as_deref())
        .param_opt("name", args.name.as_deref())
        .param_opt("limit", args.limit.map(u64::from));
    let limit = match validate_read(&args) {
        Ok(limit) => limit,
        Err((parameter, detail)) => {
            return env.refused(
                &detail,
                Refusal::InvalidParameter {
                    parameter,
                    accepted: vec![
                        "an exact entity ID, a domain/name filter, or a bounded list".into(),
                    ],
                    detail: detail.clone(),
                },
            );
        }
    };
    let Some((states_url, token)) = ha_api_credentials() else {
        let detail = "Home Assistant state reads require the add-on homeassistant_api permission or standalone UHC_HA_API_URL and UHC_HA_API_TOKEN configuration.";
        return env
            .data(&HaUnavailableResult {
                status: "unavailable",
                reason: detail,
            })
            .failed(detail);
    };
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(CallToolError::new)?;
    match read_states_at(&client, &token, &states_url, &args, limit).await {
        Ok(result) => Ok(env.json_result(&result)),
        Err(detail) => env.failed(detail),
    }
}

async fn fetch_one_at(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    entity_id: &str,
) -> Result<Option<HaState>, String> {
    let url = format!("{base_url}/{}", entity_id);
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| "Home Assistant state API could not be reached".to_string())?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if response.status() == reqwest::StatusCode::UNAUTHORIZED
        || response.status() == reqwest::StatusCode::FORBIDDEN
    {
        return Err(
            "Home Assistant refused the configured API credential; check its access and endpoint"
                .into(),
        );
    }
    if !response.status().is_success() {
        return Err(format!(
            "Home Assistant state API returned HTTP {}",
            response.status().as_u16()
        ));
    }
    let value = bounded_json(response, "Home Assistant entity").await?;
    let row: HaState = serde_json::from_value(value)
        .map_err(|_| "Home Assistant returned invalid entity state".to_string())?;
    if row.entity_id != entity_id {
        return Err("Home Assistant returned a different entity than requested".into());
    }
    Ok(Some(row))
}

async fn control_entity_at(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    args: &HASSControlEntityTool,
) -> Result<HaControlResult, HaControlFailure> {
    if !valid_entity_id(&args.entity_id)
        || !matches!(
            args.entity_id.split_once('.').map(|(d, _)| d),
            Some("light" | "switch")
        )
    {
        return Err(HaControlFailure::NotSent(
            "Only one exact light.* or switch.* entity ID is accepted".into(),
        ));
    }
    match fetch_one_at(client, token, base_url, &args.entity_id)
        .await
        .map_err(HaControlFailure::NotSent)?
    {
        Some(_) => {}
        None => {
            return Err(HaControlFailure::NotFound(format!(
                "Home Assistant entity {} was not found; no action was sent",
                args.entity_id
            )))
        }
    }
    let Some((domain, _)) = args.entity_id.split_once('.') else {
        return Err(HaControlFailure::NotSent(
            "Entity ID must contain a domain separator".into(),
        ));
    };
    let action = args.action.service();
    let service_root = base_url.strip_suffix("/api/states").unwrap_or(base_url);
    let service_url = format!("{service_root}/api/services/{domain}/{action}");
    let response = client.post(service_url).bearer_auth(token)
        .json(&serde_json::json!({"entity_id": args.entity_id}))
        .send().await.map_err(|_| HaControlFailure::Uncertain("Home Assistant service call outcome is uncertain; read the entity state before repeating".into()))?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = format!(
            "Home Assistant service call returned HTTP {}; inspect current state before retrying",
            status.as_u16()
        );
        return Err(if status.is_client_error() {
            HaControlFailure::Rejected(detail)
        } else {
            HaControlFailure::Uncertain(detail)
        });
    }
    let observed = fetch_one_at(client, token, base_url, &args.entity_id)
        .await
        .ok()
        .flatten();
    let observed_state = observed.as_ref().map(|row| row.state.clone());
    let confirmed = observed_state.as_deref() == Some(args.action.expected_state());
    Ok(HaControlResult {
        status: if confirmed {
            "confirmed"
        } else {
            "accepted_unconfirmed"
        },
        entity_id: args.entity_id.clone(),
        action,
        accepted: true,
        confirmed,
        observed_at: observed_at(),
        observed_state,
    })
}

pub async fn handle_control_entity(
    args: HASSControlEntityTool,
) -> Result<CallToolResult, CallToolError> {
    let env = Envelope::write("ha_control_entity", "control_entity")
        .param("entity_id", args.entity_id.as_str())
        .param("action", args.action.service());
    if !valid_entity_id(&args.entity_id)
        || !matches!(
            args.entity_id.split_once('.').map(|(d, _)| d),
            Some("light" | "switch")
        )
    {
        let detail = "Only one exact light.* or switch.* entity ID is accepted";
        return env.refused(
            detail,
            Refusal::InvalidParameter {
                parameter: "entity_id",
                accepted: vec![
                    "an exact existing light.* or switch.* entity ID returned by ha_read_states"
                        .into(),
                ],
                detail: detail.into(),
            },
        );
    }
    let Some((states_url, token)) = ha_api_credentials() else {
        let detail = "Home Assistant control requires the add-on homeassistant_api permission or standalone UHC_HA_API_URL and UHC_HA_API_TOKEN configuration.";
        return env
            .data(&HaUnavailableResult {
                status: "unavailable",
                reason: detail,
            })
            .failed(detail);
    };
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(CallToolError::new)?;
    match control_entity_at(&client, &token, &states_url, &args).await {
        Ok(result) => Ok(env.json_result(&result)),
        Err(failure @ HaControlFailure::NotFound(_)) => env.refused(
            failure.detail(),
            Refusal::UnknownTarget {
                parameter: "entity_id",
                discover_with: "ha_read_states",
                detail: failure.detail().to_string(),
            },
        ),
        Err(failure) => env.data(&failure.payload(&args)).failed(failure.detail()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    #[test]
    fn runtime_credentials_use_supervisor_or_explicit_standalone_configuration() {
        assert_eq!(
            credentials_for(true, Some("supervisor".into()), None, None),
            Some((SUPERVISOR_STATES_URL.into(), "supervisor".into()))
        );
        assert_eq!(
            credentials_for(
                false,
                Some("must-not-win".into()),
                Some("http://ha.local:8123/api/config".into()),
                Some("standalone".into())
            ),
            Some((
                "http://ha.local:8123/api/states".into(),
                "standalone".into()
            ))
        );
        assert!(credentials_for(false, Some("supervisor".into()), None, None).is_none());
        assert!(credentials_for(false, None, Some("http://ha.local:8123".into()), None).is_none());
    }

    #[test]
    fn standalone_endpoint_rejects_ambiguous_or_unsafe_url_forms() {
        for url in [
            "file:///etc/passwd",
            "http://user:pass@ha.local:8123/api/config",
            "http://ha.local:8123/api/config?token=x",
            "http://ha.local:8123/api/config#fragment",
            "http://ha.local:8123/proxy/api/config",
        ] {
            assert!(states_endpoint(url).is_none(), "accepted {url}");
        }
        assert_eq!(
            states_endpoint("https://ha.example/api/"),
            Some("https://ha.example/api/states".into())
        );
    }

    #[derive(Clone)]
    struct Stub {
        states: Arc<Mutex<Value>>,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        fail_read_after_service: bool,
        service_delay: Duration,
        service_status: StatusCode,
    }

    fn authorized(headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some("Bearer fixture-token")
    }

    async fn list_states(
        State(stub): State<Stub>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        if !authorized(&headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            );
        }
        (StatusCode::OK, Json(stub.states.lock().unwrap().clone()))
    }

    async fn get_state(
        State(stub): State<Stub>,
        Path(entity_id): Path<String>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        if !authorized(&headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            );
        }
        if stub.fail_read_after_service && !stub.calls.lock().unwrap().is_empty() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"readback unavailable"})),
            );
        }
        let rows = stub.states.lock().unwrap();
        match rows
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["entity_id"] == entity_id))
        {
            Some(row) => (StatusCode::OK, Json(row.clone())),
            None => (StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))),
        }
    }

    async fn service(
        State(stub): State<Stub>,
        Path((domain, service)): Path<(String, String)>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        if !authorized(&headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            );
        }
        stub.calls
            .lock()
            .unwrap()
            .push((format!("{domain}/{service}"), body.clone()));
        let entity = body["entity_id"].as_str().unwrap_or_default();
        {
            let mut rows = stub.states.lock().unwrap();
            if let Some(row) = rows
                .as_array_mut()
                .and_then(|rows| rows.iter_mut().find(|row| row["entity_id"] == entity))
            {
                row["state"] = if service == "turn_on" {
                    json!("on")
                } else {
                    json!("off")
                };
            }
        }
        tokio::time::sleep(stub.service_delay).await;
        (stub.service_status, Json(json!([])))
    }

    async fn server(stub: Stub) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/core/api/states", get(list_states))
            .route("/core/api/states/{entity_id}", get(get_state))
            .route("/core/api/services/{domain}/{service}", post(service))
            .with_state(stub);
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/core/api/states"), task)
    }

    fn entity(id: &str, name: &str, state: &str, extra: Value) -> Value {
        json!({
            "entity_id": id,
            "state": state,
            "attributes": {
                "friendly_name": name,
                "unit_of_measurement": "°C",
                "device_class": "temperature",
                "state_class": "measurement",
                "current_temperature": 4.0,
                "access_token": "must-not-escape",
                "arbitrary_private_attribute": extra
            },
            "last_changed": "2026-10-06T12:00:00Z",
            "last_updated": "2026-10-06T12:00:01Z"
        })
    }

    #[test]
    fn entity_ids_and_discovery_filters_are_closed_and_bounded() {
        assert!(validate_read(&HASSReadStatesTool {
            limit: None,
            ..Default::default()
        })
        .is_err());
        assert!(validate_read(&HASSReadStatesTool {
            entity_id: Some("sensor.x/../../services/restart".into()),
            ..Default::default()
        })
        .is_err());
        assert!(validate_read(&HASSReadStatesTool {
            entity_ids: Some(vec!["sensor.one".into(); MAX_ENTITY_IDS + 1]),
            ..Default::default()
        })
        .is_err());
        assert!(validate_read(&HASSReadStatesTool {
            entity_id: Some("sensor.one".into()),
            domain: Some("sensor".into()),
            ..Default::default()
        })
        .is_err());
        assert!(validate_read(&HASSReadStatesTool {
            domain: Some("sensor".into()),
            limit: Some(101),
            ..Default::default()
        })
        .is_err());
        assert_eq!(
            validate_read(&HASSReadStatesTool {
                name: Some("fridge".into()),
                ..Default::default()
            })
            .unwrap(),
            DEFAULT_LIMIT
        );
    }

    #[tokio::test]
    async fn read_filters_results_reports_truncation_and_excludes_unapproved_attributes() {
        let stub = Stub {
            states: Arc::new(Mutex::new(json!([
                entity(
                    "sensor.fridge_a",
                    "Garage Fridge A",
                    "4",
                    json!("private-a")
                ),
                entity(
                    "sensor.fridge_b",
                    "Garage Fridge B",
                    "5",
                    json!("private-b")
                ),
                entity("light.garage", "Garage Light", "off", json!("private-c"))
            ]))),
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_read_after_service: false,
            service_delay: Duration::ZERO,
            service_status: StatusCode::OK,
        };
        let (base, task) = server(stub).await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let args = HASSReadStatesTool {
            name: Some("fridge".into()),
            limit: Some(1),
            ..Default::default()
        };
        let result = read_states_at(&client, "fixture-token", &base, &args, 1)
            .await
            .unwrap();
        assert_eq!(result.status, "ok");
        assert_eq!(result.matched, 2);
        assert_eq!(result.returned, 1);
        assert!(result.truncated);
        assert!(result.observed_at.ends_with('Z'));
        let value = serde_json::to_value(&result.states[0]).unwrap();
        assert_eq!(value["entity_id"], "sensor.fridge_a");
        assert_eq!(value["unit"], "°C");
        assert_eq!(value["current_temperature"], 4.0);
        assert!(value.get("access_token").is_none());
        assert!(value.get("attributes").is_none());
        task.abort();
    }

    #[tokio::test]
    async fn exact_reads_report_missing_ids_and_light_control_confirms_readback() {
        let stub = Stub {
            states: Arc::new(Mutex::new(json!([entity(
                "light.garage",
                "Garage Light",
                "off",
                json!(null)
            )]))),
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_read_after_service: false,
            service_delay: Duration::ZERO,
            service_status: StatusCode::OK,
        };
        let (base, task) = server(stub.clone()).await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let args = HASSReadStatesTool {
            entity_ids: Some(vec!["light.garage".into(), "switch.garage_fan".into()]),
            ..Default::default()
        };
        let result = read_states_at(&client, "fixture-token", &base, &args, DEFAULT_LIMIT)
            .await
            .unwrap();
        assert_eq!(result.status, "partial");
        assert_eq!(result.missing_entity_ids, vec!["switch.garage_fan"]);

        let action = HASSControlEntityTool {
            entity_id: "light.garage".into(),
            action: HaEntityAction::TurnOn,
        };
        let result = control_entity_at(&client, "fixture-token", &base, &action)
            .await
            .unwrap();
        assert_eq!(result.status, "confirmed");
        assert!(result.accepted && result.confirmed);
        assert_eq!(result.observed_state.as_deref(), Some("on"));
        assert_eq!(
            stub.calls.lock().unwrap().as_slice(),
            &[("light/turn_on".into(), json!({"entity_id":"light.garage"}))]
        );

        let invalid = HASSControlEntityTool {
            entity_id: "lock.front_door".into(),
            action: HaEntityAction::TurnOff,
        };
        assert!(control_entity_at(&client, "fixture-token", &base, &invalid)
            .await
            .is_err());
        assert_eq!(
            stub.calls.lock().unwrap().len(),
            1,
            "unsupported target caused no service call"
        );
        task.abort();
    }

    #[tokio::test]
    async fn accepted_service_with_failed_readback_is_not_reported_confirmed() {
        let stub = Stub {
            states: Arc::new(Mutex::new(json!([entity(
                "switch.garage_fan",
                "Garage Fan",
                "off",
                json!(null)
            )]))),
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_read_after_service: true,
            service_delay: Duration::ZERO,
            service_status: StatusCode::OK,
        };
        let (base, task) = server(stub).await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let action = HASSControlEntityTool {
            entity_id: "switch.garage_fan".into(),
            action: HaEntityAction::TurnOn,
        };
        let result = control_entity_at(&client, "fixture-token", &base, &action)
            .await
            .unwrap();
        assert_eq!(result.status, "accepted_unconfirmed");
        assert!(result.accepted);
        assert!(!result.confirmed);
        assert_eq!(result.observed_state, None);
        task.abort();
    }

    #[tokio::test]
    async fn timeout_after_dispatch_has_uncertain_nonreplay_guidance() {
        let stub = Stub {
            states: Arc::new(Mutex::new(json!([entity(
                "light.garage",
                "Garage Light",
                "off",
                json!(null)
            )]))),
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_read_after_service: false,
            service_delay: Duration::from_millis(100),
            service_status: StatusCode::OK,
        };
        let (base, task) = server(stub).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let action = HASSControlEntityTool {
            entity_id: "light.garage".into(),
            action: HaEntityAction::TurnOn,
        };
        let failure = control_entity_at(&client, "fixture-token", &base, &action)
            .await
            .unwrap_err();
        assert!(matches!(failure, HaControlFailure::Uncertain(_)));
        let payload = serde_json::to_value(failure.payload(&action)).unwrap();
        assert_eq!(payload["status"], "uncertain");
        assert_eq!(payload["dispatch_attempted"], true);
        assert!(payload["accepted"].is_null());
        assert_eq!(payload["confirmed"], false);
        assert!(payload["retry_guidance"]
            .as_str()
            .unwrap()
            .contains("Read the entity state"));
        task.abort();
    }
}
