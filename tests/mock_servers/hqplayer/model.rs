//! Stateful HQPlayer daemon model — the semantics layer of the conformance boundary.
//!
//! Decides **what** the daemon says and **how its state changes**. It never touches a socket: it is
//! a `request line -> reply document` function over mutable state, driven by [`super::wire`].
//! Enumeration documents come from [`super::corpus`].
//!
//! Every capability here exists because a named issue #322 acceptance criterion asks for it:
//!
//! | Capability | Acceptance criterion |
//! |---|---|
//! | `GetInfo`, `State`, `Status` with `metadata` child, `VolumeRange`, modes/rates/filters/shapers, advanced settings | AC1 |
//! | [`Faults::reject_next`] | AC3 explicit `result="Error"` |
//! | [`Faults::accept_but_ignore`] | AC3 syntactic OK without state change |
//! | [`Faults::apply_after_polls`] | AC3 delayed state change |
//! | [`DaemonModel::external_change`] | AC3 external changes |
//! | volume range variants, mute | AC4 |
//! | [`DaemonModel::requests`] | AC5 — assert the *value the client sent*, from observed list pairs |
//! | mode-relative enumerations | AC1, AC5 |
//! | transport / seek / matrix | AC7 |
//!
//! Behaviour is taken from the verified protocol reference:
//! <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/protocol.md>
//!
//! * Setters and transport commands echo the request element with `result="OK"` or
//!   `result="Error"`, the latter carrying a reason as element text. Unknown elements get an echoed
//!   error and the connection is never dropped.
//! * `SetAdaptiveVolume` answers a bare `<SetAdaptiveVolume/>` with **no** `result` attribute —
//!   absent is a third case and must not read as failure.
//! * `State` reports settings as **list indices**; `Status` reports the *active* filter/shaper/mode
//!   as display strings. `State.volume` is a double in dB.
//! * Enumerations are mode-relative: a mode change swaps the filter/shaper/rate lists wholesale and
//!   resets the rate to auto.
//! * A setter can answer `result="OK"` without the setting actually applying, so `OK` alone is
//!   never proof of application.

use std::sync::{Arc, Mutex};

use super::corpus::{self, VERIFIED_PROFILE};
use super::wire::Responder;

/// Layout of reply documents. Both are legal; the multiline shape is what the daemon's own
/// container responses look like and is what misframes a line-per-document reader.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DocumentStyle {
    /// Declaration and whole document on one line.
    Compact,
    /// Container children one per line, so a container's first `/>` arrives before its closing tag.
    #[default]
    PrettyMultiline,
}

/// Volume control shape, mirroring the `VolumeRange` response (AC4).
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeRange {
    pub min_db: f64,
    pub max_db: f64,
    /// `None` reproduces the verified live sample, which sends no `step` attribute at all.
    pub step_db: Option<f64>,
    pub enabled: bool,
    pub adaptive: bool,
}

impl Default for VolumeRange {
    fn default() -> Self {
        Self {
            min_db: -60.0,
            max_db: 0.0,
            step_db: Some(0.5),
            enabled: true,
            adaptive: false,
        }
    }
}

/// Track metadata rendered as the `Status` document's optional self-closing child (AC1, AC2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    pub artist: String,
    pub album: String,
    pub song: String,
    pub samplerate: u32,
    pub bits: u32,
    pub channels: u32,
    pub bitrate: u32,
}

impl Metadata {
    /// A track whose title contains an ampersand, so entity handling is exercised.
    pub fn sample() -> Self {
        Self {
            artist: "Bill Evans".to_string(),
            album: "Sunday at the Village Vanguard".to_string(),
            song: "Alice in Wonderland".to_string(),
            samplerate: 44100,
            bits: 24,
            channels: 2,
            bitrate: 1411,
        }
    }
}

/// Which DSP chain the engine currently has **loaded**.
///
/// Distinct from the configured mode, and that distinction is the point. Under `[source]`
/// (`mode_index == 0`) a chain *is* loaded and takes filter and shaper edits live, but which one
/// depends on the material the source is feeding — so the configured mode cannot answer it. Deriving
/// the chain from `mode_index`, as this model did before the HQPTuner amendment, makes a
/// source-following session inexpressible. Chain liveness and pin family are not one question.
///
/// **Provenance: derived-upstream, tier-2-only.** Source-following is an upstream observation on
/// hqplayerd 6.0.4 (Opal), one host, and is *not* UHC-qualified — see #332. It is also not
/// promotable by the tier-1 read-only gate: tier 1 captures whichever chain the daemon already has
/// loaded, and reaching the other one needs `SetMode`, which mutates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadedChain {
    Pcm,
    Sdm,
}

/// How a renderer reports `active_mode`.
///
/// `State.active_mode` and `Status.active_mode` have **independent, unresolved semantics** in UHC's
/// evidence base. **#341 owns settling them.** What is actually known:
///
/// * `Status.active_mode` **does not resolve** under `[source]`: it echoes the configured mode,
///   verified upstream on hqplayerd 6.0.4, 2026-07-29, mid-playback. The upstream client therefore
///   *rejects* that field as a chain resolver and falls back to the `Status.active_rate` family
///   instead (`livemap._chain_from_status`). An earlier upstream revision did build a fallback on
///   `active_mode` and it was replaced precisely because it was wrong.
/// * `docs/hqplayer-protocol-reference.md` warns that `Status.active_mode` may read `[source]` while
///   DSD is being output, and says to trust `State`'s numeric `active_mode` instead. **No capture in
///   UHC's evidence base shows what `State.active_mode` reports under `[source]`.**
///
/// So the two sources are not in contradiction and it is not the case that "both cannot be right":
/// one is measured to echo, and the other is simply **unmeasured**. An earlier revision of this
/// comment described a contradiction, which came from propagating a superseded reading out of the
/// stable-branch salvage report; the beta/dev delta had already corrected it.
///
/// Before this amendment the model derived both fields from `mode_index`, so the fake agreed with
/// itself by construction and quietly answered the unmeasured half. Making the reporting a
/// **per-profile policy** means a fixture states which behaviour it claims, and a divergence between
/// the two fields becomes something a test can express rather than something the fake has ruled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveModeReporting {
    /// Echo the **configured** mode. Under `[source]` this reports `[source]` and resolves nothing.
    /// **Measured** for `Status.active_mode`: hqplayerd 6.0.4, 2026-07-29, playback active.
    EchoesConfiguredMode,
    /// Resolve to the **loaded** chain's family — what a caller usually wants, and what
    /// `docs/hqplayer-protocol-reference.md` claims of `State.active_mode`. **Unmeasured** for either
    /// field. Note that a client wanting the loaded chain under `[source]` should derive it from
    /// `Status.active_rate`, as the upstream client does, rather than expect this from `active_mode`.
    ResolvesLoadedChain,
}

