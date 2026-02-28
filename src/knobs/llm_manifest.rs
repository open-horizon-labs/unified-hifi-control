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
    /// Zone to apply the generated manifest to.
    pub zone_id: String,
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

## Manifest JSON Schema (v2 command-pattern)

A manifest has these top-level fields:
- screens: array of screen objects (types: media, list, card, progress, status)
- nav: {{ order: [screen_ids], default: screen_id }}

A media screen uses the command-pattern schema:
- type: "media"
- id: string
- lines: array of {{ text, style }} where style is "title", "subtitle", or "detail"
- elements: array of element objects (see below) — determines which buttons appear
- encoder: object with cw, ccw, press, long_press fields

## Element Schema
Each element is a self-contained {{display, behavior}} unit:
```json
{{
  "display": {{
    "icon": "skip_previous",   // Material Symbols icon name
    "label": null,             // optional text label
    "active": null             // optional boolean highlight
  }},
  "on_tap": {{ "action": "previous", "params": null }},
  "on_long_press": null
}}
```

## Encoder Schema
```json
{{
  "cw": {{ "action": "volume_up" }},
  "ccw": {{ "action": "volume_down" }},
  "press": {{ "action": "toggle_playback" }},
  "long_press": {{ "action": "show_zone_picker" }}
}}
```

## Available Actions
toggle_playback, play, pause, next, previous, stop,
volume_up, volume_down, toggle_mute, mute, unmute,
show_zone_picker, list_scroll_up, list_scroll_down, list_select,
seek_forward (skip ahead in track), seek_backward (skip back in track),
select_zone (select the highlighted zone in the zone picker list)

## Available Icon Names (Material Symbols)
Transport: skip_previous, play_arrow, pause, stop, skip_next, shuffle, repeat, repeat_one
Seek: forward_5, forward_10, forward_30, replay_5, replay_10, replay_30
Volume: volume_up, volume_down, volume_mute, volume_off
Other: music_note, settings

## Maximum Elements
Up to 6 elements per media screen.

## Current Manifest
{current_manifest_json}

## Rules
1. Preserve all screens and fields the user didn't mention.
2. The elements[] array on the media screen determines which buttons appear. Remove an element to hide that button.
3. SAFETY GUARDRAIL: encoder.cw must always be volume_up and encoder.ccw must always be volume_down. Never change these.
4. encoder.press and encoder.long_press are configurable.
5. Do not include "controls" or "interactions" fields — those are deprecated v1 fields. Use elements + encoder only.
6. Return ONLY valid JSON — no explanation, no markdown fences, just the JSON object."#
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
    let current_manifest_json = match state
        .manifests
        .get_current_manifest_json(&req.zone_id)
        .await
    {
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

    // 6. Validate the parsed manifest
    validate_manifest(&parsed).map_err(|e| {
        tracing::warn!("LLM manifest failed validation: {}", e);
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!("Manifest validation failed: {}", e)
            })),
        )
    })?;

    // 7. Store the new manifest for the requested zone
    state
        .manifests
        .set_full(
            &req.zone_id,
            parsed.screens.clone(),
            parsed.nav.clone(),
            parsed.interactions.clone(),
        )
        .await;

    tracing::info!(
        zone_id = %req.zone_id,
        screens = parsed.screens.len(),
        device_type = %req.device_type,
        "LLM-generated manifest stored"
    );

    // 8. Return the stored manifest (without fast state — caller can GET /knob/manifest for full)
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

// ── Validation ───────────────────────────────────────────────────────────────

const MAX_ELEMENTS: usize = 6;

