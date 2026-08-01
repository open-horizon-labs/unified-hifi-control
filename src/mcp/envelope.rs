//! The structured result envelope every MCP tool fills (issue #395).
//!
//! # The problem this solves
//!
//! Before this module, a tool answered `"Play Now: Superstition"` or
//! `"Volume adjusted"`. Neither states whether the command was accepted, what
//! scope it hit, or what the system looks like now — so a model cannot tell
//! "accepted" from "possibly failed" and retries. #221 recorded ChatGPT looping
//! on `hifi_play` for exactly that reason. Refusals were worse: an operation the
//! provider cannot do and a genuine failure were both just text, so a model could
//! not learn a limit, only re-hit it.
//!
//! # Where the envelope rides, and why
//!
//! On [`CallToolResult::structured_content`] — the wire field `structuredContent`
//! — **never** as a second content block.
//!
//! That is not a preference. `tests/mcp_contract.rs::result_text` defines "the
//! human-readable text" as the concatenation of every text block, and #395's hard
//! constraint is that the text is unchanged. A second block would turn
//! `hifi_zones`'s `"[]"` into `"[]\n{…}"`, which is a changed response however it
//! is framed. `every_tool_result_has_exactly_one_content_block` asserts the
//! single block so that constraint stays checked rather than remembered.
//!
//! The cost is real and is not hidden: the MCP spec *recommends* dual emission
//! ("a tool that returns structured content SHOULD also return the serialized
//! JSON in a TextContent block") for clients predating `structuredContent`, and
//! this module declines that recommendation because the issue's additive
//! constraint outranks it. A client that ignores `structuredContent` sees exactly
//! today's behavior — no regression, no improvement.
//!
//! Also note: `rust-mcp-macros` 0.8.1 hard-codes `output_schema: None`
//! (`src/tool/generator.rs:37`), so no tool can advertise an output schema. That
//! blocks shape discovery from `tools/list` — and it is also why emitting
//! `structuredContent` is safe, since spec-compliant clients only validate it
//! against a declared `outputSchema`.
//!
//! # `data` cannot disagree with the text, but it is not byte-equal to it
//!
//! For the tools whose human text *is* JSON, [`Envelope::json_result`] renders
//! the text from the payload **struct** and `data` from
//! `serde_json::to_value(payload)`. Those two are semantically identical and
//! **differ in key order**: a struct serializes in declaration order, while
//! `serde_json::Value::Object` is a `BTreeMap` (this crate is not built with
//! `preserve_order`) and serializes alphabetically. Rendering the text from the
//! `Value` instead would reorder the keys of `hifi_zones`, `hifi_now_playing`,
//! `hifi_search` and `hifi_hqplayer_status` — i.e. it would change the text this
//! issue promises not to change. See the warning at the top of
//! [`crate::mcp::types`].
//!
//! So the guarantee is **parsed-value equality**, not byte equality, and every
//! test asserting it compares `serde_json::Value`s rather than strings.
//!
//! # Versioning
//!
//! [`ENVELOPE_SCHEMA`] is `uhc.mcp.envelope/<major>`. The compatibility rule,
//! stated here because #396, #397, #399 and #400 all build on this shape:
//!
//! - **No bump:** adding an optional envelope field, adding a [`Refusal`]
//!   variant, adding a field inside `data`, or populating `observed` where it was
//!   previously absent. Consumers must ignore fields they do not know.
//! - **Bump:** removing or retyping a field, removing a `Refusal` variant, or
//!   changing what an [`Outcome`] means.
//!
//! A bump is an epic-level decision (#392), not an individual issue's, because
//! every downstream issue's payload rides this shape.
//!
//! # What downstream issues attach where
//!
//! | Issue | Attaches |
//! |---|---|
//! | #396 REFS | `data` — an opaque `ref` per search hit; a new `Refusal` variant for an expired ref |
//! | #397 RESOURCES | projects `data` verbatim as resource contents; nothing else of the envelope |
//! | #398 CAPABILITIES | reclassifies [`Refusal::ProviderLimitation`] vs [`Refusal::NotImplemented`], and closes `operation` for `hifi_control` |
//! | #399 BROWSE / #400 QUEUE | `data` for their payloads; `observed` with a new [`ReadFrom`] variant for adapter-sourced reads |