/// Which filter fields a `State` document carries.
///
/// **This is a client-contract shape, not a daemon claim.** Every capture in the corpus carries both
/// `filter1x` and `filterNx`, and nothing here asserts that a daemon omits them. What *is* true, and
/// what this exists to make reachable, is that UHC's own parser admits their absence —
/// `HqpState::filter1x` and `HqpState::filter_nx` are `Option<u32>` — so the client has a branch for
/// it and that branch decides whether a one-sided `SetFilter` **guesses** the sibling from the legacy
/// combined `filter` field or **refuses**. #322's own audit recorded the guess as defect C5.
///
/// Constructed shapes and observed evidence are kept apart in this suite (see the
/// `synthetic-chain-hazard` profile), so this is deliberately *not* reported by
/// [`DaemonModel::armed_upstream_claims`]: that list names unqualified claims about **daemons**, and
/// this makes no claim about any daemon at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FilterFieldReporting {
    /// `filter`, `filter1x` and `filterNx` — what every corpus capture shows.
    #[default]
    Split,
    /// The legacy combined `filter` only, so both siblings read as absent to the client.
    CombinedOnly,
}

/// The daemon's observable state.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonState {
    /// 0 stopped, 1 paused, 2 playing, 3 stop requested.
    pub playback: u8,
    /// The **configured** `ModesItem` list index: 0 `[source]`, 1 PCM, 2 SDM.
    ///
    /// For which chain is loaded, read [`Self::loaded_chain`] — under `[source]` these differ.
    pub mode_index: u32,
    /// The chain the engine has loaded, independent of [`Self::mode_index`].
    ///
    /// A configured PCM or SDM mode pins this. Under `[source]` the source decides, so a test moves
    /// it with [`DaemonModel::external_change`] while `mode_index` stays 0.
    pub loaded_chain: LoadedChain,
    pub filter_1x_index: u32,
    pub filter_nx_index: u32,
    pub shaper_index: u32,
    /// `RatesItem` list index; 0 is auto/source-based.
    pub rate_index: u32,
    /// `GetJunkFilters` list index, reported by `State` as the `filter_junk` int.
    pub filter_junk_index: u32,
    pub volume_db: f64,
    pub volume_range: VolumeRange,
    pub convolution: bool,
    pub invert: bool,
    pub repeat: u8,
    pub random: bool,
    pub matrix_profile: String,
    pub position: u32,
    pub length: u32,
    pub track: u32,
    pub track_id: String,
    pub metadata: Option<Metadata>,
    pub active_rate_hz: u32,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            playback: 0,
            mode_index: 1, // PCM
            // Consistent with the configured mode: a configured PCM mode pins the PCM chain.
            loaded_chain: LoadedChain::Pcm,
            filter_1x_index: 6,
            filter_nx_index: 6,
            shaper_index: 4,
            rate_index: 0,
            filter_junk_index: 0,
            // Fractional negative dB by default, because that is the wire reality (AC4).
            volume_db: -23.5,
            volume_range: VolumeRange::default(),
            convolution: false,
            invert: false,
            repeat: 0,
            random: false,
            matrix_profile: "Default".to_string(),
            position: 0,
            length: 0,
            track: 0,
            track_id: String::new(),
            metadata: None,
            active_rate_hz: 0,
        }
    }
}

type Change = Box<dyn FnOnce(&mut DaemonState) + Send>;

/// Deliberate misbehaviours a test can arm (AC3). Each models something the real daemon does.
#[derive(Default)]
pub struct Faults {
    /// `(element, reason)` — answer `result="Error"` once and apply nothing.
    pub reject_next: Vec<(String, String)>,
    /// Elements that answer `result="OK"` but never apply.
    pub accept_but_ignore: Vec<String>,
    /// `(element, polls)` — answer `OK` now, apply after this many `State`/`Status` reads.
    pub apply_after_polls: Vec<(String, u32)>,
    /// Make `MatrixGetProfile` report a different name than `State.matrix_profile`, so a client can
    /// be tested against a daemon whose two views of the same setting disagree.
    pub matrix_current_override: Option<String>,
    /// Under a configured `[source]` mode, answer every `SetRate` with `result="OK"` and apply
    /// nothing, leaving both `State.rate` and `Status.active_rate` where they were.
    ///
    /// Distinct from [`Self::accept_but_ignore`], which is unconditional: the refusal here is
    /// **mode-conditional**, which is the behaviour that was measured, and it is what makes the
    /// difference between "this setter is deaf" and "this setter is deaf *in this mode*". What
    /// governs the rate under `[source]` is a persistent config limit for which no wire command
    /// exists, so no amount of retrying on the live lane can succeed.
    ///
    /// Verified upstream 2026-07-29 mid-playback, twice, and confirmed on the output as well as the
    /// slot. **Provenance: derived-upstream, tier-2-only, pending #332.** Defaults to `false` so no
    /// existing expectation silently changes meaning; a fixture that wants the behaviour opts in and
    /// carries the provenance.
    pub source_refuses_rate_pin: bool,
    pending: Vec<(u32, Change)>,
}

impl std::fmt::Debug for Faults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Faults")
            .field("reject_next", &self.reject_next)
            .field("accept_but_ignore", &self.accept_but_ignore)
            .field("apply_after_polls", &self.apply_after_polls)
            .field("matrix_current_override", &self.matrix_current_override)
            // Named even when unarmed. Faults are printed in assertion messages precisely so a failure
            // says which misbehaviour was armed, and this flag changes `SetRate` semantics — omitting
            // it made the mode-conditional refusal invisible in exactly the message a maintainer reads.
            .field("source_refuses_rate_pin", &self.source_refuses_rate_pin)
            .field("pending", &self.pending.len())
            .finish()
    }
}

