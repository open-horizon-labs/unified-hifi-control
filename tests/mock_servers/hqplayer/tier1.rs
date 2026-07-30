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

use unified_hifi_control::adapters::hqplayer::HqpAdapter;

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

/// Families the corpus holds as enumerations, paired with their item tag.
const ENUM_FAMILIES: [(&str, &str, &str); 4] = [
    ("modes", "GetModes", "ModesItem"),
    ("filters", "GetFilters", "FiltersItem"),
    ("shapers", "GetShapers", "ShapersItem"),
    ("rates", "GetRates", "RatesItem"),
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
            (other, _) => other.to_string(),
        };
        let expected = corpus::enum_entries(&corpus::document(profile, &doc_stem), item_tag);

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