use serde::Serialize;
use serde_json::{Map, Value};

use crate::api::AppState;
use crate::mcp::types::{text_result, McpNowPlaying};
use rust_mcp_sdk::schema::{schema_utils::CallToolError, CallToolResult};

/// The envelope's schema identifier. See the module docs for the bump rule.
pub const ENVELOPE_SCHEMA: &str = "uhc.mcp.envelope/1";

// =============================================================================
// Outcome
// =============================================================================

/// What happened, in a form a model can branch on.
///
/// # `verified` is deliberately absent
///
/// No adapter confirms a command, and [`Observed`] is a snapshot that may predate
/// the call. #395 forbids inventing verification the adapters cannot do, so the
/// vocabulary has no way to express it — a claim that cannot be spelled cannot be
/// made by accident. #360's `pending`/`verified`/`failed` maps on as
/// [`Self::Accepted`] / [`Self::Ok`] (reads only) / [`Self::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A read succeeded and `data` is the answer. Reads only — a read is
    /// self-confirming, a write is not.
    Ok,
    /// A write was accepted by the backend without error. **No claim** is made
    /// that the intended effect has happened; `observed`, when present, is a
    /// snapshot with its own timestamp, not a confirmation.
    Accepted,
    /// Refused because the operation is not available here. `refusal` says
    /// whether that is the provider's limit or UHC's gap.
    Unsupported,
    /// Refused because the request was wrong. `refusal` names the parameter.
    Invalid,
    /// Attempted, and it failed.
    Error,
}

// =============================================================================
// Scope
// =============================================================================

/// The backend UHC **identified** for a call.
///
/// Values match the zone-id prefixes so a client can correlate them without a
/// lookup table.
///
/// # Identification, not routing
///
/// This is derived from [`ZoneTarget`](crate::mcp::routing::ZoneTarget) — what
/// UHC could work out about the zone id — and **not** from the route the call
/// then took. The two differ for an unrecognised prefix: `sonos:abc` is
/// identified as [`Self::Unknown`] while transport, search and play still send it
/// to Roon (today's silent default, #398's to fix).
///
/// Reporting `roon` there would be worse than saying nothing. A model would learn
/// that a Sonos zone is a Roon zone and that Roon is at fault for the failure,
/// when the actual remedy is a valid zone id. #392 rule 3 forbids making a
/// provider look responsible for a UHC decision, and a field literally named
/// `provider` is the last place to bend that.
///
/// So `provider: "unknown"` next to a Roon-shaped failure detail is the legible
/// fingerprint of the silent default, sitting where #398 can find it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Roon,
    Lms,
    #[serde(rename = "openhome")]
    OpenHome,
    #[serde(rename = "upnp")]
    Upnp,
    #[serde(rename = "hqplayer")]
    HqPlayer,
    /// The zone id's prefix names no adapter, so UHC identified nothing.
    ///
    /// No claim is made about such a zone's capabilities, because none can be.
    Unknown,
}

/// What a call acted on.
#[derive(Debug, Serialize)]
pub struct Scope {
    /// The adapter this call was routed to.
    pub provider: Provider,
    /// The zone id as the client supplied it. Absent for tools with no zone
    /// (`hifi_status`, the HQPlayer tools) and for `hifi_zones`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    /// The zone's name **as the aggregator knows it**, not as the client guessed.
    /// Absent when the aggregator has no such zone — which, paired with a present
    /// `zone_id`, is how a client tells a typo from a zone that exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_name: Option<String>,
}

impl Scope {
    /// A provider-only scope, for tools that act on no zone.
    pub fn provider_only(provider: Provider) -> Self {
        Self {
            provider,
            zone_id: None,
            zone_name: None,
        }
    }

