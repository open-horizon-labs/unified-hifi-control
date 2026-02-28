//! LLM-driven manifest generation via cloud proxy
//!
//! POST /api/manifest/generate — License-gated endpoint that calls the
//! LLM cloud proxy (llm-proxy.ohlabs.ai) to generate custom manifests
//! from natural language prompts.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::api::AppState;
use crate::knobs::manifest::*;

// ── Request / Response types ─────────────────────────────────────────────────

/// POST body for manifest generation.
#[derive(Debug, Deserialize)]
pub struct GenerateManifestRequest {
    /// Natural language prompt describing desired changes.
    pub prompt: String,
    /// Device type (e.g., "knob", "frame"). Defaults to "knob".
    #[serde(default = "default_device_type")]
    pub device_type: String,
}

fn default_device_type() -> String {
    "knob".to_string()
}

/// Response from manifest generation.
#[derive(Debug, Serialize)]
pub struct GenerateManifestResponse {
    pub manifest: Manifest,
}

// ── LLM proxy client trait (for testability) ─────────────────────────────────

/// Trait for calling the LLM proxy, enabling fake injection in tests.
#[async_trait::async_trait]
pub trait LlmProxyClient: Send + Sync {
    async fn call(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        license: &str,
    ) -> Result<String, LlmError>;
}

/// Errors from the LLM proxy call.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    HttpError(String),
    #[error("LLM proxy returned error status {status}: {body}")]
    ProxyError { status: u16, body: String },
    #[error("Failed to parse LLM response: {0}")]
    ParseError(String),
}

// ── Real LLM proxy client ────────────────────────────────────────────────────

/// Production client that calls the real llm-proxy.ohlabs.ai.
pub struct RealLlmProxyClient {
    client: reqwest::Client,
    proxy_url: String,
}

impl Default for RealLlmProxyClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RealLlmProxyClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            proxy_url: "https://llm-proxy.ohlabs.ai/v1/messages".to_string(),
        }
    }
}

#[async_trait::async_trait]
impl LlmProxyClient for RealLlmProxyClient {
    async fn call(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        license: &str,
    ) -> Result<String, LlmError> {
        let body = serde_json::json!({
            "model": "claude-sonnet-4-5-20250514",
            "max_tokens": 4096,
            "system": system_prompt,
            "messages": [
                {
                    "role": "user",
                    "content": user_prompt
                }
            ]
        });

        let resp = self
            .client
            .post(&self.proxy_url)
            .header("Authorization", format!("Bearer {}", license))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::HttpError(e.to_string()))?;

        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "failed to read body".to_string());
            return Err(LlmError::ProxyError { status, body });
        }

        let response_json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LlmError::ParseError(e.to_string()))?;

        // Extract text from the Anthropic Messages API response format:
        // { "content": [{ "type": "text", "text": "..." }] }
        extract_text_from_response(&response_json)
    }
}

