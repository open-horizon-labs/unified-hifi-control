//! Tier-1 read-only live verification: capture a daemon, diff it against the corpus.
//!
//! This is the workflow ADR 003 names as the real-daemon merge gate. The corpus is transcribed
//! rather than captured, so the only thing that can settle it is a comparison against live hardware —
//! and the only comparison safe to run against someone's listening room is a read-only one.
//!
//! **Read-only, absolutely.** Nothing here sends `Set*`, `Volume*`, transport or matrix-set commands.
//! That constraint has a consequence worth stating rather than hiding: `GetFilters`, `GetShapers` and
//! `GetRates` are **mode-relative**, and reaching the inactive mode's lists would require `SetMode`.
//! So tier 1 verifies the lists for whichever mode the daemon is *already* in, records which mode
//! that was, and leaves the other mode's lists derived until tier 2. [`Report`] says so explicitly
//! rather than letting a green run imply per-mode coverage it did not have.
//!
//! The same code runs hermetically against the fake daemon — that is how the differ itself is tested,
//! and how a deliberate corpus/daemon mismatch is proven to be detected rather than assumed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use unified_hifi_control::adapters::hqplayer::{framing, HqpAdapter};

use super::corpus::{self, EnumEntry};
use super::raw;

/// Whether a `Status` document actually carried its optional self-closing `metadata` child.
///
/// Three states, deliberately. An earlier version inferred presence from `samplerate > 0`, which made
/// a present child carrying zeros indistinguishable from no child at all — the same
/// infer-a-fact-from-a-legitimate-zero mistake as the original `unwrap_or(0)` volume defect, committed
/// inside the tool built to catch it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MetadataChild {
    Present,
    Absent,
    /// Never looked at. Distinct from `Absent`, and must never read as a satisfied claim.
    #[default]
    Unobserved,
}

/// Shape facts about an enumeration entry that the semantic types cannot carry.
///
/// `FilterItem` has `arg` but no `description`, so `description` presence can only come from the raw
/// document. ADR 003 requires both, so both are observed rather than assumed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryShape {
    pub arg: Option<u32>,
    pub has_description: Option<bool>,
}

/// The observable read-side contract of the persistent `/config` form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigFormObs {
    /// Form field names actually present, e.g. `profile`, `profile_name`.
    pub field_names: BTreeSet<String>,
    /// Whether the unnamed base `[default]` option is offered.
    pub offers_default: bool,
    /// `(value, title)` for every named profile, read from the form itself rather than reconstructed
    /// from the semantic profile parser.
    pub named_profiles: Vec<(String, String)>,
}