struct Enumerations {
    modes: String,
    filters_pcm: String,
    filters_sdm: String,
    shapers_pcm: String,
    shapers_sdm: String,
    rates_pcm: String,
    rates_sdm: String,
    junk_filters: String,
    matrix_profiles: String,
    info: String,
}

impl Enumerations {
    fn load(profile: &str) -> Self {
        // A non-verified profile is deliberately partial. Fall back to the verified corpus for
        // documents it does not carry rather than inventing shapes for a version nobody observed.
        // Whether the profile carries the document is corpus's policy, not ours: `carries` checks the
        // same extensions and root `load` uses, so a `.html` fixture is not missed by a hardcoded
        // `.xml` and routed to the fallback.
        let pick = |stem: &str| -> String {
            if corpus::carries(profile, stem) {
                corpus::document(profile, stem)
            } else {
                corpus::document(VERIFIED_PROFILE, stem)
            }
        };
        Self {
            modes: pick("modes"),
            filters_pcm: pick("filters_pcm"),
            filters_sdm: pick("filters_sdm"),
            shapers_pcm: pick("shapers_pcm"),
            shapers_sdm: pick("shapers_sdm"),
            rates_pcm: pick("rates_pcm"),
            rates_sdm: pick("rates_sdm"),
            junk_filters: pick("junkfilters"),
            matrix_profiles: pick("matrix_profiles"),
            info: pick("getinfo"),
        }
    }
}

struct Inner {
    state: DaemonState,
    faults: Faults,
    style: DocumentStyle,
    /// How `State.active_mode` reports. Separate from the `Status` policy **because the contradiction
    /// is precisely that the two may disagree** — one shared field would re-impose the agreement the
    /// amendment removed.
    state_active_mode: ActiveModeReporting,
    /// How `Status.active_mode` reports.
    status_active_mode: ActiveModeReporting,
    /// Which filter fields `State` carries. See [`FilterFieldReporting`].
    filter_fields: FilterFieldReporting,
    /// `(element, chain)` queue — each entry moves the loaded chain once, right after that element
    /// is answered, in the order they were armed.
    ///
    /// A queue rather than a single slot because a client that retries has more than one window to
    /// be caught in, and "the chain moved during the retry as well" is the case a bounded retry must
    /// answer for. One slot could only ever express the first.
    chain_switch_after: Vec<(String, LoadedChain)>,
    /// `(element, mode index)` queue — an external controller changes configured mode immediately
    /// after the named reply, before the client's next command.
    mode_switch_after: Vec<(String, u32)>,
    enums: Enumerations,
    /// Every request line received, in order, so a test can assert on the value the client actually
    /// put on the wire (AC5) instead of on timing.
    requests: Vec<String>,
}

/// The stateful daemon model. Cloneable handle over shared state.
#[derive(Clone)]
pub struct DaemonModel {
    inner: Arc<Mutex<Inner>>,
}

impl DaemonModel {
    /// A daemon serving the live-verified 6.0.4 corpus.
    pub fn verified() -> Self {
        Self::with_profile(VERIFIED_PROFILE)
    }