    /// A zone scope with the name **read back** from the aggregator.
    ///
    /// AGENTS.md: the aggregator owns zone state. The name is never taken from an
    /// adapter or echoed from the request.
    pub async fn for_zone(state: &AppState, zone_id: &str, provider: Provider) -> Self {
        let zone_name = state
            .aggregator
            .get_zone(zone_id)
            .await
            .map(|z| z.zone_name);
        Self {
            provider,
            zone_id: Some(zone_id.to_string()),
            zone_name,
        }
    }
}

// =============================================================================
// Observed state
// =============================================================================

/// Where an [`Observed`] snapshot was read from.
///
/// One variant today. #400 adds adapter-sourced queue reads, which is why this is
/// an enum rather than an implicit constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadFrom {
    /// `ZoneAggregator` — the single source of truth for zone state.
    Aggregator,
}

/// State read back after a write.
///
/// # This is not verification
///
/// The aggregator is updated by bus events, which arrive on the adapters' own
/// schedules — LMS polls, Roon pushes. So this snapshot may predate the command
/// that produced it. `as_of_ms` is included precisely so the client can judge
/// that for itself instead of being told a conclusion the server cannot support.
#[derive(Debug, Serialize)]
pub struct Observed {
    /// Provenance. Named `read_from` rather than `source` to avoid colliding with
    /// `hifi_search`'s `source` parameter in the field inventory.
    pub read_from: ReadFrom,
    /// The aggregator's `last_updated` for this zone, ms since epoch.
    pub as_of_ms: u64,
    /// The zone as the aggregator currently holds it.
    pub zone: McpNowPlaying,
}

impl Observed {
    /// Read a zone back from the aggregator, or `None` if it holds no such zone.
    pub async fn from_aggregator(state: &AppState, zone_id: &str) -> Option<Self> {
        let zone = state.aggregator.get_zone(zone_id).await?;
        Some(Self {
            read_from: ReadFrom::Aggregator,
            as_of_ms: zone.last_updated,
            zone: crate::mcp::types::now_playing_from_zone(zone),
        })
    }
}

// =============================================================================
// Refusals
// =============================================================================

/// Why a call was refused, in the terms #395 requires to be distinguishable.
///
/// Each variant implies a *different* client action, which is the test for
/// whether a distinction earns its place:
///
/// | variant | outcome | what the client should do |
/// |---|---|---|
/// | [`Self::ProviderLimitation`] | `unsupported` | never retry; use `alternatives` |
/// | [`Self::NotImplemented`]     | `unsupported` | retry once `tracked_by` ships |
/// | [`Self::InvalidParameter`]   | `invalid`     | resend with a value from `accepted` |
/// | [`Self::UnknownTarget`]      | `invalid`     | enumerate via `discover_with` |
/// | [`Self::BackendError`]       | `error`       | retrying may work |
///
/// The variant determines the [`Outcome`] — see [`Self::outcome`] and
/// [`Envelope::refuse`] — so the two can never contradict each other.
///
/// # `detail` is the envelope's sentence, not a copy of the text
///
/// Where today's frozen prose is misleading, `detail` says the true thing. The
/// clearest case is OpenHome/UPnP volume: the text says "not supported for this
/// zone type", and the truth is that UHC's adapters implement it and expose it
/// over HTTP while the MCP path simply does not call it. Correcting the prose is
/// #398's job; being honest in the structured payload is this issue's.
#[derive(Debug, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// The provider's own protocol cannot do this. It will never work.
    ///
    /// Use only with evidence about the provider. #392 rule 3: classifying a UHC
    /// gap this way is how LMS got understated in the first place.
    ProviderLimitation {
        /// What was attempted, in the envelope's `operation` vocabulary.
        operation: String,
        /// What this zone *can* do instead, as callable tool invocations.
        alternatives: Vec<String>,
        detail: String,
    },
    /// The provider supports this; UHC has not wired it to MCP yet.
    NotImplemented {
        operation: String,
        /// The UHC issue that will implement it, so "not yet" is checkable.
        tracked_by: &'static str,
        alternatives: Vec<String>,
        detail: String,
    },
    /// A parameter's value is not accepted.
    ///
    /// Also covers a parameter that is required in this context and absent — the
    /// remedy is the same, so the distinction would not change client behavior.
    InvalidParameter {
        parameter: &'static str,
        /// The accepted values, or the accepted form when the set is open.
        accepted: Vec<String>,
        detail: String,
    },
    /// The named target does not exist and the valid set must be enumerated.
    UnknownTarget {
        parameter: &'static str,
        /// The tool that lists valid values for `parameter`.
        discover_with: &'static str,
        detail: String,
    },
    /// The operation was attempted and the backend failed.
    BackendError { detail: String },
}