/// Project the `/config` page into an allowlist of observable facts.
///
/// An allowlisted projection rather than stored HTML: tag-level redaction left secrets in the element
/// text between the tags, and no amount of pattern-matching on a whole page is as safe as never keeping
/// the page.
pub fn project_config_form(page: &str) -> ConfigFormObs {
    // An allowlist, built by walking the document with quick-xml and keeping only three things: the
    // names of non-hidden form fields, whether the profile select offers `[default]`, and each named
    // option's value and label. Element TEXT is never kept except as an option label, and an option
    // label sits inside the select we are already allowlisting.
    //
    // The previous approach stored the page and redacted tags, which left `<password>SEKRET</password>`
    // as `<!-- redacted -->SEKRET<!-- redacted -->` — the secret survived between the tags. Nothing
    // that is not on this allowlist is retained at all, so there is nothing to leak.
    let sensitive_name = raw::is_sensitive;

    let mut obs = ConfigFormObs::default();
    let mut reader = quick_xml::Reader::from_str(page);
    reader.config_mut().check_end_names = false;
    let mut in_profile_select = false;
    let mut pending_option: Option<String> = None;

    loop {
        let event = reader.read_event();
        // Whether this element closes in the same event. `Start` parks an option's value in
        // `pending_option` for the `Text` handler to pair with a label; an `Empty` option has no text
        // node, so nothing ever consumed it — the next option overwrote it and the profile silently
        // disappeared, which the tier-1 differ then reported as a profile the daemon does not offer.
        // A false absence attributed to the daemon is exactly the class this boundary exists to
        // prevent, so the two events are distinguished rather than shared.
        let self_closing = matches!(event, Ok(quick_xml::events::Event::Empty(_)));
        match event {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_lowercase();
                let attrs: BTreeMap<String, String> = e
                    .attributes()
                    .filter_map(Result::ok)
                    .map(|a| {
                        (
                            String::from_utf8_lossy(a.key.as_ref()).to_lowercase(),
                            a.normalized_value(quick_xml::XmlVersion::Explicit1_0)
                                .map(|v| v.into_owned())
                                .unwrap_or_default(),
                        )
                    })
                    .collect();
                let name = attrs.get("name").cloned().unwrap_or_default();
                // Case-insensitive, matching `raw::is_hostile_tag`. Attribute *keys* are lowercased
                // above but values are not, so an exact compare let `type="Hidden"` past the drop and
                // recorded the field name. Two spellings of one privacy rule is how the guarantee ends
                // up depending on the daemon's capitalisation.
                let is_hidden = attrs
                    .get("type")
                    .map(|t| t.eq_ignore_ascii_case("hidden"))
                    .unwrap_or(false);

                match tag.as_str() {
                    "input" | "select" | "textarea" => {
                        // Hidden inputs and anything sensitively named are dropped entirely — not
                        // recorded and not redacted, because a redaction still admits the name existed.
                        if !name.is_empty() && !is_hidden && !sensitive_name(&name) {
                            obs.field_names.insert(name.clone());
                        }
                        in_profile_select = tag == "select" && name == "profile";
                    }
                    "option" if in_profile_select => {
                        let value = attrs.get("value").cloned().unwrap_or_default();
                        if value == "[default]" {
                            obs.offers_default = true;
                            pending_option = None;
                        } else if value.is_empty() {
                            // Nothing to record either way.
                        } else if self_closing {
                            // No text node is coming, so record it now with its value as its label —
                            // the same fallback the `Text` handler applies to an empty label.
                            obs.named_profiles.push((value.clone(), value));
                            pending_option = None;
                        } else {
                            pending_option = Some(value);
                        }
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Text(t)) => {
                // The only element text kept anywhere: an option's label, inside the profile select.
                if let Some(value) = pending_option.take() {
                    let label = t
                        .xml10_content()
                        .ok()
                        .and_then(|decoded| {
                            quick_xml::escape::unescape(&decoded)
                                .ok()
                                .map(|value| value.trim().to_string())
                        })
                        .unwrap_or_default();
                    let title = if label.is_empty() {
                        value.clone()
                    } else {
                        label
                    };
                    obs.named_profiles.push((value, title));
                }
            }
            Ok(quick_xml::events::Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_lowercase();
                // `<option value="Night"></option>` carries no text node either, so the `Text` handler
                // never ran and the value is still parked. Flushing it here is the other half of the
                // self-closing case: unconditionally clearing `pending_option` on any end tag is what
                // dropped it, and a dropped profile reads as one the daemon does not offer.
                if tag == "option" {
                    if let Some(value) = pending_option.take() {
                        obs.named_profiles.push((value.clone(), value));
                    }
                }
                if tag == "select" {
                    in_profile_select = false;
                }
                // Any other end tag closes whatever context an option's text could have belonged to,
                // so a value still parked at that point has no label coming and no element to own it.
                pending_option = None;
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }
    obs
}

/// ADR 003's claim rows, rendered as data. `REQUIRED_CLAIMS` *is* this list, and
/// `required_claims_faithfully_renders_every_adr_003_row` asserts the two cannot drift.
pub const ADR_CLAIM_ROWS: [&str; 15] = [
    "getinfo",
    "state",
    "status",
    "status_metadata_child",
    "volume_range",
    "modes",
    "filters",
    "filters_arg",
    "filters_description",
    "shapers",
    "shapers_shape",
    "rates",
    "junkfilters",
    "matrix",
    "config_form",
];

/// What the daemon said, in the shape the corpus can be compared against.
#[derive(Debug, Clone, Default)]
pub struct Capture {
    pub identity: BTreeMap<String, String>,
    /// `family stem -> entries`, e.g. `"modes" -> [...]`. Enumeration families only.
    pub enumerations: BTreeMap<String, Vec<EnumEntry>>,
    /// Scalar families rendered as attribute maps, e.g. `"state"`, `"volume_range"`.
    pub scalars: BTreeMap<String, BTreeMap<String, String>>,
    /// The mode the daemon was already in, as a list index. The enumerations above are its lists.
    pub active_mode_index: u32,
    pub active_mode_name: String,
    /// Whether a `Status` document carried the optional self-closing `metadata` child, **observed**
    /// from the raw document rather than inferred from any value.
    pub status_metadata_child: MetadataChild,
    /// Per-family shape facts, keyed `family/name`, for claims the semantic types cannot carry.
    pub entry_shapes: BTreeMap<String, EntryShape>,
    /// Sanitized raw documents, per family, as the evidence each shape diff actually used. Host and
    /// credential material never reach this map.
    pub raw_documents: BTreeMap<String, String>,
    /// The `/config` read-side contract, when credentials were supplied.
    pub config_form: Option<ConfigFormObs>,
    /// Whether web credentials were available for this run at all.
    ///
    /// Recorded because `config_form: None` conflates two different facts: a run that was never
    /// entitled to look (an accepted limit) and a run that looked and failed (an unverified required
    /// claim). Without this the differ cannot tell them apart and reads the second as the first.
    pub has_web_credentials: bool,
    /// `(value, title)` from the persistent HTTP lane's `/config` profile list. `None` when no
    /// credentials were supplied — recorded as not-captured rather than as an empty truth.
    pub config_profiles: Option<Vec<(String, String)>>,
    /// The same list as the *production semantic parser* reports it, kept only to cross-check the raw
    /// projection above. It is never allowed to stand in as the evidence: tier 1 reads the raw lane so
    /// that a parser defect shows up as a disagreement instead of being reproduced in the record.
    pub config_profiles_semantic: Option<Vec<(String, String)>>,
    /// How long each family took to deliver, so `HqpTimeouts::response` can be set from evidence.
    pub latencies: BTreeMap<String, Duration>,
    /// Unsolicited documents the client skipped during the capture.
    pub unsolicited_skipped: u32,
    /// The matrix profile the daemon reports as current, read-only via `MatrixGetProfile`.
    pub current_matrix_profile: Option<(u32, String)>,
    /// Set when `MatrixGetProfile` could not be read at all, as distinct from a daemon that
    /// legitimately reports no current selection. `None` alone cannot tell those apart.
    pub matrix_current_read_failed: bool,
    /// The whole-command deadline in force during the capture, so delivery times can be judged
    /// against the budget that actually applied rather than against a remembered default.
    pub response_deadline: Duration,
}

/// One way the daemon and the corpus disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub family: String,
    pub kind: DivergenceKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergenceKind {
    /// The corpus claims an entry the daemon does not have, or vice versa.
    MissingEntry,
    /// Both have the name, but at different list positions. The domain `State`/`Set*` speak.
    IndexMismatch,
    /// Both have the name, but with different enum IDs. The domain `hqplayerd.xml` speaks.
    EnumIdMismatch,
    /// A scalar attribute the corpus expects is absent, or present when it should not be.
    AttributePresence,
    /// Identity fields differ, e.g. a different daemon version than the corpus profile claims.
    Identity,
    /// Two lanes that should agree do not — e.g. the raw `/config` projection and the production
    /// semantic parser naming different profiles. The daemon is not at fault here; the client is.
    LaneDisagreement,
}

/// The outcome of a tier-1 run.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub profile: String,
    pub divergences: Vec<Divergence>,
    pub capture: Capture,
    /// Families the run could not reach, with the reason. Honest partial coverage.
    pub not_captured: Vec<String>,
    /// Per-family verdict: did this family deliver inside the configured whole-command deadline?
    /// Latency on its own does not answer the question the deadline poses.
    pub within_deadline: BTreeMap<String, bool>,
    /// True only when every captured family delivered inside the deadline.
    pub overall_within_deadline: bool,
    /// Every claim this run actually compared. The point of recording it is that "clean" must not be
    /// able to mean "never checked" — a family missing from here is an unverified claim, not a pass.
    pub checked: BTreeSet<String>,
    /// Required claims that could not be observed and are **not** one of the deliberately accepted
    /// limits. Any entry here withholds a merge-gate pass.
    pub unverified: Vec<String>,
}

/// Every claim ADR 003 requires a tier-1 run to compare. Held as data so a family cannot be forgotten
/// silently: the differ is asserted against this list, not against whatever it happens to walk.
pub const REQUIRED_CLAIMS: [&str; 15] = ADR_CLAIM_ROWS;

/// Attributes `State` must carry. Values are instance-specific; presence is the contract.
pub const STATE_REQUIRED: [&str; 17] = [
    "state",
    "mode",
    "filter",
    "filter1x",
    "filterNx",
    "shaper",
    "rate",
    "volume",
    "active_mode",
    "active_rate",
    "invert",
    "convolution",
    "repeat",
    "random",
    "adaptive",
    "filter_junk",
    "matrix_profile",
];

/// Attributes `Status` must carry.
pub const STATUS_REQUIRED: [&str; 14] = [
    "state",
    "track",
    "track_id",
    "position",
    "length",
    "volume",
    "active_mode",
    "active_filter",
    "active_shaper",
    "active_rate",
    "active_bits",
    "active_channels",
    "samplerate",
    "bitrate",
];

/// Attributes `VolumeRange` must carry. `step` is legitimately optional — the verified live sample
/// omits it — so it is not required, but its presence is recorded either way.
pub const VOLUME_RANGE_REQUIRED: [&str; 4] = ["min", "max", "enabled", "adaptive"];

/// Attributes `GetInfo` must carry. `name` and `platform` are instance-specific in value but
/// contractually present, so presence is checked and value is not.
pub const GETINFO_REQUIRED: [&str; 5] = ["name", "product", "version", "platform", "engine"];

/// Form fields the `/config` read side must expose.
pub const CONFIG_FORM_REQUIRED: [&str; 2] = ["profile", "profile_name"];

/// Claims a read-only run is allowed not to reach, with the reason recorded in the report.
pub const ACCEPTED_LIMITS: [&str; 2] = ["inactive_mode_lists", "config_read_side_no_credentials"];

impl Report {
    pub fn is_clean(&self) -> bool {
        self.divergences.is_empty()
    }

    /// A merge-gate pass needs more than an empty divergence list: every required claim must have
    /// actually been compared, and nothing required may be left unverified.
    ///
    /// This is the distinction the earlier version could not express — "clean" meant "found nothing",
    /// which a differ that checked nothing also satisfies.
    pub fn merge_gate_pass(&self) -> bool {
        self.divergences.is_empty()
            && self.unverified.is_empty()
            && REQUIRED_CLAIMS
                .iter()
                .all(|claim| self.checked.contains(*claim))
    }

    /// A human-readable summary for the operator running the gate.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "tier-1 read-only verification against `{}`",
            self.profile
        );
        let _ = writeln!(
            out,
            "  daemon: {}",
            self.capture
                .identity
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let _ = writeln!(
            out,
            "  active mode: index {} ({}) — the enumerations below are THIS mode's lists only",
            self.capture.active_mode_index, self.capture.active_mode_name
        );
        let _ = writeln!(
            out,
            "  status metadata child: {}",
            match self.capture.status_metadata_child {
                MetadataChild::Present => "present",
                MetadataChild::Absent => "absent (no track loaded?)",
                MetadataChild::Unobserved => "UNOBSERVED - not a satisfied claim",
            }
        );
        let _ = writeln!(
            out,
            "  unsolicited documents skipped: {}",
            self.capture.unsolicited_skipped
        );
        let _ = writeln!(
            out,
            "  container delivery (evidence for HqpTimeouts::response):"
        );
        let mut worst = Duration::ZERO;
        for (family, took) in &self.capture.latencies {
            if *took > worst {
                worst = *took;
            }
            let _ = writeln!(out, "    {family:<16} {:>8.1?}", took);
        }
        let _ = writeln!(out, "    slowest family   {worst:>8.1?}");
        let _ = writeln!(
            out,
            "    configured whole-command deadline {:?} -> overall {}",
            self.capture.response_deadline,
            if self.overall_within_deadline {
                "WITHIN"
            } else {
                "EXCEEDED"
            }
        );
        if !self.overall_within_deadline {
            let over: Vec<&String> = self
                .within_deadline
                .iter()
                .filter(|(_, ok)| !**ok)
                .map(|(f, _)| f)
                .collect();
            let _ = writeln!(
                out,
                "    EXCEEDED by {over:?} — HqpTimeouts::response is too small for this daemon and \
                 should be raised from this evidence, not inherited"
            );
        }
        let _ = writeln!(
            out,
            "  current matrix profile: {}",
            self.capture
                .current_matrix_profile
                .as_ref()
                .map(|(i, n)| format!("index {i} ({n})"))
                .unwrap_or_else(|| "none reported".to_string())
        );
        if !self.not_captured.is_empty() {
            let _ = writeln!(out, "  NOT captured (partial coverage):");
            for n in &self.not_captured {
                let _ = writeln!(out, "    - {n}");
            }
        }
        if self.divergences.is_empty() {
            let _ = writeln!(out, "  divergences: none");
        } else {
            let _ = writeln!(out, "  divergences: {}", self.divergences.len());
            for d in &self.divergences {
                let _ = writeln!(out, "    [{:?}] {}: {}", d.kind, d.family, d.detail);
            }
        }
        out
    }
}

/// Stable identifier for the machine-readable artifact. Bump the version when the shape changes.
pub const ARTIFACT_SCHEMA: &str = "uhc-hqp-tier1/v1";

/// Markers bracketing the artifact in captured output, so a caller can lift the JSON out
/// deterministically rather than scraping the human render. Part of the contract; do not reword.
pub const ARTIFACT_BEGIN: &str = "----BEGIN uhc-hqp-tier1 ARTIFACT----";
pub const ARTIFACT_END: &str = "----END uhc-hqp-tier1 ARTIFACT----";

impl Report {
    /// The artifact wrapped in stable markers, so a caller can lift it out of captured stdout
    /// without scraping the human render. The markers are part of the contract.
    pub fn artifact_block(&self) -> String {
        format!(
            "{ARTIFACT_BEGIN}\n{}\n{ARTIFACT_END}",
            serde_json::to_string_pretty(&self.to_json())
                .unwrap_or_else(|e| format!("{{\"error\":\"serialisation failed: {e}\"}}"))
        )
    }

    /// Machine-readable artifact for CI to store and later runs to compare against.
    ///
    /// Deliberately carries **no host, user, password or any other connection secret** — only what
    /// the daemon said about itself and how the corpus compares. A verification artifact that leaks
    /// credentials is worse than no artifact.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": ARTIFACT_SCHEMA,
            "profile": self.profile,
            // Only what the daemon says about itself. No host, port, user or password: this artifact
            // is meant to be committed or attached to CI, and a leaked credential would outlive the run.
            "daemon": self.capture.identity,
            "active_mode": {
                "index": self.capture.active_mode_index,
                "name": self.capture.active_mode_name,
                "note": "enumerations below are this mode's lists only; the other mode needs SetMode (tier 2)",
            },
            "status_metadata_child": format!("{:?}", self.capture.status_metadata_child),
            "current_matrix_profile": self.capture.current_matrix_profile
                .as_ref()
                .map(|(i, n)| serde_json::json!({ "index": i, "name": n })),
            "unsolicited_skipped": self.capture.unsolicited_skipped,
            "response_deadline_ms": self.capture.response_deadline.as_millis(),
            "overall_within_deadline": self.overall_within_deadline,
            "families": self.capture.latencies.iter().map(|(name, took)| serde_json::json!({
                "name": name,
                "delivery_ms": took.as_millis(),
                "entries": self.capture.enumerations.get(name).map(|e| e.len()),
                "within_deadline": self.within_deadline.get(name).copied().unwrap_or(false),
            })).collect::<Vec<_>>(),
            // The normalized capture itself, so a later run can be compared against this one without
            // scraping the human render. Everything here came from the daemon; nothing here came from
            // the connection.
            //
            // Authority note, since some values also appear as a digest above: `capture` is the
            // normalized state and is what a later run should diff against. The top-level fields are a
            // convenience summary for a human or a CI dashboard, derived from exactly this data.
            "capture": {
                "enumerations": self.capture.enumerations.iter().map(|(family, entries)| {
                    let rows: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|e| serde_json::json!({
                            "index": e.index,
                            "name": e.name,
                            "enum_id": e.enum_id,
                            "rate": e.rate,
                        }))
                        .collect();
                    (family.clone(), serde_json::Value::Array(rows))
                }).collect::<serde_json::Map<String, serde_json::Value>>(),
                "scalars": self.capture.scalars,
                // Explicitly null when unreached, so absent-and-ambiguous is not a possible state.
                "config_profiles": self.capture.config_profiles,
                "config_profiles_semantic": self.capture.config_profiles_semantic,
                "matrix_current_read_failed": self.capture.matrix_current_read_failed,
                "status_metadata_child": format!("{:?}", self.capture.status_metadata_child),
                "entry_shapes": self.capture.entry_shapes.iter().map(|(k, v)| (k.clone(), serde_json::json!({
                    "arg": v.arg,
                    "has_description": v.has_description,
                }))).collect::<serde_json::Map<String, serde_json::Value>>(),
                "config_form": self.capture.config_form.as_ref().map(|f| serde_json::json!({
                    "field_names": f.field_names,
                    "offers_default": f.offers_default,
                    "named_profiles": f.named_profiles,
                })),
                // The evidence each shape diff actually used. Sanitised at capture time AND again
                // here: defence in depth, so a document that reached the map by any other route still
                // cannot carry a hidden input or token out.
                "raw_documents": self.capture.raw_documents.iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(raw::sanitize(v))))
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
            },
            "checked": self.checked,
            "unverified": self.unverified,
            "merge_gate_pass": self.merge_gate_pass(),
            "divergences": self.divergences.iter().map(|d| serde_json::json!({
                "family": d.family,
                "kind": format!("{:?}", d.kind),
                "detail": d.detail,
            })).collect::<Vec<_>>(),
            "not_captured": self.not_captured,
        })
    }
}