    pub fn with_profile(profile: &str) -> Self {
        let mut inner = Inner {
            state: DaemonState::default(),
            faults: Faults::default(),
            style: DocumentStyle::default(),
            // Defaults reproduce the *current* shipped model exactly, so no existing expectation
            // changes meaning. They are not a ruling on the contradiction: `EchoesConfiguredMode`
            // happens to be the verified behaviour for `Status` (hqplayerd 6.0.4, 2026-07-29) and is
            // merely the status quo for `State`, whose resolving claim is unverified. A fixture that
            // wants either behaviour must now say so.
            state_active_mode: ActiveModeReporting::EchoesConfiguredMode,
            status_active_mode: ActiveModeReporting::EchoesConfiguredMode,
            filter_fields: FilterFieldReporting::default(),
            chain_switch_after: Vec::new(),
            mode_switch_after: Vec::new(),
            enums: Enumerations::load(profile),
            requests: Vec::new(),
        };
        // A real daemon never reports an index outside its own lists, and list lengths differ per
        // version profile and per mode. Clamp the defaults so the model stays self-consistent
        // whichever corpus it is serving.
        let clamp = |value: u32, count: u32| if count == 0 { 0 } else { value.min(count - 1) };
        let filters = inner.entry_count("GetFilters", "FiltersItem");
        let shapers = inner.entry_count("GetShapers", "ShapersItem");
        let rates = inner.entry_count("GetRates", "RatesItem");
        let modes = inner.entry_count("GetModes", "ModesItem");
        let junk = inner.entry_count("GetJunkFilters", "JunkFiltersItem");
        inner.state.filter_1x_index = clamp(inner.state.filter_1x_index, filters);
        inner.state.filter_nx_index = clamp(inner.state.filter_nx_index, filters);
        inner.state.shaper_index = clamp(inner.state.shaper_index, shapers);
        inner.state.rate_index = clamp(inner.state.rate_index, rates);
        inner.state.mode_index = clamp(inner.state.mode_index, modes);
        inner.state.filter_junk_index = clamp(inner.state.filter_junk_index, junk);
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    pub fn set_style(&self, style: DocumentStyle) {
        self.lock().style = style;
    }

    /// Declare which filter fields `State` carries. See [`FilterFieldReporting`] — a client-contract
    /// shape, never a claim about a daemon.
    pub fn set_filter_field_reporting(&self, reporting: FilterFieldReporting) {
        self.lock().filter_fields = reporting;
    }

    /// Declare how each renderer reports `active_mode`.
    ///
    /// A fixture must state this rather than inherit it, because the two sources are the subject of
    /// an unresolved evidence contradiction that **#341** owns. See [`ActiveModeReporting`].
    pub fn set_active_mode_reporting(
        &self,
        state: ActiveModeReporting,
        status: ActiveModeReporting,
    ) {
        let mut inner = self.lock();
        inner.state_active_mode = state;
        inner.status_active_mode = status;
    }

    /// Move the **loaded chain** without touching the configured mode, as a source change does under
    /// `[source]`. Chain-scoped indices are clamped into the new chain's lists, which is a
    /// self-consistency invariant and not a claim that the daemon preserves selections.
    ///
    /// **Provenance: derived-upstream, tier-2-only** — see [`LoadedChain`].
    pub fn source_loads_chain(&self, chain: LoadedChain) {
        let mut inner = self.lock();
        // `[source]` is the only configured mode whose loaded chain the source decides; in PCM or SDM
        // the configured mode *is* the chain. Letting the two disagree produces a daemon state that
        // does not exist — SDM enumerations served while PCM is reported configured — and
        // `armed_upstream_claims` does not flag it, so a test built on it would prove nothing about the
        // client. Refused at the setter, where the misuse is.
        assert_eq!(
            inner.state.mode_index, 0,
            "source_loads_chain requires configured [source] mode"
        );
        inner.state.loaded_chain = chain;
        inner.clamp_to_loaded_chain();
    }

    /// Move the loaded chain **while the client is mid-conversation**: once, immediately after the
    /// named element has been answered.
    ///
    /// [`Self::source_loads_chain`] moves the chain between client calls, which is enough for a
    /// client that reads one list. It is not enough for one that reads several and publishes them as
    /// a set: four replies read one after another are four moments, and a source change landing
    /// between two of them yields a set that mixes the chains. Only a change *inside* the window can
    /// express that, so the trigger is the request rather than the test's own timeline.
    ///
    /// **Provenance: derived-upstream, tier-2-only** — the same source-following observation as
    /// [`LoadedChain`]. That a change can land between two requests needs no separate evidence: it
    /// follows from the chain being able to move at all while the client is connected.
    ///
    /// Calls queue: arming twice moves the chain on the next two matching requests, in order, which
    /// is how a client's *retry* window is reached as well as its first.
    pub fn switch_chain_after_request(&self, element: &str, chain: LoadedChain) {
        let mut inner = self.lock();
        assert_eq!(
            inner.state.mode_index, 0,
            "switch_chain_after_request requires configured [source] mode, for the same reason \
             source_loads_chain does: in PCM or SDM the configured mode *is* the chain"
        );
        inner.chain_switch_after.push((element.to_string(), chain));
    }

    /// Change configured mode between two client commands, as another controller can.
    pub fn switch_mode_after_request(&self, element: &str, mode_index: u32) {
        let mut inner = self.lock();
        assert!(
            mode_index < inner.entry_count("GetModes", "ModesItem"),
            "switch_mode_after_request requires a mode the daemon offers"
        );
        inner
            .mode_switch_after
            .push((element.to_string(), mode_index));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("daemon model lock")
    }

    /// Read the daemon's state, e.g. to prove a setter actually applied.
    pub fn state(&self) -> DaemonState {
        self.lock().state.clone()
    }

    /// Change state behind the client's back, as another HQPlayer controller would (AC3).
    pub fn external_change(&self, f: impl FnOnce(&mut DaemonState)) {
        f(&mut self.lock().state);
    }

    /// Arm deliberate misbehaviour (AC3).
    pub fn arm(&self, f: impl FnOnce(&mut Faults)) {
        f(&mut self.lock().faults);
    }

    /// Which currently-armed behaviours are **not UHC-qualified**, named so a test cannot claim a
    /// real daemon combination by accident.
    ///
    /// Every entry is derived from upstream observation on one daemon, one host, and is pending #332.
    /// A default model returns an empty list: nothing unqualified is on unless a fixture asked for it.
    /// The point is that arming such a behaviour is visible rather than implicit — a fake with more
    /// switches than evidence can otherwise be configured into a daemon that has never existed, and a
    /// conformance verdict about an impossible daemon is worse than no verdict.
    pub fn armed_upstream_claims(&self) -> Vec<&'static str> {
        let inner = self.lock();
        let mut armed = Vec::new();
        if inner.faults.source_refuses_rate_pin {
            armed.push("source_refuses_rate_pin (derived-upstream, tier-2-only, pending #332)");
        }
        if inner.state.loaded_chain != LoadedChain::Pcm && inner.state.mode_index == 0 {
            armed.push(
                "loaded chain moved under [source] (derived-upstream, tier-2-only, pending #332)",
            );
        }
        if inner.state_active_mode == ActiveModeReporting::ResolvesLoadedChain {
            armed.push("State.active_mode resolving (UNMEASURED - no capture supports it)");
        }
        if inner.status_active_mode == ActiveModeReporting::ResolvesLoadedChain {
            armed.push("Status.active_mode resolving (contradicts the measured echo behaviour)");
        }
        armed
    }

    /// Full request lines the client has sent, in order.
    pub fn requests(&self) -> Vec<String> {
        self.lock().requests.clone()
    }

    /// The most recent request line for an element, if any.
    pub fn last_request(&self, element: &str) -> Option<String> {
        self.lock()
            .requests
            .iter()
            .rev()
            .find(|r| request_element(r).as_deref() == Some(element))
            .cloned()
    }

    /// How many times the client sent an element.
    pub fn request_count(&self, element: &str) -> usize {
        self.lock()
            .requests
            .iter()
            .filter(|r| request_element(r).as_deref() == Some(element))
            .count()
    }

    /// The enumeration document currently in force, honouring mode relativity.
    pub fn current_enumeration(&self, family: &str) -> String {
        self.lock().enumeration(family)
    }
}

impl Inner {
    /// Whether the **loaded chain** is SDM.
    ///
    /// Reads `loaded_chain`, deliberately not `mode_index`. Before the amendment this was
    /// `mode_index == 2`, which meant `[source]` served the PCM lists unconditionally and no fixture
    /// could express a source-following session.
    fn sdm(&self) -> bool {
        self.state.loaded_chain == LoadedChain::Sdm
    }