impl Refusal {
    /// The outcome this refusal implies. The single mapping; see [`Envelope::refuse`].
    pub fn outcome(&self) -> Outcome {
        match self {
            Self::ProviderLimitation { .. } | Self::NotImplemented { .. } => Outcome::Unsupported,
            Self::InvalidParameter { .. } | Self::UnknownTarget { .. } => Outcome::Invalid,
            Self::BackendError { .. } => Outcome::Error,
        }
    }

    /// A backend failure whose detail is the same sentence as the human text.
    pub fn backend_error(detail: impl Into<String>) -> Self {
        Self::BackendError {
            detail: detail.into(),
        }
    }

    /// An unaccepted parameter value.
    pub fn invalid_parameter(
        parameter: &'static str,
        accepted: &[&str],
        detail: impl Into<String>,
    ) -> Self {
        Self::InvalidParameter {
            parameter,
            accepted: accepted.iter().map(|s| (*s).to_string()).collect(),
            detail: detail.into(),
        }
    }
}

// =============================================================================
// The envelope
// =============================================================================

/// One structured result shape, filled by every tool.
///
/// Field order here is the serialized order, and it is chosen so the two things a
/// model needs first — what happened, and to what — come first.
#[derive(Debug, Serialize)]
pub struct Envelope {
    /// [`ENVELOPE_SCHEMA`]. Present so a consumer can branch on the shape without
    /// guessing from field presence.
    pub schema: &'static str,
    pub outcome: Outcome,
    /// The MCP tool name.
    pub tool: &'static str,
    /// The server's normalized name for what it did.
    ///
    /// **Not a capability name** — #398 owns capability vocabulary and may map
    /// operations onto it. **Not a closed set:** `hifi_control` ends its action
    /// match with `other => other`, so an unrecognised action appears here
    /// verbatim. A client must not treat this as an enum. #398 closes it by
    /// rejecting unknown actions.
    pub operation: String,
    /// The parameters **as the server resolved them** — not an echo of the
    /// request.
    ///
    /// Keys are always a subset of the tool's own declared input parameters, so a
    /// client already knows every name. Values are post-normalization, and that
    /// is the point: `hifi_control action=volume_down` with no `value` reports
    /// `value: -5.0`, which is the only place the defaulted delta and the sign
    /// are observable. Differing from the request is intended, not a bug.
    pub params: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    /// State read back after a write. Absent for reads (the read *is* the
    /// observation, and duplicating it would create two places a client could
    /// look and disagree) and absent for HQPlayer writes, whose state the
    /// aggregator does not hold and must not — see #397's constraints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<Observed>,
    /// Present exactly when `outcome` is `unsupported`, `invalid` or `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
    /// The tool's own payload — the same JSON the human text carries, for the
    /// tools whose text is JSON. #397 projects this verbatim as resource
    /// contents; #396/#399/#400 extend it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Envelope {
    fn new(tool: &'static str, operation: impl Into<String>, outcome: Outcome) -> Self {
        Self {
            schema: ENVELOPE_SCHEMA,
            outcome,
            tool,
            operation: operation.into(),
            params: Map::new(),
            scope: None,
            observed: None,
            refusal: None,
            data: None,
        }
    }