/// Extract text content from Anthropic Messages API response.
pub fn extract_text_from_response(response: &serde_json::Value) -> Result<String, LlmError> {
    response
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|block| {
                if block.get("type")?.as_str()? == "text" {
                    block.get("text")?.as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| LlmError::ParseError("No text content in LLM response".to_string()))
}

// ── System prompt ────────────────────────────────────────────────────────────

/// Build the LLM system prompt with the current manifest embedded.
pub fn build_system_prompt(current_manifest_json: &str) -> String {
    format!(
        r#"You are a HiPhi Dial manifest generator. You produce JSON manifests that configure what a physical audio control device displays and how its inputs behave.

## Manifest JSON Schema

A manifest has these top-level fields:
- screens: array of screen objects (types: media, list, card, progress, status)
- nav: {{ order: [screen_ids], default: screen_id }}
- interactions: object mapping physical input names to action names (optional)

A media screen has:
- type: "media"
- id: string
- controls: array of button names to show (optional — absent means show all defaults)
- image_url: string (optional)
- lines: array of {{ text, style }} where style is "title", "subtitle", or "detail"

## Available Controls (buttons on media screen)
prev, play, next, mute

## Available Actions
toggle_playback, play, pause, next, previous, stop, volume_up, volume_down, mute, unmute, toggle_mute

## Physical Inputs (Knob device)
encoder_cw, encoder_ccw, encoder_press, encoder_long_press, button_prev, button_play, button_next, swipe_left, swipe_right

## Current Manifest
{current_manifest_json}

## Rules
1. Preserve all screens and fields the user didn't mention
2. controls[] on the media screen determines which buttons appear. Omit a button to hide it.
3. interactions{{}} maps physical inputs to actions. Unmapped inputs do nothing.
4. Always keep encoder_cw→volume_up and encoder_ccw→volume_down (safety)
5. If adding a mute button, map it in controls AND map a physical input to toggle_mute in interactions
6. Return ONLY valid JSON — no explanation, no markdown fences, just the JSON object"#
    )
}

// ── Manifest parsing ─────────────────────────────────────────────────────────

/// Parse an LLM-generated manifest from raw text.
/// Handles both clean JSON and JSON wrapped in markdown fences.
pub fn parse_manifest_from_llm_text(text: &str) -> Result<ParsedManifest, String> {
    // Strip markdown fences if present
    let cleaned = text.trim();
    let json_str = if cleaned.starts_with("```") {
        // Remove opening fence (with optional language tag) and closing fence
        let without_open = cleaned
            .strip_prefix("```json")
            .or_else(|| cleaned.strip_prefix("```"))
            .unwrap_or(cleaned);
        without_open
            .strip_suffix("```")
            .unwrap_or(without_open)
            .trim()
    } else {
        cleaned
    };

    // Try to parse as our manifest structure
    let value: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| format!("Invalid JSON: {}", e))?;

    // Extract screens
    let screens: Vec<Screen> = value
        .get("screens")
        .ok_or("Missing 'screens' field")?
        .clone()
        .pipe_deserialize()
        .map_err(|e| format!("Invalid screens: {}", e))?;

    // Extract nav
    let nav: Nav = value
        .get("nav")
        .ok_or("Missing 'nav' field")?
        .clone()
        .pipe_deserialize()
        .map_err(|e| format!("Invalid nav: {}", e))?;

    // Extract interactions (optional)
    let interactions: Option<HashMap<String, String>> = value
        .get("interactions")
        .and_then(|v| {
            if v.is_null() {
                None
            } else {
                Some(v.clone().pipe_deserialize())
            }
        })
        .transpose()
        .map_err(|e| format!("Invalid interactions: {}", e))?;

    Ok(ParsedManifest {
        screens,
        nav,
        interactions,
    })
}

/// Helper trait for deserializing serde_json::Value.
trait PipeDeserialize: Sized {
    fn pipe_deserialize<T: serde::de::DeserializeOwned>(self) -> Result<T, serde_json::Error>;
}

impl PipeDeserialize for serde_json::Value {
    fn pipe_deserialize<T: serde::de::DeserializeOwned>(self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self)
    }
}

/// Parsed manifest from LLM output (before merging with fast state).
#[derive(Debug, Clone)]
pub struct ParsedManifest {
    pub screens: Vec<Screen>,
    pub nav: Nav,
    pub interactions: Option<HashMap<String, String>>,
}

// ── Handler ──────────────────────────────────────────────────────────────────