    /// Clamp every chain-scoped index into the currently loaded chain's list bounds.
    ///
    /// This is a **self-consistency invariant, not a behavioural claim**: a real daemon never reports
    /// an index outside its own list, and the two chains' lists differ in length. It deliberately
    /// does *not* assert that the daemon preserves a selection across a chain change, which is
    /// unobserved — it only keeps the model from reporting an index its own enumeration cannot
    /// resolve. `with_profile` applies the same invariant at construction.
    fn clamp_to_loaded_chain(&mut self) {
        let clamp = |value: u32, count: u32| if count == 0 { 0 } else { value.min(count - 1) };
        let filters = self.entry_count("GetFilters", "FiltersItem");
        let shapers = self.entry_count("GetShapers", "ShapersItem");
        let rates = self.entry_count("GetRates", "RatesItem");
        self.state.filter_1x_index = clamp(self.state.filter_1x_index, filters);
        self.state.filter_nx_index = clamp(self.state.filter_nx_index, filters);
        self.state.shaper_index = clamp(self.state.shaper_index, shapers);
        self.state.rate_index = clamp(self.state.rate_index, rates);
    }

    fn enumeration(&self, family: &str) -> String {
        match (family, self.sdm()) {
            ("GetModes", _) => self.enums.modes.clone(),
            ("GetFilters", false) => self.enums.filters_pcm.clone(),
            ("GetFilters", true) => self.enums.filters_sdm.clone(),
            ("GetShapers", false) => self.enums.shapers_pcm.clone(),
            ("GetShapers", true) => self.enums.shapers_sdm.clone(),
            ("GetRates", false) => self.enums.rates_pcm.clone(),
            ("GetRates", true) => self.enums.rates_sdm.clone(),
            ("GetJunkFilters", _) => self.enums.junk_filters.clone(),
            ("MatrixListProfiles", _) => self.enums.matrix_profiles.clone(),
            ("GetInfo", _) => self.enums.info.clone(),
            other => panic!("no corpus enumeration for {other:?}"),
        }
    }

    fn name_at_index(&self, family: &str, item_tag: &str, index: u32) -> String {
        corpus::enum_entries(&self.enumeration(family), item_tag)
            .into_iter()
            .find(|e| e.index == index)
            .map(|e| e.name)
            .unwrap_or_default()
    }

    fn entry_count(&self, family: &str, item_tag: &str) -> u32 {
        corpus::enum_entries(&self.enumeration(family), item_tag).len() as u32
    }

    /// Advance delayed-apply changes. Called on every `State`/`Status` render (AC3).
    fn tick_pending(&mut self) {
        let mut still = Vec::new();
        let mut applied = false;
        for (remaining, change) in std::mem::take(&mut self.faults.pending) {
            if remaining <= 1 {
                change(&mut self.state);
                applied = true;
            } else {
                still.push((remaining - 1, change));
            }
        }
        self.faults.pending = still;
        // A *deferred* `SetMode` lands here rather than in the setter arm, so the chain can change
        // without the setter's clamp having run. Re-applying the invariant here is what keeps a
        // delayed mode change from leaving an index the new chain's enumeration cannot resolve.
        if applied {
            self.clamp_to_loaded_chain();
        }
    }