    /// A read. Succeeds as [`Outcome::Ok`] unless refused.
    pub fn read(tool: &'static str, operation: impl Into<String>) -> Self {
        Self::new(tool, operation, Outcome::Ok)
    }

    /// A write. Succeeds as [`Outcome::Accepted`] — never `ok`, because nothing
    /// here can confirm the effect.
    pub fn write(tool: &'static str, operation: impl Into<String>) -> Self {
        Self::new(tool, operation, Outcome::Accepted)
    }

    /// Record a resolved parameter. The name must be one the tool declares.
    pub fn param(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.params.insert(name.to_string(), value.into());
        self
    }

    /// Record an optional parameter, omitting it entirely when absent rather than
    /// emitting `null` — an absent parameter and one explicitly set to null are
    /// different requests.
    pub fn param_opt(self, name: &str, value: Option<impl Into<Value>>) -> Self {
        match value {
            Some(v) => self.param(name, v),
            None => self,
        }
    }

    pub fn scope(mut self, scope: Scope) -> Self {
        self.scope = Some(scope);
        self
    }

    pub fn observed(mut self, observed: Option<Observed>) -> Self {
        self.observed = observed;
        self
    }

    /// Attach a refusal, which also sets the outcome. The two cannot disagree
    /// because there is only one place either is decided.
    pub fn refuse(mut self, refusal: Refusal) -> Self {
        self.outcome = refusal.outcome();
        self.refusal = Some(refusal);
        self
    }

    /// Attach `data` without rendering it as the text.
    ///
    /// For tools whose text is prose. [`Self::json_result`] is the path for tools
    /// whose text *is* the payload.
    pub fn data(mut self, payload: &impl Serialize) -> Self {
        self.data = serde_json::to_value(payload).ok();
        self
    }