/// Validate a parsed manifest against known controls, actions, and inputs.
/// Returns `Ok(())` if valid, `Err(message)` describing the first problem found.
pub fn validate_manifest(parsed: &ParsedManifest) -> Result<(), String> {
    let valid_controls = ["prev", "play", "next", "mute"];
    let valid_actions = [
        "toggle_playback",
        "play",
        "pause",
        "next",
        "previous",
        "stop",
        "volume_up",
        "volume_down",
        "mute",
        "unmute",
        "toggle_mute",
        "show_zone_picker",
        "list_scroll_up",
        "list_scroll_down",
        "list_select",
        "seek_forward",
        "seek_backward",
        "select_zone",
    ];
    let valid_inputs = [
        "encoder_cw",
        "encoder_ccw",
        "encoder_press",
        "encoder_long_press",
        "button_prev",
        "button_play",
        "button_next",
        "swipe_left",
        "swipe_right",
    ];

    for screen in &parsed.screens {
        if let Screen::Media(media) = screen {
            // Validate v1 controls (backward compat)
            if let Some(controls) = &media.controls {
                for c in controls {
                    if !valid_controls.contains(&c.as_str()) {
                        return Err(format!(
                            "Unknown control: '{}'. Valid: {:?}",
                            c, valid_controls
                        ));
                    }
                }
            }

            // Validate v2 elements
            if let Some(elements) = &media.elements {
                if elements.len() > MAX_ELEMENTS {
                    return Err(format!(
                        "Too many elements: {} (max {})",
                        elements.len(),
                        MAX_ELEMENTS
                    ));
                }
                for (i, elem) in elements.iter().enumerate() {
                    if let Some(ref tap) = elem.on_tap {
                        if !valid_actions.contains(&tap.action.as_str()) {
                            return Err(format!(
                                "Element[{}].on_tap: unknown action '{}'. Valid: {:?}",
                                i, tap.action, valid_actions
                            ));
                        }
                    }
                    if let Some(ref lp) = elem.on_long_press {
                        if !valid_actions.contains(&lp.action.as_str()) {
                            return Err(format!(
                                "Element[{}].on_long_press: unknown action '{}'. Valid: {:?}",
                                i, lp.action, valid_actions
                            ));
                        }
                    }
                }
            }

            // Validate v2 encoder
            if let Some(ref encoder) = media.encoder {
                // Safety guardrail: CW must be volume_up, CCW must be volume_down
                if encoder.cw.action != "volume_up" {
                    return Err(format!(
                        "Safety: encoder.cw must be 'volume_up', got '{}'",
                        encoder.cw.action
                    ));
                }
                if encoder.ccw.action != "volume_down" {
                    return Err(format!(
                        "Safety: encoder.ccw must be 'volume_down', got '{}'",
                        encoder.ccw.action
                    ));
                }
                if let Some(ref press) = encoder.press {
                    if !valid_actions.contains(&press.action.as_str()) {
                        return Err(format!(
                            "encoder.press: unknown action '{}'. Valid: {:?}",
                            press.action, valid_actions
                        ));
                    }
                }
                if let Some(ref lp) = encoder.long_press {
                    if !valid_actions.contains(&lp.action.as_str()) {
                        return Err(format!(
                            "encoder.long_press: unknown action '{}'. Valid: {:?}",
                            lp.action, valid_actions
                        ));
                    }
                }
            }
        } else if let Screen::List(list) = screen {
            // Validate list screen encoder actions against the whitelist.
            // No volume lock here — list screens use list_scroll_down/list_scroll_up for CW/CCW.
            if let Some(ref encoder) = list.encoder {
                if !valid_actions.contains(&encoder.cw.action.as_str()) {
                    return Err(format!(
                        "list encoder.cw: unknown action '{}'. Valid: {:?}",
                        encoder.cw.action, valid_actions
                    ));
                }
                if !valid_actions.contains(&encoder.ccw.action.as_str()) {
                    return Err(format!(
                        "list encoder.ccw: unknown action '{}'. Valid: {:?}",
                        encoder.ccw.action, valid_actions
                    ));
                }
                if let Some(ref press) = encoder.press {
                    if !valid_actions.contains(&press.action.as_str()) {
                        return Err(format!(
                            "list encoder.press: unknown action '{}'. Valid: {:?}",
                            press.action, valid_actions
                        ));
                    }
                }
                if let Some(ref lp) = encoder.long_press {
                    if !valid_actions.contains(&lp.action.as_str()) {
                        return Err(format!(
                            "list encoder.long_press: unknown action '{}'. Valid: {:?}",
                            lp.action, valid_actions
                        ));
                    }
                }
            }
        }
    }

    // Validate interactions (v1 backward compat)
    if let Some(interactions) = &parsed.interactions {
        for (input, action) in interactions {
            if !valid_inputs.contains(&input.as_str()) {
                return Err(format!(
                    "Unknown input: '{}'. Valid: {:?}",
                    input, valid_inputs
                ));
            }
            if !valid_actions.contains(&action.as_str()) {
                return Err(format!(
                    "Unknown action: '{}'. Valid: {:?}",
                    action, valid_actions
                ));
            }
        }

        // Safety: volume controls must be preserved when interactions present
        let has_vol_up = interactions
            .get("encoder_cw")
            .is_some_and(|a| a == "volume_up");
        let has_vol_down = interactions
            .get("encoder_ccw")
            .is_some_and(|a| a == "volume_down");
        if !has_vol_up || !has_vol_down {
            return Err(
                "Safety: encoder_cw must map to volume_up and encoder_ccw must map to volume_down"
                    .to_string(),
            );
        }
    }

    Ok(())
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
        assert!(prompt.contains("command-pattern"));
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
            elements: None,
            encoder: None,
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
            elements: None,
            encoder: None,
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

    // ── Validation tests ─────────────────────────────────────────────

    /// Helper: build a valid ParsedManifest with standard controls and safe interactions.
    fn make_valid_parsed_manifest() -> ParsedManifest {
        let mut interactions = HashMap::new();
        interactions.insert("encoder_cw".to_string(), "volume_up".to_string());
        interactions.insert("encoder_ccw".to_string(), "volume_down".to_string());
        interactions.insert("encoder_press".to_string(), "toggle_mute".to_string());

        ParsedManifest {
            screens: vec![Screen::Media(MediaScreen {
                id: "now_playing".to_string(),
                image_url: None,
                image_key: None,
                background_color: None,
                lines: vec![TextLine {
                    text: "Title".to_string(),
                    style: "title".to_string(),
                }],
                controls: Some(vec![
                    "prev".to_string(),
                    "play".to_string(),
                    "next".to_string(),
                ]),
                elements: None,
                encoder: None,
            })],
            nav: Nav {
                order: vec!["now_playing".to_string()],
                default: "now_playing".to_string(),
            },
            interactions: Some(interactions),
        }
    }

    #[test]
    fn test_validate_manifest_valid_passes() {
        let parsed = make_valid_parsed_manifest();
        assert!(
            validate_manifest(&parsed).is_ok(),
            "Valid manifest should pass validation"
        );
    }

    #[test]
    fn test_validate_manifest_unknown_control_fails() {
        let mut parsed = make_valid_parsed_manifest();
        if let Screen::Media(ref mut media) = parsed.screens[0] {
            media.controls = Some(vec!["prev".to_string(), "shuffle".to_string()]);
        }
        let result = validate_manifest(&parsed);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("Unknown control: 'shuffle'"),
            "Should identify the unknown control"
        );
    }

    #[test]
    fn test_validate_manifest_unknown_input_fails() {
        let mut parsed = make_valid_parsed_manifest();
        let interactions = parsed.interactions.as_mut().unwrap();
        interactions.insert("triple_tap".to_string(), "play".to_string());
        let result = validate_manifest(&parsed);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("Unknown input: 'triple_tap'"),
            "Should identify the unknown input"
        );
    }

    #[test]
    fn test_validate_manifest_unknown_action_fails() {
        let mut parsed = make_valid_parsed_manifest();
        let interactions = parsed.interactions.as_mut().unwrap();
        interactions.insert("encoder_press".to_string(), "rewind".to_string());
        let result = validate_manifest(&parsed);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("Unknown action: 'rewind'"),
            "Should identify the unknown action"
        );
    }

    #[test]
    fn test_validate_manifest_missing_volume_safety_fails() {
        let mut parsed = make_valid_parsed_manifest();
        // Remove encoder_cw → volume_up mapping
        let interactions = parsed.interactions.as_mut().unwrap();
        interactions.insert("encoder_cw".to_string(), "next".to_string());
        let result = validate_manifest(&parsed);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("Safety:"),
            "Should flag missing volume safety"
        );
    }

    #[test]
    fn test_validate_manifest_no_interactions_passes() {
        let mut parsed = make_valid_parsed_manifest();
        parsed.interactions = None;
        assert!(
            validate_manifest(&parsed).is_ok(),
            "Manifest without interactions should pass (interactions are optional)"
        );
    }

    #[test]
    fn test_validate_manifest_no_controls_passes() {
        let mut parsed = make_valid_parsed_manifest();
        if let Screen::Media(ref mut media) = parsed.screens[0] {
            media.controls = None;
        }
        assert!(
            validate_manifest(&parsed).is_ok(),
            "Manifest without controls should pass (controls are optional)"
        );
    }
}