    fn newline(&self) -> &'static str {
        match self.style {
            DocumentStyle::Compact => "",
            DocumentStyle::PrettyMultiline => "\n",
        }
    }

    fn wrap(&self, document: &str) -> String {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{document}")
    }

    /// The mode index the *loaded* chain corresponds to: 1 PCM, 2 SDM. Used only by a resolving
    /// [`ActiveModeReporting`] policy.
    fn loaded_chain_mode_index(&self) -> u32 {
        match self.state.loaded_chain {
            LoadedChain::Pcm => 1,
            LoadedChain::Sdm => 2,
        }
    }

    fn render_state(&mut self) -> String {
        self.tick_pending();
        let active_mode_index = match self.state_active_mode {
            ActiveModeReporting::EchoesConfiguredMode => self.state.mode_index,
            ActiveModeReporting::ResolvesLoadedChain => self.loaded_chain_mode_index(),
        };
        // Absent siblings are rendered by *omitting the attributes*, not by emitting empty ones: the
        // client reads them with `parse_attr(...).and_then(parse)`, and an empty value would be an
        // unparseable present attribute rather than an absent one. Only absence exercises the branch.
        let siblings = match self.filter_fields {
            FilterFieldReporting::Split => format!(
                " filter1x=\"{}\" filterNx=\"{}\"",
                self.state.filter_1x_index, self.state.filter_nx_index
            ),
            FilterFieldReporting::CombinedOnly => String::new(),
        };
        let s = &self.state;
        let doc = format!(
            concat!(
                "<State state=\"{state}\" mode=\"{mode}\" filter=\"{filter}\"",
                "{siblings} shaper=\"{shaper}\" rate=\"{rate}\" ",
                "volume=\"{volume}\" active_mode=\"{active_mode}\" active_rate=\"{active_rate}\" ",
                "convolution=\"{conv}\" invert=\"{invert}\" repeat=\"{repeat}\" ",
                "random=\"{random}\" adaptive=\"{adaptive}\" filter_junk=\"{junk}\" ",
                "matrix_profile=\"{matrix}\"/>"
            ),
            state = s.playback,
            mode = s.mode_index,
            // Legacy single-filter field; observed tracking the most recently set of 1x/Nx.
            filter = s.filter_nx_index,
            siblings = siblings,
            shaper = s.shaper_index,
            rate = s.rate_index,
            volume = format_db(s.volume_db),
            // Policy-driven, never derived from `mode_index` directly: see `ActiveModeReporting`.
            // `State.active_mode` is numeric, so a resolving policy reports the loaded chain's own
            // mode index rather than the configured one.
            active_mode = active_mode_index,
            active_rate = s.active_rate_hz,
            conv = flag(s.convolution),
            invert = flag(s.invert),
            repeat = s.repeat,
            random = flag(s.random),
            adaptive = flag(s.volume_range.adaptive),
            junk = s.filter_junk_index,
            // Already in wire form: corpus documents carry escaped attribute values.
            matrix = s.matrix_profile,
        );
        self.wrap(&doc)
    }

    fn render_status(&mut self) -> String {
        self.tick_pending();
        let active_filter =
            self.name_at_index("GetFilters", "FiltersItem", self.state.filter_nx_index);
        let active_shaper =
            self.name_at_index("GetShapers", "ShapersItem", self.state.shaper_index);
        // Policy-driven: `Status.active_mode` is a display string, so an echoing policy names the
        // configured mode (`[source]` included) while a resolving one names the loaded chain.
        let active_mode_index = match self.status_active_mode {
            ActiveModeReporting::EchoesConfiguredMode => self.state.mode_index,
            ActiveModeReporting::ResolvesLoadedChain => self.loaded_chain_mode_index(),
        };
        let active_mode = self.name_at_index("GetModes", "ModesItem", active_mode_index);
        let nl = self.newline();
        let s = self.state.clone();

        let open = format!(
            concat!(
                "<Status state=\"{state}\" track=\"{track}\" track_id=\"{track_id}\" ",
                "position=\"{position}\" length=\"{length}\" volume=\"{volume}\" ",
                "active_mode=\"{active_mode}\" active_filter=\"{active_filter}\" ",
                "active_shaper=\"{active_shaper}\" active_rate=\"{active_rate}\" ",
                "active_bits=\"{bits}\" active_channels=\"{channels}\" ",
                "samplerate=\"{samplerate}\" bitrate=\"{bitrate}\" filter_junk=\"{junk}\" ",
                "random=\"{random}\" repeat=\"{repeat}\">"
            ),
            state = s.playback,
            track = s.track,
            track_id = s.track_id,
            position = s.position,
            length = s.length,
            volume = format_db(s.volume_db),
            // Names come from corpus documents, so they are already escaped.
            active_mode = active_mode,
            active_filter = active_filter,
            active_shaper = active_shaper,
            active_rate = s.active_rate_hz,
            bits = s.metadata.as_ref().map(|m| m.bits).unwrap_or(0),
            channels = s.metadata.as_ref().map(|m| m.channels).unwrap_or(0),
            samplerate = s.metadata.as_ref().map(|m| m.samplerate).unwrap_or(0),
            bitrate = s.metadata.as_ref().map(|m| m.bitrate).unwrap_or(0),
            junk = s.filter_junk_index,
            random = flag(s.random),
            repeat = s.repeat,
        );

        // The metadata child is SELF-CLOSING, so the document ends at `</Status>` and not here.
        let child = match s.metadata.as_ref() {
            Some(m) => format!(
                concat!(
                    "<metadata artist=\"{artist}\" album=\"{album}\" song=\"{song}\" ",
                    "samplerate=\"{samplerate}\" bits=\"{bits}\" channels=\"{channels}\" ",
                    "bitrate=\"{bitrate}\"/>{nl}"
                ),
                artist = xml_escape(&m.artist),
                album = xml_escape(&m.album),
                song = xml_escape(&m.song),
                samplerate = m.samplerate,
                bits = m.bits,
                channels = m.channels,
                bitrate = m.bitrate,
                nl = nl,
            ),
            None => String::new(),
        };

        self.wrap(&format!("{open}{nl}{child}</Status>"))
    }

    fn render_enumeration(&mut self, family: &str, item_tag: &str) -> String {
        let doc = self.enumeration(family);
        let entries = corpus::enum_entries(&doc, item_tag);
        let nl = self.newline();
        let mut body = format!("<{family}>{nl}");
        for e in &entries {
            match (e.rate, e.enum_id) {
                // RatesItem carries index and rate only, never a value attribute.
                (Some(rate), _) => body.push_str(&format!(
                    "<{item_tag} index=\"{}\" rate=\"{}\"/>{nl}",
                    e.index, rate
                )),
                // Names are copied straight back out: they arrived from a corpus document and are
                // already in escaped wire form. Escaping again would invent the double-escaping
                // quirk instead of reproducing the daemon.
                (None, Some(id)) => {
                    // arg and description are re-emitted when the corpus carries them: a fake that
                    // drops attributes its own fixtures declare is lossier than the daemon it models,
                    // and it would make those claims unverifiable by construction.
                    let arg = e.arg.map(|a| format!(" arg=\"{a}\"")).unwrap_or_default();
                    let desc = e
                        .description
                        .as_ref()
                        .map(|d| format!(" description=\"{d}\""))
                        .unwrap_or_default();
                    body.push_str(&format!(
                        "<{item_tag} index=\"{}\" name=\"{}\" value=\"{}\"{arg}{desc}/>{nl}",
                        e.index, e.name, id
                    ))
                }
                (None, None) => body.push_str(&format!(
                    "<{item_tag} index=\"{}\" name=\"{}\"/>{nl}",
                    e.index, e.name
                )),
            }
        }
        body.push_str(&format!("</{family}>"));
        self.wrap(&body)
    }

    fn ok(&self, element: &str) -> String {
        self.wrap(&format!("<{element} result=\"OK\"/>"))
    }

    fn error(&self, element: &str, reason: &str) -> String {
        if reason.is_empty() {
            self.wrap(&format!("<{element} result=\"Error\"/>"))
        } else {
            self.wrap(&format!(
                "<{element} result=\"Error\">{}</{element}>",
                xml_escape(reason)
            ))
        }
    }

    fn take_rejection(&mut self, element: &str) -> Option<String> {
        let at = self
            .faults
            .reject_next
            .iter()
            .position(|(el, _)| el == element)?;
        Some(self.faults.reject_next.remove(at).1)
    }

    fn ignores(&self, element: &str) -> bool {
        self.faults.accept_but_ignore.iter().any(|el| el == element)
    }

    fn delay_for(&self, element: &str) -> Option<u32> {
        self.faults
            .apply_after_polls
            .iter()
            .find(|(el, _)| el == element)
            .map(|(_, n)| *n)
    }

    /// Apply a setter's effect, honouring `accept_but_ignore` and `apply_after_polls`.
    /// The wire answer is `OK` in all three cases — that is the point.
    fn apply(&mut self, element: &str, change: Change) -> String {
        if self.ignores(element) {
            return self.ok(element);
        }
        match self.delay_for(element) {
            Some(polls) if polls > 0 => self.faults.pending.push((polls, change)),
            _ => change(&mut self.state),
        }
        self.ok(element)
    }
}

fn flag(v: bool) -> &'static str {
    if v {
        "1"
    } else {
        "0"
    }
}