/// Families the corpus holds as enumerations, paired with their item tag.
const ENUM_FAMILIES: [(&str, &str, &str); 6] = [
    ("modes", "GetModes", "ModesItem"),
    ("filters", "GetFilters", "FiltersItem"),
    ("shapers", "GetShapers", "ShapersItem"),
    ("rates", "GetRates", "RatesItem"),
    ("junkfilters", "GetJunkFilters", "JunkFiltersItem"),
    ("matrix", "MatrixListProfiles", "MatrixProfile"),
];

/// Read every family this tier is allowed to touch.
///
/// Every call here is a query. There is no `Set*` on this path, by construction.
pub async fn capture(adapter: &HqpAdapter) -> anyhow::Result<Capture> {
    let mut c = Capture::default();
    let skipped_before = adapter.unsolicited_skipped().await;

    let timed = |name: &str, took: Duration, c: &mut Capture| {
        c.latencies.insert(name.to_string(), took);
    };

    let t = Instant::now();
    let info = adapter.get_info().await?;
    timed("getinfo", t.elapsed(), &mut c);
    for (k, v) in [
        ("name", info.name),
        ("product", info.product),
        ("version", info.version),
        ("platform", info.platform),
        ("engine", info.engine),
    ] {
        c.identity.insert(k.to_string(), v);
    }

    let t = Instant::now();
    let state = adapter.get_state().await?;
    timed("state", t.elapsed(), &mut c);
    let mut state_attrs = BTreeMap::new();
    state_attrs.insert("state".into(), state.state.to_string());
    state_attrs.insert("active_mode".into(), state.active_mode.to_string());
    state_attrs.insert("active_rate".into(), state.active_rate.to_string());
    state_attrs.insert("invert".into(), state.invert.to_string());
    state_attrs.insert("convolution".into(), state.convolution.to_string());
    state_attrs.insert("repeat".into(), state.repeat.to_string());
    state_attrs.insert("random".into(), state.random.to_string());
    state_attrs.insert("adaptive".into(), state.adaptive.to_string());
    state_attrs.insert("mode".into(), state.mode.to_string());
    state_attrs.insert("filter".into(), state.filter.to_string());
    state_attrs.insert(
        "filter1x".into(),
        state.filter1x.map(|v| v.to_string()).unwrap_or_default(),
    );
    state_attrs.insert(
        "filterNx".into(),
        state.filter_nx.map(|v| v.to_string()).unwrap_or_default(),
    );
    state_attrs.insert("shaper".into(), state.shaper.to_string());
    state_attrs.insert("rate".into(), state.rate.to_string());
    state_attrs.insert("volume".into(), format!("{}", state.volume_db));
    state_attrs.insert("filter_junk".into(), state.filter_junk.to_string());
    state_attrs.insert("matrix_profile".into(), state.matrix_profile.clone());
    c.scalars.insert("state".into(), state_attrs);
    c.active_mode_index = u32::from(state.mode);

    let t = Instant::now();
    let status = adapter.get_playback_status().await?;
    timed("status", t.elapsed(), &mut c);
    let mut status_attrs = BTreeMap::new();
    status_attrs.insert("state".into(), status.state.to_string());
    status_attrs.insert("track".into(), status.track.to_string());
    status_attrs.insert("track_id".into(), status.track_id.clone());
    status_attrs.insert("position".into(), status.position.to_string());
    status_attrs.insert("length".into(), status.length.to_string());
    status_attrs.insert("active_rate".into(), status.active_rate.to_string());
    status_attrs.insert("active_bits".into(), status.active_bits.to_string());
    status_attrs.insert("active_channels".into(), status.active_channels.to_string());
    status_attrs.insert("samplerate".into(), status.samplerate.to_string());
    status_attrs.insert("bitrate".into(), status.bitrate.to_string());
    status_attrs.insert("active_filter".into(), status.active_filter.clone());
    status_attrs.insert("active_shaper".into(), status.active_shaper.clone());
    status_attrs.insert("active_mode".into(), status.active_mode.clone());
    status_attrs.insert("volume".into(), format!("{}", status.volume_db));
    c.scalars.insert("status".into(), status_attrs);
    // A metadata child is the only way samplerate/bits reach Status, so their presence is the proxy.
    // Presence of the metadata child, and the filter/shaper wire-shape claims, are observed from raw
    // documents. Read-only: the lane refuses anything that is not a query.
    let conn = adapter.get_status().await;
    if let Some(host) = conn.host.clone() {
        let deadline = adapter.timeouts().await.response;
        // Every native family, read off the socket through the closed query set. Nothing here is
        // reconstructed from semantically parsed values: the diffs compare what the wire actually said.
        // Iterates Query::ALL rather than a hand-typed list, so a twelfth variant is observed without
        // anyone remembering to add it here.
        for query in raw::Query::ALL {
            let family = query.capture_family();
            match raw::observe(&host, conn.port, query, deadline).await {
                Ok(document) => {
                    if query == raw::Query::Status {
                        c.status_metadata_child = if raw::has_child(&document, "metadata") {
                            MetadataChild::Present
                        } else {
                            MetadataChild::Absent
                        };
                    }
                    if matches!(query, raw::Query::GetFilters | raw::Query::GetShapers) {
                        let item = if query == raw::Query::GetFilters {
                            "FiltersItem"
                        } else {
                            "ShapersItem"
                        };
                        for attrs in raw::child_attrs(&document, item) {
                            let map: BTreeMap<&str, &str> = attrs
                                .iter()
                                .map(|(k, v)| (k.as_str(), v.as_str()))
                                .collect();
                            if let Some(name) = map.get("name") {
                                c.entry_shapes.insert(
                                    format!("{family}/{name}"),
                                    EntryShape {
                                        arg: map.get("arg").and_then(|a| a.parse().ok()),
                                        has_description: Some(map.contains_key("description")),
                                    },
                                );
                            }
                        }
                    }
                    // Sanitised before storage, so the sanitiser is not something a caller can forget.
                    // Never carries the host it came from.
                    c.raw_documents
                        .insert(family.to_string(), raw::sanitize(&document));
                }
                Err(e) => tracing::warn!("tier-1 raw lane: {} unobserved: {e}", query.element()),
            }
        }
    }

    let t = Instant::now();
    let vr = adapter.get_volume_range().await?;
    timed("volume_range", t.elapsed(), &mut c);
    let mut vr_attrs = BTreeMap::new();
    vr_attrs.insert("min".into(), format!("{}", vr.min_db));
    vr_attrs.insert("max".into(), format!("{}", vr.max_db));
    vr_attrs.insert(
        "step".into(),
        vr.step_db.map(|s| format!("{s}")).unwrap_or_default(),
    );
    vr_attrs.insert("enabled".into(), vr.enabled.to_string());
    vr_attrs.insert("adaptive".into(), vr.adaptive.to_string());
    c.scalars.insert("volume_range".into(), vr_attrs);

    let t = Instant::now();
    let modes = adapter.get_modes().await?;
    timed("modes", t.elapsed(), &mut c);
    c.active_mode_name = modes
        .iter()
        .find(|m| m.index == c.active_mode_index)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| format!("unknown({})", c.active_mode_index));
    c.enumerations.insert(
        "modes".into(),
        modes
            .into_iter()
            .map(|m| EnumEntry {
                index: m.index,
                name: m.name,
                enum_id: Some(m.value),
                rate: None,
                arg: None,
                description: None,
            })
            .collect(),
    );

    let t = Instant::now();
    let filters = adapter.get_filters().await?;
    timed("filters", t.elapsed(), &mut c);
    c.enumerations.insert(
        "filters".into(),
        filters
            .into_iter()
            .map(|f| EnumEntry {
                index: f.index,
                name: f.name,
                enum_id: Some(f.value),
                rate: None,
                arg: Some(f.arg),
                description: None,
            })
            .collect(),
    );

    let t = Instant::now();
    let shapers = adapter.get_shapers().await?;
    timed("shapers", t.elapsed(), &mut c);
    c.enumerations.insert(
        "shapers".into(),
        shapers
            .into_iter()
            .map(|s| EnumEntry {
                index: s.index,
                name: s.name,
                enum_id: Some(s.value),
                rate: None,
                arg: None,
                description: None,
            })
            .collect(),
    );

    let t = Instant::now();
    let rates = adapter.get_rates().await?;
    timed("rates", t.elapsed(), &mut c);
    c.enumerations.insert(
        "rates".into(),
        rates
            .into_iter()
            .map(|r| EnumEntry {
                index: r.index,
                name: String::new(),
                enum_id: None,
                rate: Some(r.rate),
                arg: None,
                description: None,
            })
            .collect(),
    );

    let t = Instant::now();
    let junk = adapter.get_junk_filters().await?;
    timed("junkfilters", t.elapsed(), &mut c);
    c.enumerations.insert(
        "junkfilters".into(),
        junk.into_iter()
            .map(|j| EnumEntry {
                index: j.index,
                name: j.name,
                enum_id: Some(j.value),
                rate: None,
                arg: None,
                description: None,
            })
            .collect(),
    );

    let t = Instant::now();
    let matrix = adapter.get_matrix_profiles().await?;
    timed("matrix", t.elapsed(), &mut c);
    c.enumerations.insert(
        "matrix".into(),
        matrix
            .into_iter()
            .map(|p| EnumEntry {
                index: p.index,
                name: p.name,
                enum_id: None,
                rate: None,
                arg: None,
                description: None,
            })
            .collect(),
    );

    // Read-only: what `MatrixGetProfile` reports, captured as an observation. Deliberately the raw
    // command rather than `get_matrix_profile()`, which since #347 answers from
    // `State.matrix_profile` — diffing that field against itself would prove nothing, and settling
    // whether the command agrees with `State` is exactly what this lane is for.
    let t = Instant::now();
    match adapter.read_matrix_get_profile().await {
        Ok(Some(p)) => {
            timed("matrix_current", t.elapsed(), &mut c);
            c.current_matrix_profile = Some((p.index, p.name));
        }
        Ok(None) => {
            timed("matrix_current", t.elapsed(), &mut c);
            c.current_matrix_profile = None;
        }
        Err(e) => {
            // A read failure and "no current selection" are different facts, and None cannot tell
            // them apart. The error text is logged but deliberately not stored in the capture: it can
            // contain the address the client was talking to, and the artifact must stay free of that.
            tracing::warn!("tier-1: MatrixGetProfile unavailable: {e}");
            c.matrix_current_read_failed = true;
            c.current_matrix_profile = None;
        }
    }

    c.response_deadline = adapter.timeouts().await.response;

    // Persistent HTTP lane, read side only. Attempted just when credentials were supplied; a failure
    // is recorded as not-captured rather than swallowed, because "no profiles" and "could not ask"
    // are different facts.
    c.has_web_credentials = adapter.has_web_credentials().await;
    if c.has_web_credentials {
        // Read-side shape via an allowlisted projection. The page itself never enters the capture:
        // tag-level sanitisation left secrets in the element text between the tags, so the only safe
        // handling is to keep the three facts we need and discard everything else.
        let t = Instant::now();
        match adapter.fetch_config_page_raw().await {
            Ok(page) => {
                timed("config_profiles", t.elapsed(), &mut c);
                let obs = project_config_form(&page);
                // The raw lane is the evidence. Both facts come from the same projection, so the
                // record cannot end up half raw and half reconstructed.
                c.config_profiles = Some(obs.named_profiles.clone());
                c.config_form = Some(obs);
            }
            Err(e) => tracing::warn!("tier-1: /config read side unavailable: {e}"),
        }

        // Cross-check only. The production parser reads a different path (`/config/profile/load`), so
        // a disagreement is a real finding about the client and is reported as one below.
        let t = Instant::now();
        match adapter.fetch_profiles().await {
            Ok(profiles) => {
                timed("config_profiles_semantic", t.elapsed(), &mut c);
                c.config_profiles_semantic =
                    Some(profiles.into_iter().map(|p| (p.value, p.title)).collect());
            }
            Err(e) => tracing::warn!("tier-1: semantic profile cross-check unavailable: {e}"),
        }
    }

    c.unsolicited_skipped = adapter
        .unsolicited_skipped()
        .await
        .saturating_sub(skipped_before);

    Ok(c)
}