/// POST /api/manifest/generate — Generate a manifest via LLM.
///
/// License-gated: returns 403 if no memex_license is configured.
pub async fn generate_manifest_handler(
    State(state): State<AppState>,
    Json(req): Json<GenerateManifestRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // 1. License gate: check that a memex_license is configured
    let license = state.event_reporter.get_license().await.ok_or_else(|| {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "Memex license required for manifest generation"
            })),
        )
    })?;

    // 2. Get current manifest (pushed or generate a default representation)
    let current_manifest_json = match state.manifests.get_current_manifest_json().await {
        Some(json) => serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string()),
        None => {
            // No pushed manifest — build a minimal default for context
            serde_json::to_string_pretty(&serde_json::json!({
                "screens": [{
                    "type": "media",
                    "id": "now_playing",
                    "lines": [
                        {"text": "Now Playing", "style": "title"},
                        {"text": "Artist", "style": "subtitle"}
                    ]
                }],
                "nav": {"order": ["now_playing"], "default": "now_playing"}
            }))
            .unwrap_or_else(|_| "{}".to_string())
        }
    };

    // 3. Build system prompt
    let system_prompt = build_system_prompt(&current_manifest_json);

    // 4. Call the LLM proxy
    let llm_client = RealLlmProxyClient::new();
    let llm_response = llm_client
        .call(&system_prompt, &req.prompt, &license)
        .await
        .map_err(|e| {
            tracing::error!("LLM proxy call failed: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("LLM proxy error: {}", e)
                })),
            )
        })?;

    // 5. Parse the response into a manifest
    let parsed = parse_manifest_from_llm_text(&llm_response).map_err(|e| {
        tracing::error!("Failed to parse LLM manifest output: {}", e);
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!("LLM produced invalid manifest: {}", e)
            })),
        )
    })?;

    // 6. Store the new manifest
    state
        .manifests
        .set_full(
            parsed.screens.clone(),
            parsed.nav.clone(),
            parsed.interactions.clone(),
        )
        .await;

    tracing::info!(
        screens = parsed.screens.len(),
        device_type = %req.device_type,
        "LLM-generated manifest stored"
    );

    // 7. Return the stored manifest (without fast state — caller can GET /knob/manifest for full)
    let sha = compute_manifest_sha_full(&parsed.screens, &parsed.nav, &parsed.interactions);
    let manifest = serde_json::json!({
        "version": MANIFEST_VERSION,
        "sha": sha,
        "screens": parsed.screens,
        "nav": parsed.nav,
        "interactions": parsed.interactions,
    });

    Ok(Json(manifest))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_system_prompt_includes_current_manifest() {
        let manifest_json = r#"{"screens":[],"nav":{"order":[],"default":"now_playing"}}"#;
        let prompt = build_system_prompt(manifest_json);

        assert!(prompt.contains("HiPhi Dial manifest generator"));
        assert!(prompt.contains(manifest_json));
        assert!(prompt.contains("encoder_cw"));
        assert!(prompt.contains("toggle_mute"));
        assert!(prompt.contains("mute"));
    }

    #[test]
    fn test_parse_manifest_from_clean_json() {
        let json = r#"{
            "screens": [
                {
                    "type": "media",
                    "id": "now_playing",
                    "lines": [{"text": "Title", "style": "title"}],
                    "controls": ["next", "mute"]
                }
            ],
            "nav": {"order": ["now_playing"], "default": "now_playing"},
            "interactions": {
                "encoder_cw": "volume_up",
                "encoder_ccw": "volume_down",
                "encoder_press": "toggle_mute"
            }
        }"#;

        let parsed = parse_manifest_from_llm_text(json).expect("should parse");
        assert_eq!(parsed.screens.len(), 1);
        assert_eq!(parsed.nav.default, "now_playing");

        let interactions = parsed.interactions.expect("should have interactions");
        assert_eq!(interactions.get("encoder_cw").unwrap(), "volume_up");
        assert_eq!(interactions.get("encoder_press").unwrap(), "toggle_mute");

        // Verify controls on media screen
        if let Screen::Media(ref media) = parsed.screens[0] {
            let controls = media.controls.as_ref().expect("should have controls");
            assert_eq!(controls, &["next", "mute"]);
        } else {
            panic!("Expected media screen");
        }
    }

    #[test]
    fn test_parse_manifest_from_markdown_fenced_json() {
        let text = r#"```json
{
    "screens": [
        {
            "type": "media",
            "id": "now_playing",
            "lines": [{"text": "Title", "style": "title"}]
        }
    ],
    "nav": {"order": ["now_playing"], "default": "now_playing"}
}
```"#;

        let parsed = parse_manifest_from_llm_text(text).expect("should parse fenced JSON");
        assert_eq!(parsed.screens.len(), 1);
        assert!(parsed.interactions.is_none());
    }

    #[test]
    fn test_parse_manifest_rejects_invalid_json() {
        let result = parse_manifest_from_llm_text("not json at all");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid JSON"));
    }

    #[test]
    fn test_parse_manifest_rejects_missing_screens() {
        let json = r#"{"nav": {"order": [], "default": "x"}}"#;
        let result = parse_manifest_from_llm_text(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("screens"));
    }

    #[test]
    fn test_parse_manifest_rejects_missing_nav() {
        let json = r#"{"screens": []}"#;
        let result = parse_manifest_from_llm_text(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nav"));
    }

    #[test]
    fn test_extract_text_from_response() {
        let response = serde_json::json!({
            "content": [
                {"type": "text", "text": "{\"screens\":[]}"}
            ]
        });

        let text = extract_text_from_response(&response).expect("should extract text");
        assert_eq!(text, "{\"screens\":[]}");
    }

    #[test]
    fn test_extract_text_from_response_no_content() {
        let response = serde_json::json!({"id": "msg_123"});
        let result = extract_text_from_response(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_manifest_with_controls_serializes_correctly() {
        let media = MediaScreen {
            id: "now_playing".to_string(),
            image_url: None,
            image_key: None,
            background_color: None,
            lines: vec![TextLine {
                text: "Test".to_string(),
                style: "title".to_string(),
            }],
            controls: Some(vec!["next".to_string(), "mute".to_string()]),
        };

        let json = serde_json::to_value(&media).expect("should serialize");
        let controls = json.get("controls").expect("should have controls");
        assert_eq!(controls, &serde_json::json!(["next", "mute"]));
    }

    #[test]
    fn test_manifest_without_controls_omits_field() {
        let media = MediaScreen {
            id: "now_playing".to_string(),
            image_url: None,
            image_key: None,
            background_color: None,
            lines: vec![],
            controls: None,
        };

        let json = serde_json::to_value(&media).expect("should serialize");
        assert!(
            json.get("controls").is_none(),
            "controls should be omitted when None"
        );
    }

    #[test]
    fn test_manifest_with_interactions_serializes_correctly() {
        let mut interactions = HashMap::new();
        interactions.insert("encoder_cw".to_string(), "volume_up".to_string());
        interactions.insert("encoder_ccw".to_string(), "volume_down".to_string());
        interactions.insert("encoder_press".to_string(), "toggle_mute".to_string());

        let manifest = Manifest {
            version: MANIFEST_VERSION,
            sha: "abcd1234".to_string(),
            fast: FastState {
                zone_id: "test".to_string(),
                is_playing: false,
                volume: None,
                volume_min: None,
                volume_max: None,
                volume_step: None,
                volume_type: None,
                seek_position: None,
                length: None,
                transport: Transport {
                    play: true,
                    pause: true,
                    next: true,
                    prev: true,
                },
            },
            screens: vec![],
            nav: Nav {
                order: vec![],
                default: "now_playing".to_string(),
            },
            interactions: Some(interactions),
        };

        let json = serde_json::to_value(&manifest).expect("should serialize");
        let interactions = json.get("interactions").expect("should have interactions");
        assert_eq!(interactions.get("encoder_press").unwrap(), "toggle_mute");
    }

    #[test]
    fn test_manifest_without_interactions_omits_field() {
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            sha: "abcd1234".to_string(),
            fast: FastState {
                zone_id: "test".to_string(),
                is_playing: false,
                volume: None,
                volume_min: None,
                volume_max: None,
                volume_step: None,
                volume_type: None,
                seek_position: None,
                length: None,
                transport: Transport {
                    play: true,
                    pause: true,
                    next: true,
                    prev: true,
                },
            },
            screens: vec![],
            nav: Nav {
                order: vec![],
                default: "now_playing".to_string(),
            },
            interactions: None,
        };

        let json = serde_json::to_value(&manifest).expect("should serialize");
        assert!(
            json.get("interactions").is_none(),
            "interactions should be omitted when None"
        );
    }

    #[test]
    fn test_sha_changes_with_interactions() {
        let screens = vec![];
        let nav = Nav {
            order: vec![],
            default: "now_playing".to_string(),
        };

        let sha_without = compute_manifest_sha_full(&screens, &nav, &None);

        let mut interactions = HashMap::new();
        interactions.insert("encoder_press".to_string(), "toggle_mute".to_string());
        let sha_with = compute_manifest_sha_full(&screens, &nav, &Some(interactions));

        assert_ne!(
            sha_without, sha_with,
            "SHA should change when interactions are added"
        );
    }
}