/// Render dB the way the daemon does: a double, keeping a decimal place.
fn format_db(v: f64) -> String {
    if (v - v.trunc()).abs() < f64::EPSILON {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Element name of a request line. Delegates to the document layer so the wire and the model share
/// one tokeniser rather than keeping two that can drift.
pub fn request_element(line: &str) -> Option<String> {
    corpus::element_name(line)
}

/// Read an attribute off a request line. Used by tests to assert what the client sent (AC5).
pub fn request_attr(line: &str, key: &str) -> Option<String> {
    let pat = format!(" {key}=\"");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn request_u32(line: &str, key: &str) -> Option<u32> {
    request_attr(line, key)?.parse().ok()
}

impl Responder for DaemonModel {
    fn respond(&self, request: &str) -> Option<String> {
        let element = request_element(request)?;
        let mut inner = self.lock();
        inner.requests.push(request.trim().to_string());

        // An armed rejection wins, and the connection stays up on an error.
        if let Some(reason) = inner.take_rejection(&element) {
            return Some(inner.error(&element, &reason));
        }

        let reply = match element.as_str() {
            "GetInfo" => {
                let doc = inner.enumeration("GetInfo");
                inner.wrap(&doc)
            }
            // Embedded 6 exposes the active configuration through the native lane. The
            // conformance daemon models the ordinary working configuration; named profile
            // snapshots are represented by the web backup fixture rather than by a second
            // mutable native state machine.
            "ConfigurationGet" => inner.wrap("<ConfigurationGet value=\"\"/>"),
            "State" => inner.render_state(),
            "Status" => inner.render_status(),

            "VolumeRange" => {
                let vr = inner.state.volume_range.clone();
                let step = match vr.step_db {
                    Some(s) => format!(" step=\"{}\"", format_db(s)),
                    None => String::new(),
                };
                inner.wrap(&format!(
                    "<VolumeRange min=\"{}\" max=\"{}\"{} enabled=\"{}\" adaptive=\"{}\"/>",
                    format_db(vr.min_db),
                    format_db(vr.max_db),
                    step,
                    flag(vr.enabled),
                    flag(vr.adaptive),
                ))
            }

            "GetModes" => inner.render_enumeration("GetModes", "ModesItem"),
            "GetFilters" => inner.render_enumeration("GetFilters", "FiltersItem"),
            "GetShapers" => inner.render_enumeration("GetShapers", "ShapersItem"),
            "GetRates" => inner.render_enumeration("GetRates", "RatesItem"),
            "GetJunkFilters" => inner.render_enumeration("GetJunkFilters", "JunkFiltersItem"),
            "MatrixListProfiles" => inner.render_enumeration("MatrixListProfiles", "MatrixProfile"),

            "MatrixGetProfile" => {
                let name = inner
                    .faults
                    .matrix_current_override
                    .clone()
                    .unwrap_or_else(|| inner.state.matrix_profile.clone());
                let index =
                    corpus::enum_entries(&inner.enumeration("MatrixListProfiles"), "MatrixProfile")
                        .into_iter()
                        .find(|e| e.name == name)
                        .map(|e| e.index)
                        .unwrap_or(0);
                inner.wrap(&format!(
                    "<MatrixGetProfile index=\"{index}\" value=\"{name}\"/>"
                ))
            }

            "MatrixSetProfile" => {
                let name = request_attr(request, "value").unwrap_or_default();
                let known =
                    corpus::enum_entries(&inner.enumeration("MatrixListProfiles"), "MatrixProfile")
                        .into_iter()
                        .any(|e| e.name == name);
                if known {
                    inner.apply(
                        "MatrixSetProfile",
                        Box::new(move |s| s.matrix_profile = name),
                    )
                } else {
                    inner.error("MatrixSetProfile", "unknown profile")
                }
            }

            "SetMode" => match request_u32(request, "value") {
                Some(index) if index < inner.entry_count("GetModes", "ModesItem") => {
                    let reply = inner.apply(
                        "SetMode",
                        Box::new(move |s| {
                            s.mode_index = index;
                            // Verified: `SetMode` clears the single rate pin outright, and does so
                            // even when the mode does not change. Measured upstream 2026-07-28.
                            s.rate_index = 0;
                            s.active_rate_hz = 0;
                            // A configured PCM or SDM mode pins the loaded chain. `[source]` does
                            // not: the source keeps deciding, so the chain is left where it was.
                            match index {
                                1 => s.loaded_chain = LoadedChain::Pcm,
                                2 => s.loaded_chain = LoadedChain::Sdm,
                                _ => {}
                            }
                            // NOTE: this deliberately no longer resets filter and shaper indices to
                            // 0. That reset was an unevidenced invention of this fake — upstream says
                            // `SetMode` clears the *rate pin* and reloads the chain, and says nothing
                            // about filters returning to the first entry. Keeping indices consistent
                            // with the newly loaded chain's list is handled by the clamp below, which
                            // is an invariant rather than a behavioural claim.
                        }),
                    );
                    inner.clamp_to_loaded_chain();
                    reply
                }
                _ => inner.error("SetMode", "invalid mode"),
            },

            "SetFilter" => {
                let count = inner.entry_count("GetFilters", "FiltersItem");
                let nx = request_u32(request, "value");
                let one_x = request_u32(request, "value1x");
                match nx {
                    Some(nx) if nx < count && one_x.map(|v| v < count).unwrap_or(true) => inner
                        .apply(
                            "SetFilter",
                            Box::new(move |s| {
                                s.filter_nx_index = nx;
                                // `value` alone sets BOTH; `value1x` splits them, `value` = Nx.
                                s.filter_1x_index = one_x.unwrap_or(nx);
                            }),
                        ),
                    _ => inner.error("SetFilter", "invalid filter"),
                }
            }

            "SetShaping" => {
                let count = inner.entry_count("GetShapers", "ShapersItem");
                match request_u32(request, "value") {
                    Some(index) if index < count => {
                        inner.apply("SetShaping", Box::new(move |s| s.shaper_index = index))
                    }
                    _ => inner.error("SetShaping", "invalid shaper"),
                }
            }

            "SetRate" => {
                let entries = corpus::enum_entries(&inner.enumeration("GetRates"), "RatesItem");
                // Mode-conditional deafness, checked before validity: under `[source]` the daemon
                // answers OK and applies nothing, whatever the requested index. Note that this is
                // indistinguishable from success by readback whenever the request happens to equal
                // the current value — Auto/0 against an already-unpinned rate being the case that
                // matters. #347 owns making that outcome truthful.
                if inner.faults.source_refuses_rate_pin && inner.state.mode_index == 0 {
                    inner.ok("SetRate")
                } else {
                    match request_u32(request, "value") {
                        Some(index) if entries.iter().any(|e| e.index == index) => {
                            let hz = entries
                                .iter()
                                .find(|e| e.index == index)
                                .and_then(|e| e.rate)
                                .unwrap_or(0);
                            inner.apply(
                                "SetRate",
                                Box::new(move |s| {
                                    s.rate_index = index;
                                    s.active_rate_hz = hz;
                                }),
                            )
                        }
                        _ => inner.error("SetRate", "invalid rate"),
                    }
                }
            }

            "SetJunkFilter" => {
                let count = inner.entry_count("GetJunkFilters", "JunkFiltersItem");
                match request_u32(request, "value") {
                    Some(index) if index < count => inner.apply(
                        "SetJunkFilter",
                        Box::new(move |s| s.filter_junk_index = index),
                    ),
                    _ => inner.error("SetJunkFilter", "invalid junk filter"),
                }
            }

            "SetConvolution" => {
                let on = request_attr(request, "value").as_deref() == Some("1");
                inner.apply("SetConvolution", Box::new(move |s| s.convolution = on))
            }

            // Verified quirk: a bare element with NO result attribute.
            "SetAdaptiveVolume" => {
                let on = request_attr(request, "value").as_deref() == Some("1");
                if !inner.ignores("SetAdaptiveVolume") {
                    inner.state.volume_range.adaptive = on;
                }
                inner.wrap("<SetAdaptiveVolume/>")
            }

            "SetRepeat" => match request_u32(request, "value") {
                Some(mode @ 0..=2) => inner.apply(
                    "SetRepeat",
                    Box::new(move |state| state.repeat = mode as u8),
                ),
                _ => inner.error("SetRepeat", "invalid repeat mode"),
            },

            "SetRandom" => {
                let on = request_attr(request, "value").as_deref() == Some("1");
                inner.apply("SetRandom", Box::new(move |state| state.random = on))
            }

            "Volume" => {
                if !inner.state.volume_range.enabled {
                    // Verified: a fixed-volume daemon answers result="Error" with no reason text
                    // and leaves the level unchanged.
                    inner.error("Volume", "")
                } else {
                    match request_attr(request, "value").and_then(|v| v.parse::<f64>().ok()) {
                        Some(db) => {
                            let vr = inner.state.volume_range.clone();
                            if db < vr.min_db || db > vr.max_db {
                                inner.error("Volume", "out of range")
                            } else {
                                // An explicit level is how the daemon leaves the mute floor: `Volume`
                                // is the unmute path, not a second `VolumeMute`.
                                inner.apply(
                                    "Volume",
                                    Box::new(move |s| {
                                        s.volume_db = db;
                                    }),
                                )
                            }
                        }
                        None => inner.error("Volume", "invalid volume"),
                    }
                }
            }

            "VolumeUp" | "VolumeDown" => {
                if !inner.state.volume_range.enabled {
                    inner.error(&element, "")
                } else {
                    let vr = inner.state.volume_range.clone();
                    let step = vr.step_db.unwrap_or(1.0);
                    let delta = if element == "VolumeUp" { step } else { -step };
                    inner.apply(
                        &element,
                        Box::new(move |s| {
                            s.volume_db = (s.volume_db + delta).clamp(vr.min_db, vr.max_db)
                        }),
                    )
                }
            }

            // Absolute mute-to-floor, idempotent - verified live on a real HQPlayer 6.0.2 Embedded
            // daemon (issue #322): repeated VolumeMute keeps State.volume at the floor, it never toggles
            // back, and there is no separate mute flag on the wire. Unmute is a distinct `Volume` write.
            "VolumeMute" => inner.apply(
                "VolumeMute",
                Box::new(|s| s.volume_db = s.volume_range.min_db),
            ),

            "Play" => inner.apply("Play", Box::new(|s| s.playback = 2)),
            "Pause" => inner.apply("Pause", Box::new(|s| s.playback = 1)),
            "Stop" => inner.apply("Stop", Box::new(|s| s.playback = 0)),
            "Next" => inner.apply(
                "Next",
                Box::new(|s| {
                    s.track = s.track.saturating_add(1);
                    s.position = 0;
                }),
            ),
            "Previous" => inner.apply(
                "Previous",
                Box::new(|s| {
                    s.track = s.track.saturating_sub(1);
                    s.position = 0;
                }),
            ),
            "Seek" => {
                let len = inner.state.length;
                match request_u32(request, "position") {
                    Some(p) if len == 0 || p <= len => {
                        inner.apply("Seek", Box::new(move |s| s.position = p))
                    }
                    _ => inner.error("Seek", "seek out of range"),
                }
            }

            // Verified: even an unknown element gets an echoed error, connection intact.
            other => inner.error(other, "Unknown command"),
        };

        // A source-driven chain change landing *inside* a client's multi-request conversation. Applied
        // after the reply is built, so this request still sees the chain it asked about and every
        // later one sees the new chain — which is exactly how a change between two requests looks.
        let due = inner
            .chain_switch_after
            .first()
            .is_some_and(|(el, _)| *el == element);
        if due {
            let (_, chain) = inner.chain_switch_after.remove(0);
            inner.state.loaded_chain = chain;
            inner.clamp_to_loaded_chain();
        }

        let mode_due = inner
            .mode_switch_after
            .first()
            .is_some_and(|(el, _)| *el == element);
        if mode_due {
            let (_, mode_index) = inner.mode_switch_after.remove(0);
            inner.state.mode_index = mode_index;
            // Match the measured SetMode side effect: every configured-mode change clears the pin.
            inner.state.rate_index = 0;
            inner.state.active_rate_hz = 0;
            match mode_index {
                1 => inner.state.loaded_chain = LoadedChain::Pcm,
                2 => inner.state.loaded_chain = LoadedChain::Sdm,
                _ => {}
            }
            inner.clamp_to_loaded_chain();
        }

        Some(reply)
    }
}