/// Compare a capture against a corpus profile.
///
/// Enumerations are matched **by name**, because a name is the only stable handle across versions;
/// the index and enum ID are then the things under test. That is the same rule the client itself
/// follows, so a divergence here is exactly a divergence the client would act on.
pub fn diff(capture: &Capture, profile: &str) -> Report {
    let mut report = Report {
        profile: profile.to_string(),
        capture: capture.clone(),
        ..Report::default()
    };

    report.checked.insert("getinfo".into());
    for key in GETINFO_REQUIRED {
        if !capture.identity.contains_key(key) {
            report.divergences.push(Divergence {
                family: "getinfo".into(),
                kind: DivergenceKind::AttributePresence,
                detail: format!(
                    "GetInfo is missing {key:?}; its value is instance-specific but its presence is \
                     part of the contract"
                ),
            });
        }
    }

    // Scalar shape contracts. There are no corpus fixtures for these families, so what is compared is
    // the required attribute SET - values are instance-specific and comparing them would produce noise.
    for (family, required) in [
        ("state", STATE_REQUIRED.as_slice()),
        ("status", STATUS_REQUIRED.as_slice()),
        ("volume_range", VOLUME_RANGE_REQUIRED.as_slice()),
    ] {
        match capture.scalars.get(family) {
            None => report.unverified.push(format!(
                "{family} - not captured, so its shape was never compared"
            )),
            Some(observed) => {
                report.checked.insert(family.to_string());
                for key in required {
                    if !observed.contains_key(*key) {
                        report.divergences.push(Divergence {
                            family: family.into(),
                            kind: DivergenceKind::AttributePresence,
                            detail: format!("{family} is missing required attribute {key:?}"),
                        });
                    }
                }
            }
        }
    }

    // The metadata child is a structural claim; Unobserved is not a pass.
    match capture.status_metadata_child {
        MetadataChild::Unobserved => report.unverified.push(
            "status_metadata_child - never observed; presence must be read from the raw document, \
             never inferred from a value that has a legitimate zero"
                .into(),
        ),
        MetadataChild::Present | MetadataChild::Absent => {
            report.checked.insert("status_metadata_child".into());
        }
    }

    // Filter arg flags and description presence, per ADR 003.
    // `FiltersItem` carries `description`; `ShapersItem` does not. Requiring it of both - as an
    // earlier revision of the ADR row did - made a correct daemon look wrong, so each family is now
    // checked against its own documented shape.
    for (prefix, claim, expect_description) in [
        ("filters/", "filters_description", true),
        ("shapers/", "shapers_shape", false),
    ] {
        if capture.entry_shapes.keys().any(|k| k.starts_with(prefix)) {
            report.checked.insert(claim.into());
            for (key, shape) in &capture.entry_shapes {
                if !key.starts_with(prefix) {
                    continue;
                }
                match (expect_description, shape.has_description) {
                    (true, Some(true)) | (false, Some(false)) => {}
                    (true, _) => report.divergences.push(Divergence {
                        family: claim.into(),
                        kind: DivergenceKind::AttributePresence,
                        detail: format!("{key} carries no description attribute"),
                    }),
                    (false, _) => report.divergences.push(Divergence {
                        family: claim.into(),
                        kind: DivergenceKind::AttributePresence,
                        detail: format!(
                            "{key} carries a description attribute; ShapersItem is documented as \
                             index/name/value only, so this is a wire shape the reference does not \
                             describe"
                        ),
                    }),
                }
            }
        } else {
            report.unverified.push(format!(
                "{claim} - the raw lane observed no shapes for {prefix}"
            ));
        }
    }

    if capture
        .entry_shapes
        .keys()
        .any(|k| k.starts_with("filters/"))
    {
        report.checked.insert("filters_arg".into());
        // Only filters declare `arg`; shapers do not, so requiring it of them would manufacture noise.
        for (key, shape) in &capture.entry_shapes {
            if key.starts_with("filters/") && shape.arg.is_none() {
                report.divergences.push(Divergence {
                    family: "filters_arg".into(),
                    kind: DivergenceKind::AttributePresence,
                    detail: format!("{key} carries no arg attribute"),
                });
            }
        }
    } else {
        report.unverified.push(
            "filters_arg - the raw lane observed no filter shapes, so arg flags and description \
             presence were not compared"
                .into(),
        );
    }

    // Identity: the corpus profile claims a daemon. If we are looking at a different one, every
    // other comparison is against the wrong baseline and should be read that way.
    let expected_identity = corpus::document(profile, "getinfo");
    for key in ["product", "version", "engine"] {
        let want = attr_of(&expected_identity, key);
        let got = capture.identity.get(key).cloned().unwrap_or_default();
        if let Some(want) = want {
            if want != got {
                report.divergences.push(Divergence {
                    family: "getinfo".into(),
                    kind: DivergenceKind::Identity,
                    detail: format!("{key}: corpus says {want:?}, daemon says {got:?}"),
                });
            }
        }
    }

    for (stem, _family, item_tag) in ENUM_FAMILIES {
        let Some(observed) = capture.enumerations.get(stem) else {
            report.not_captured.push(format!("{stem} (not in capture)"));
            continue;
        };
        // Filters, shapers and rates are mode-relative. Under a configured `[source]` mode (index 0)
        // the loaded chain is decided by the material the source feeds, not by the configured mode, so
        // it can be PCM or SDM and the corpus has no document that is correct to diff against. The old
        // `_` arm fell to the PCM corpus for *any* non-SDM index, which meant a `[source]` daemon
        // serving the SDM chain was compared against PCM and every entry read as a divergence — a
        // false absence manufactured by the differ. Record the family not-captured and the claim
        // unverified, with the reason, rather than compare a guessed chain. `matrix` is mode-
        // independent and still compares.
        if matches!(stem, "filters" | "shapers" | "rates") && capture.active_mode_index == 0 {
            report.not_captured.push(format!(
                "{stem} — daemon is in [source] mode (index 0); the loaded chain (PCM or SDM) is \
                 material-dependent and not identified by the configured mode, so no mode-relative \
                 corpus is correct to diff against"
            ));
            report.unverified.push(format!(
                "{stem} — not compared under [source]; the loaded chain is unknown here and reaching a \
                 known chain needs SetMode (tier 2)"
            ));
            continue;
        }
        // For an explicit mode the configured mode *is* the chain: index 2 is SDM, index 1 is PCM.
        let doc_stem = match (stem, capture.active_mode_index) {
            ("filters", 2) => "filters_sdm".to_string(),
            ("filters", _) => "filters_pcm".to_string(),
            ("shapers", 2) => "shapers_sdm".to_string(),
            ("shapers", _) => "shapers_pcm".to_string(),
            ("rates", 2) => "rates_sdm".to_string(),
            ("rates", _) => "rates_pcm".to_string(),
            ("matrix", _) => "matrix_profiles".to_string(),
            (other, _) => other.to_string(),
        };
        // Corpus documents hold attribute values in escaped wire form; the client returns them
        // decoded. Compare in the decoded domain, using the very function the client uses, or every
        // name carrying an entity - a matrix profile called "Rock & Roll", say - reads as a
        // divergence on both sides at once.
        let expected: Vec<EnumEntry> =
            corpus::enum_entries(&corpus::document(profile, &doc_stem), item_tag)
                .into_iter()
                .map(|mut e| {
                    e.name = framing::decode_entities(&e.name);
                    e
                })
                .collect();

        report.checked.insert(stem.to_string());
        if stem == "rates" {
            diff_rates(&mut report, stem, &expected, observed);
            continue;
        }

        for want in &expected {
            match observed.iter().find(|o| o.name == want.name) {
                None => report.divergences.push(Divergence {
                    family: doc_stem.clone(),
                    kind: DivergenceKind::MissingEntry,
                    detail: format!("corpus has {:?}, daemon does not", want.name),
                }),
                Some(got) => {
                    if got.index != want.index {
                        report.divergences.push(Divergence {
                            family: doc_stem.clone(),
                            kind: DivergenceKind::IndexMismatch,
                            detail: format!(
                                "{:?}: corpus index {}, daemon index {}",
                                want.name, want.index, got.index
                            ),
                        });
                    }
                    if want.enum_id.is_some() && got.enum_id != want.enum_id {
                        report.divergences.push(Divergence {
                            family: doc_stem.clone(),
                            kind: DivergenceKind::EnumIdMismatch,
                            detail: format!(
                                "{:?}: corpus enum ID {:?}, daemon enum ID {:?}",
                                want.name, want.enum_id, got.enum_id
                            ),
                        });
                    }
                }
            }
        }
        for got in observed {
            if !got.name.is_empty() && !expected.iter().any(|w| w.name == got.name) {
                report.divergences.push(Divergence {
                    family: doc_stem.clone(),
                    kind: DivergenceKind::MissingEntry,
                    detail: format!("daemon has {:?}, corpus does not", got.name),
                });
            }
        }
    }

    // Scalar shape. The corpus holds no `volume_range` fixture — the earlier ones were dropped when
    // the model became the source of those variants — so there is nothing here to diff against and
    // saying so is the only honest option. An earlier draft of this block pretended otherwise: it
    // read the `engine` attribute off `getinfo`, discarded it, and claimed in a comment that step
    // optionality was "asserted below", which it never was. A corpus/daemon disagreement about `step`
    // would have read as an accepted gap.
    if let Some(vr) = capture.scalars.get("volume_range") {
        let step = vr.get("step").cloned().unwrap_or_default();
        report.not_captured.push(format!(
            "volume_range shape — no corpus fixture to diff against; daemon reported step={:?}, \
             min={:?}, max={:?} for the record",
            if step.is_empty() {
                "<absent>".to_string()
            } else {
                step
            },
            vr.get("min").cloned().unwrap_or_default(),
            vr.get("max").cloned().unwrap_or_default(),
        ));
    }

    // Branching on the recorded fact rather than on absence. `config_form: None` cannot distinguish a
    // run with no credentials from a credentialed run whose reads both failed — both `/config` reads only
    // `warn!` — so the accepted-limit branch fired for the failure case too and marked a required claim
    // *checked*. `Report::checked` exists precisely so that "clean" cannot mean "never looked".
    if !capture.has_web_credentials {
        // No credentials: a declared accepted limit, recorded in not_captured rather than as an
        // unverified required claim.
        report.checked.insert("config_form".into());
    } else if capture.config_form.is_none() {
        // Credentials were accepted but the form was never observed. That is a required claim left
        // unverified, and collapsing it into the accepted limit would make an absent credential and a
        // failed observation look alike.
        report.unverified.push(
            "config_form - credentials were available but the read side was not observed".into(),
        );
    }

    if let Some(form) = &capture.config_form {
        report.checked.insert("config_form".into());
        for field in CONFIG_FORM_REQUIRED {
            if !form.field_names.contains(field) {
                report.divergences.push(Divergence {
                    family: "config_form".into(),
                    kind: DivergenceKind::AttributePresence,
                    detail: format!("the /config read side is missing form field {field:?}"),
                });
            }
        }
        if !form.offers_default {
            report.divergences.push(Divergence {
                family: "config_form".into(),
                kind: DivergenceKind::AttributePresence,
                detail: "the /config profile select does not offer the unnamed base \"[default]\"; \
                         the named-versus-default distinction is load-bearing, since loading a NAMED \
                         profile restarts the daemon and empties /backup/settings.zip"
                    .into(),
            });
        }
    }

    // The two lanes read different paths for the same fact. Disagreement is a client finding, so it is
    // surfaced rather than silently resolved in favour of either side.
    if let (Some(raw_lane), Some(semantic)) =
        (&capture.config_profiles, &capture.config_profiles_semantic)
    {
        let mut raw_values: Vec<&str> = raw_lane.iter().map(|(v, _)| v.as_str()).collect();
        let mut semantic_values: Vec<&str> = semantic.iter().map(|(v, _)| v.as_str()).collect();
        raw_values.sort_unstable();
        semantic_values.sort_unstable();
        if raw_values != semantic_values {
            report.divergences.push(Divergence {
                family: "config_profiles".into(),
                kind: DivergenceKind::LaneDisagreement,
                detail: format!(
                    "the raw /config projection names {raw_values:?} but the production parser \
                     reports {semantic_values:?}; the profile list must not depend on which lane \
                     read it"
                ),
            });
        }
    }

    match &capture.config_profiles {
        None => report.not_captured.push(
            "config read side (/config profile list) — no web credentials supplied, or the request \
             failed, so the persistent lane was not reached"
                .into(),
        ),
        Some(observed) => {
            // The corpus form is an excerpt, so only entries it names are compared; a daemon with
            // extra profiles is normal, a daemon missing one the corpus claims is not.
            let form = corpus::document(profile, "config_profile_form");
            // Parsed out of the form rather than hardcoded, so a corpus that gains a profile does not
            // silently stop being checked. `[default]` is the unnamed base, not a named profile.
            // Parsed with quick-xml for the same reason `attr_of` is: splitting on `value="` assumes one
            // quote style and would read nothing at all from `value='Night'`, dropping the comparison
            // without reporting anything.
            let names: Vec<String> = raw::child_attrs(&form, "option")
                .into_iter()
                .filter_map(|attrs| {
                    attrs
                        .into_iter()
                        .find(|(k, _)| k == "value")
                        .map(|(_, v)| v)
                })
                .filter(|n| !n.is_empty() && n != "[default]" && n != "Apply")
                .collect();
            for name in &names {
                if !observed.iter().any(|(v, _)| v == name) {
                    report.divergences.push(Divergence {
                        family: "config_profile_form".into(),
                        kind: DivergenceKind::MissingEntry,
                        detail: format!("corpus form lists profile {name:?}, daemon does not"),
                    });
                }
            }
        }
    }

    // The current selection has to be coherent with the daemon's own two views of it.
    if capture.matrix_current_read_failed {
        report.not_captured.push(
            "MatrixGetProfile — read failed, so the current selection is unknown. Distinct from a \
             daemon reporting no selection; the failure detail is logged, not stored, because it can \
             carry the daemon address"
                .into(),
        );
    } else if let Some((current_index, current)) = &capture.current_matrix_profile {
        // Bound by the pattern rather than re-read through `map`/`unwrap_or`: the second read had a
        // `(0, "")` default that could never be taken here, and a default that cannot happen is a
        // default nobody checks if the code around it moves.
        let current_index = *current_index;
        // The pair must match, not just the name: MatrixGetProfile reports an index too, and the right
        // name at the wrong index is still an incoherent daemon.
        let listed = capture
            .enumerations
            .get("matrix")
            .map(|list| {
                list.iter()
                    .any(|e| &e.name == current && e.index == current_index)
            })
            .unwrap_or(false);
        if !listed {
            report.divergences.push(Divergence {
                family: "matrix_current".into(),
                kind: DivergenceKind::MissingEntry,
                detail: format!(
                    "MatrixGetProfile reports ({current_index}, {current:?}) as current, but that \
                     index/name pair is absent from this daemon's own MatrixListProfiles"
                ),
            });
        }
        let from_state = capture
            .scalars
            .get("state")
            .and_then(|m| m.get("matrix_profile"))
            .cloned()
            .unwrap_or_default();
        if !from_state.is_empty() && &from_state != current {
            report.divergences.push(Divergence {
                family: "matrix_current".into(),
                kind: DivergenceKind::AttributePresence,
                detail: format!(
                    "the daemon's two views of the current matrix profile disagree: \
                     MatrixGetProfile says {current:?}, State.matrix_profile says {from_state:?}"
                ),
            });
        }
    }

    // Per-family and overall verdict against the budget that actually applied. Latency alone does
    // not answer the question the deadline poses.
    for (family, took) in &capture.latencies {
        report
            .within_deadline
            .insert(family.clone(), *took <= capture.response_deadline);
    }
    // Deliberately NOT pushed into `not_captured`: a family that delivered slowly was still
    // captured, and conflating "too slow" with "never reached" would mislead anyone reading the
    // artifact later. The structured verdicts above are the machine-readable signal; the operator
    // narrative lives in `render()`.
    report.overall_within_deadline = report.within_deadline.values().all(|ok| *ok);

    // The honest limit of a read-only run.
    let inactive = if capture.active_mode_index == 2 {
        "PCM"
    } else {
        "SDM"
    };
    report.not_captured.push(format!(
        "{inactive} filter/shaper/rate lists — reaching them needs SetMode, which is tier 2"
    ));

    report
}