    /// Serialize to the map `structuredContent` carries.
    fn structured(&self) -> Option<Map<String, Value>> {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => Some(map),
            // Unreachable for this type: every field is infallibly serializable.
            // Degrading to "no envelope" rather than to a wrong one keeps the
            // additive promise even if that ever stops being true.
            _ => None,
        }
    }

    fn attach(self, result: CallToolResult) -> CallToolResult {
        match self.structured() {
            Some(map) => result.with_structured_content(map),
            None => result,
        }
    }

    /// A result whose text is the pretty-printed payload, with `data` set from
    /// the same payload.
    ///
    /// The text is rendered from the payload **struct**, exactly as
    /// `types::json_result` did before this issue, so declaration order is
    /// preserved. `data` goes through `to_value`, whose `Map` is a `BTreeMap` and
    /// therefore alphabetical. The two are equal as JSON values and differ in key
    /// order — see the module docs.
    pub fn json_result<T: Serialize>(mut self, payload: &T) -> CallToolResult {
        self.data = serde_json::to_value(payload).ok();
        let text = serde_json::to_string_pretty(payload).unwrap_or_else(|_| "{}".to_string());
        self.attach(text_result(text))
    }

    /// A result whose text is prose. The text is passed through verbatim.
    pub fn text_result(self, text: String) -> CallToolResult {
        self.attach(text_result(text))
    }

    /// A refusal: the frozen `Error: {text}` prose, plus the structured reason.
    ///
    /// Both are arguments so that every call site shows the frozen string and its
    /// classification side by side.
    pub fn refused(
        self,
        text: impl std::fmt::Display,
        refusal: Refusal,
    ) -> Result<CallToolResult, CallToolError> {
        Ok(self.refuse(refusal).text_result(format!("Error: {}", text)))
    }

    /// A backend failure whose `detail` is the same sentence as the text. The
    /// common case.
    pub fn failed(self, text: impl std::fmt::Display) -> Result<CallToolResult, CallToolError> {
        let text = text.to_string();
        let refusal = Refusal::backend_error(text.clone());
        self.refused(text, refusal)
    }

    /// A result that keeps the SDK's own `isError: true` framing while gaining an
    /// envelope.
    ///
    /// The only place this is needed is argument-parse failure, which happens
    /// before any tool runs — see [`crate::mcp::handler`]. Returning
    /// `Err(CallToolError)` there produces `content: [text], isError: true` and no
    /// structured payload; this reproduces that content byte for byte, `isError`
    /// included, and attaches the envelope.
    pub fn errored_result(self, error: CallToolError) -> CallToolResult {
        self.attach(CallToolResult::with_error(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal -> outcome mapping is the whole reason the two cannot
    /// disagree. Asserted per variant so a new variant fails until it is mapped.
    #[test]
    fn every_refusal_implies_its_documented_outcome() {
        let cases: Vec<(Refusal, Outcome)> = vec![
            (
                Refusal::ProviderLimitation {
                    operation: "radio".into(),
                    alternatives: vec![],
                    detail: "d".into(),
                },
                Outcome::Unsupported,
            ),
            (
                Refusal::NotImplemented {
                    operation: "volume".into(),
                    tracked_by: "#398",
                    alternatives: vec![],
                    detail: "d".into(),
                },
                Outcome::Unsupported,
            ),
            (
                Refusal::invalid_parameter("value", &["0-100"], "d"),
                Outcome::Invalid,
            ),
            (
                Refusal::UnknownTarget {
                    parameter: "zone_id",
                    discover_with: "hifi_zones",
                    detail: "d".into(),
                },
                Outcome::Invalid,
            ),
            (Refusal::backend_error("d"), Outcome::Error),
        ];

        for (refusal, expected) in cases {
            assert_eq!(
                refusal.outcome(),
                expected,
                "refusal {refusal:?} must imply {expected:?}"
            );
            // And the envelope must adopt it rather than keeping its own.
            let env = Envelope::read("hifi_zones", "list_zones").refuse(refusal);
            assert_eq!(env.outcome, expected);
            assert!(env.refusal.is_some());
        }
    }

    /// A successful envelope must never carry a refusal, and vice versa.
    #[test]
    fn refusal_presence_tracks_the_outcome() {
        let read = Envelope::read("hifi_zones", "list_zones");
        assert_eq!(read.outcome, Outcome::Ok);
        assert!(read.refusal.is_none());

        let write = Envelope::write("hifi_control", "play");
        assert_eq!(write.outcome, Outcome::Accepted);
        assert!(write.refusal.is_none());
    }

    /// `json_result` must render the text in declaration order while `data`
    /// carries the same value. This is the trap the design gate's dissent caught:
    /// rendering the text from `to_value` would reorder these keys and change the
    /// bytes clients receive.
    #[test]
    fn json_result_keeps_declaration_order_in_the_text() {
        #[derive(Serialize)]
        struct Payload {
            zebra: u8,
            alpha: u8,
        }

        let result = Envelope::read("t", "op").json_result(&Payload { zebra: 1, alpha: 2 });

        let text = match result.content.first() {
            Some(rust_mcp_sdk::schema::ContentBlock::TextContent(t)) => t.text.clone(),
            other => panic!("expected one text block, got {other:?}"),
        };
        assert!(
            text.find("zebra") < text.find("alpha"),
            "the text must keep declaration order: {text}"
        );

        // `data` says the same thing, in BTreeMap order.
        let structured = result
            .structured_content
            .expect("envelope must be attached");
        let data = structured.get("data").expect("data must be set");
        assert_eq!(data, &serde_json::json!({ "alpha": 2, "zebra": 1 }));

        // Equal as values, which is the guarantee that actually matters.
        let from_text: Value = serde_json::from_str(&text).expect("text must be JSON");
        assert_eq!(&from_text, data);
    }

    /// An absent optional parameter is omitted, not emitted as null.
    #[test]
    fn param_opt_omits_rather_than_nulls() {
        let env = Envelope::read("hifi_search", "search")
            .param("query", "q")
            .param_opt("source", None::<String>);
        assert!(!env.params.contains_key("source"));
        assert_eq!(env.params.get("query"), Some(&Value::from("q")));
    }
}
