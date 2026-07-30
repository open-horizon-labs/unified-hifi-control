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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use unified_hifi_control::adapters::hqplayer::{framing, HqpAdapter};

use super::corpus::{self, EnumEntry};

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
    /// Whether a `Status` document carried the optional self-closing `metadata` child.
    pub status_had_metadata_child: bool,
    /// Names from the persistent HTTP lane's `/config` profile list. Empty when no credentials were
    /// supplied — recorded as not-captured rather than as an empty truth.
    pub config_profiles: Option<Vec<String>>,
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
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.divergences.is_empty()
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
            if self.capture.status_had_metadata_child {
                "present"
            } else {
                "absent (no track loaded?)"
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
            "status_metadata_child": self.capture.status_had_metadata_child,
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
                "matrix_current_read_failed": self.capture.matrix_current_read_failed,
            },
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

    let mut timed = |name: &str, took: Duration, c: &mut Capture| {
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
    status_attrs.insert("active_filter".into(), status.active_filter.clone());
    status_attrs.insert("active_shaper".into(), status.active_shaper.clone());
    status_attrs.insert("active_mode".into(), status.active_mode.clone());
    status_attrs.insert("volume".into(), format!("{}", status.volume_db));
    c.scalars.insert("status".into(), status_attrs);
    // A metadata child is the only way samplerate/bits reach Status, so their presence is the proxy.
    c.status_had_metadata_child = status.samplerate > 0 || status.active_bits > 0;

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
            })
            .collect(),
    );

    // Read-only: MatrixGetProfile reports the current selection without changing it.
    let t = Instant::now();
    match adapter.get_matrix_profile().await {
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
    if adapter.has_web_credentials().await {
        let t = Instant::now();
        match adapter.fetch_profiles().await {
            Ok(profiles) => {
                timed("config_profiles", t.elapsed(), &mut c);
                c.config_profiles = Some(profiles.into_iter().map(|p| p.value).collect());
            }
            Err(e) => {
                c.config_profiles = None;
                tracing::warn!("tier-1: /config read side unavailable: {e}");
            }
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
        // Filters and shapers are mode-relative; compare against the profile document for the mode
        // the daemon was actually in.
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
            for name in form
                .split("value=\"")
                .skip(1)
                .filter_map(|part| part.split('"').next())
                .filter(|n| !n.is_empty() && *n != "[default]" && *n != "Apply")
                .collect::<Vec<_>>()
            {
                if !observed.iter().any(|o| o == name) {
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
    } else if let Some((_, current)) = &capture.current_matrix_profile {
        let listed = capture
            .enumerations
            .get("matrix")
            .map(|list| list.iter().any(|e| &e.name == current))
            .unwrap_or(false);
        if !listed {
            report.divergences.push(Divergence {
                family: "matrix_current".into(),
                kind: DivergenceKind::MissingEntry,
                detail: format!(
                    "MatrixGetProfile reports {current:?} as current, but it is absent from this \
                     daemon's own MatrixListProfiles"
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

fn attr_of(document: &str, key: &str) -> Option<String> {
    let pat = format!(" {key}=\"");
    let start = document.find(&pat)? + pat.len();
    let rest = &document[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