fn diff_rates(report: &mut Report, stem: &str, expected: &[EnumEntry], observed: &[EnumEntry]) {
    // Index 0 is rate 0, meaning auto/source-based. A daemon reporting otherwise breaks every
    // Hz-to-index conversion that follows.
    match observed.iter().find(|e| e.index == 0).and_then(|e| e.rate) {
        Some(0) => {}
        other => report.divergences.push(Divergence {
            family: stem.into(),
            kind: DivergenceKind::IndexMismatch,
            detail: format!("index 0 must be rate 0 (auto/source-based); daemon reports {other:?}"),
        }),
    }
    // Reverse pass: without it a daemon-only mapping is silently accepted.
    for got in observed {
        if !expected.iter().any(|w| w.index == got.index) {
            report.divergences.push(Divergence {
                family: stem.into(),
                kind: DivergenceKind::MissingEntry,
                detail: format!(
                    "daemon has index {} -> {:?} Hz, corpus does not",
                    got.index, got.rate
                ),
            });
        }
    }
    for want in expected {
        match observed.iter().find(|o| o.index == want.index) {
            None => report.divergences.push(Divergence {
                family: stem.into(),
                kind: DivergenceKind::MissingEntry,
                detail: format!("corpus has index {} , daemon does not", want.index),
            }),
            Some(got) => {
                if got.rate != want.rate {
                    report.divergences.push(Divergence {
                        family: stem.into(),
                        kind: DivergenceKind::IndexMismatch,
                        detail: format!(
                            "index {}: corpus rate {:?} Hz, daemon rate {:?} Hz",
                            want.index, want.rate, got.rate
                        ),
                    });
                }
            }
        }
    }
}

/// One root attribute, parsed rather than scanned.
///
/// Built on [`raw::root_attrs`] because hand-rolled scanning is what `raw.rs` records as the cause of a
/// silent under-observation: it assumes double quotes and no whitespace around `=`, so `index = '0'`
/// yields nothing and the diff reports a false *absence*. Failing quietly is what makes it dangerous
/// here — the comparison disappears instead of erroring, and the report still reads clean.
pub fn attr_of(document: &str, key: &str) -> Option<String> {
    raw::root_attrs(document)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}
