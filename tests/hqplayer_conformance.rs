//! HQPlayer native-protocol conformance suite (issue #322).
//!
//! These tests drive the **real** [`HqpAdapter`] over a **real** TCP socket against a fake daemon
//! that speaks the documented wire protocol. That is the whole point: `tests/adapter_integration.rs`
//! tests the HQPlayer mock with a hand-rolled TCP client instead of the adapter, saying so in a
//! comment ("Use reqwest to test the mock directly (not through adapter) because HQP adapter uses a
//! complex TCP protocol"), so no existing test can observe a client protocol defect.
//!
//! Ground rules, all of them load-bearing:
//!
//! * Adapter-facing behaviour is asserted through the adapter's public surface, so a later sans-io
//!   extraction (#162) cannot invalidate those tests. The suite also exercises `framing`, `corpus` and
//!   `DaemonModel` directly, where the adapter cannot reach the mechanism: a test that only checks the
//!   outcome can pass for a different reason than its name gives, which has happened here more than
//!   once.
//! * **No test asserts elapsed wall-clock time.** Timeout and reconnect behaviour is exercised
//!   through the injectable [`HqpTimeouts`] seam and asserted on outcomes and attempt counts.
//! * The suite is hermetic: it needs no HQPlayer. The opt-in real-daemon mode is
//!   [`tier1_live_read_only_verification_when_opted_in`], gated on `UHC_HQP_CONFORMANCE_HOST`, and it
//!   is **not** a smoke check: it captures every read-only protocol family, diffs it against a corpus
//!   profile, and fails on divergence. `merge_gate_pass` additionally requires that every claim ADR 003
//!   lists was actually compared, because a differ that checks nothing also reports no divergences.
//!   Read-only by construction — no `Set*`, no `Volume*`, no transport, no matrix set. Tier 2, the
//!   mutating anchors, is never a merge gate.
//! * Protocol truth is the corpus under `tests/fixtures/hqplayer/`, cross-checked against
//!   the 2026-07-29 salvage reports, which cite
//!   <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/protocol.md>.
//!   That upstream was read **via those reports**, not directly, which is what fixture
//!   `source_chain: read-via-report` records. Current Rust behaviour is never treated as the
//!   specification.

mod mock_servers;

use std::sync::Arc;
use std::sync::Once;
use std::time::Duration;

use unified_hifi_control::adapters::hqplayer::{
    framing, HqpAdapter, HqpRejected, HqpTimeouts, SettingOutcome,
};
use unified_hifi_control::bus::create_bus;

use mock_servers::hqplayer::corpus::{
    self, LEGACY_PROFILE, SYNTHETIC_HAZARD_PROFILE, VERIFIED_PROFILE,
};
use mock_servers::hqplayer::model::{
    request_attr, ActiveModeReporting, DaemonModel, DocumentStyle, FilterFieldReporting,
    LoadedChain, Metadata,
};
use mock_servers::hqplayer::tier1;
use mock_servers::hqplayer::wire::{Chunking, Disruption, Responder, WirePolicy, WireServer};

// =============================================================================
// Harness
// =============================================================================

/// `HqpAdapter::configure` persists to disk. Redirect the config directory at process start so a
/// conformance run can never overwrite a real user's `hqp-config.json`.
fn isolate_config_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("uhc-hqp-conformance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create isolated config dir");
        std::env::set_var("UHC_CONFIG_DIR", &dir);
    });
}

/// Short timeouts so timeout and reconnect paths finish quickly. These bound waiting; no test
/// asserts on how long anything took.
fn fast_timeouts() -> HqpTimeouts {
    HqpTimeouts {
        connect: Duration::from_millis(500),
        response: Duration::from_millis(300),
        reconnect_delay: Duration::from_millis(10),
        max_attempts: 2,
    }
}

/// `Result<SettingOutcome>` collapsed the way every advertised surface collapses it.
///
/// The adapter reports *which* of the five things a write turned out to be; HTTP and MCP both answer
/// `{ok:true}` or `{error}`, so both call [`SettingOutcome::into_applied_result`]. Tests that only
/// care whether a write landed use the same collapse, so they assert what a client would see.
trait Applied {
    fn applied(self) -> anyhow::Result<()>;
}

impl Applied for anyhow::Result<SettingOutcome> {
    fn applied(self) -> anyhow::Result<()> {
        self.and_then(SettingOutcome::into_applied_result)
    }
}

struct Harness {
    server: WireServer,
    model: DaemonModel,
    adapter: HqpAdapter,
}

impl Harness {
    async fn start(profile: &str, policy: WirePolicy, timeouts: HqpTimeouts) -> Self {
        isolate_config_dir();
        let model = DaemonModel::with_profile(profile);
        let server = WireServer::start(Arc::new(model.clone()), policy).await;

        let adapter = HqpAdapter::new(create_bus());
        adapter.set_timeouts(timeouts).await;
        adapter
            .configure(
                "127.0.0.1".to_string(),
                Some(server.port()),
                None,
                None,
                None,
            )
            .await;
        Self {
            server,
            model,
            adapter,
        }
    }

    /// The common case: verified corpus, undisturbed wire, connected.
    async fn verified() -> Self {
        let h = Self::start(VERIFIED_PROFILE, WirePolicy::default(), fast_timeouts()).await;
        h.adapter.connect().await.expect("connect to fake daemon");
        h
    }

    /// Connected, with a wire policy of the test's choosing.
    async fn with_policy(policy: WirePolicy) -> Self {
        let h = Self::start(VERIFIED_PROFILE, policy, fast_timeouts()).await;
        h.adapter.connect().await.expect("connect to fake daemon");
        h
    }

    fn stop(self) {
        self.server.stop();
    }
}

// =============================================================================
// AC1 - stateful daemon coverage: identity, State, Status with metadata child,
//       VolumeRange, modes/filters/shapers/rates, representative advanced settings
// =============================================================================

#[tokio::test]
async fn get_info_reports_the_verified_daemon_identity() {
    let h = Harness::verified().await;
    let info = h.adapter.get_info().await.expect("GetInfo");
    assert_eq!(
        (
            info.name.as_str(),
            info.product.as_str(),
            info.version.as_str(),
            info.engine.as_str()
        ),
        ("Opal", "Signalyst HQPlayer Embedded", "6", "6.0.4"),
        "GetInfo must surface name/product/version/engine as the daemon sends them"
    );
    h.stop();
}

#[tokio::test]
async fn state_reports_settings_as_list_indices() {
    let h = Harness::verified().await;
    let state = h.adapter.get_state().await.expect("State");
    let daemon = h.model.state();
    assert_eq!(
        (
            state.mode as u32,
            state.filter1x.unwrap_or(u32::MAX),
            state.filter_nx.unwrap_or(u32::MAX),
            state.shaper,
            state.rate
        ),
        (
            daemon.mode_index,
            daemon.filter_1x_index,
            daemon.filter_nx_index,
            daemon.shaper_index,
            daemon.rate_index
        ),
        "State's settings fields are list indices and must round-trip unchanged"
    );
    h.stop();
}

#[tokio::test]
async fn state_carries_representative_advanced_settings() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.convolution = true;
        s.repeat = 2;
        s.random = true;
        s.matrix_profile = "Speakers".to_string();
    });
    let state = h.adapter.get_state().await.expect("State");
    assert_eq!(
        (
            state.convolution,
            state.repeat,
            state.random,
            state.matrix_profile.as_str()
        ),
        (true, 2, true, "Speakers"),
        "convolution, repeat, random and matrix_profile must be read from State"
    );
    h.stop();
}

#[tokio::test]
async fn status_with_a_metadata_child_yields_the_playback_fields() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.playback = 2;
        s.position = 42;
        s.length = 215;
        s.metadata = Some(Metadata::sample());
    });
    let status = h.adapter.get_playback_status().await.expect("Status");
    assert_eq!(
        (status.state, status.position, status.length),
        (2, 42, 215),
        "Status fields must survive a document that carries a self-closing metadata child"
    );
    h.stop();
}

#[tokio::test]
async fn status_reports_active_settings_as_display_names() {
    let h = Harness::verified().await;
    let status = h.adapter.get_playback_status().await.expect("Status");
    assert_eq!(
        (status.active_filter.as_str(), status.active_shaper.as_str()),
        ("poly-sinc-lp", "NS9"),
        "Status reports the ACTIVE filter/shaper as strings, unlike State's numeric indices"
    );
    h.stop();
}

#[tokio::test]
async fn volume_range_reports_bounds_and_flags() {
    let h = Harness::verified().await;
    let range = h.adapter.get_volume_range().await.expect("VolumeRange");
    assert_eq!(
        (range.min, range.max, range.enabled, range.adaptive),
        (-60, 0, true, false),
        "VolumeRange must surface min/max bounds and the enabled/adaptive flags"
    );
    h.stop();
}

#[tokio::test]
async fn modes_list_distinguishes_list_index_from_enum_id() {
    let h = Harness::verified().await;
    let modes = h.adapter.get_modes().await.expect("GetModes");
    let source = modes
        .iter()
        .find(|m| m.name == "[source]")
        .expect("[source] mode is present");
    assert_eq!(
        (source.index, source.value),
        (0, -1),
        "[source] sits at list index 0 with enum ID -1: index and value are different domains"
    );
    h.stop();
}

#[tokio::test]
async fn filters_list_is_parsed_in_full_from_a_multiline_container() {
    let h = Harness::verified().await;
    let filters = h.adapter.get_filters().await.expect("GetFilters");
    let expected = corpus::enum_entries(
        &corpus::document(VERIFIED_PROFILE, "filters_pcm"),
        "FiltersItem",
    )
    .len();
    assert_eq!(
        filters.len(),
        expected,
        "every FiltersItem in the container must be parsed, not just the first"
    );
    h.stop();
}

#[tokio::test]
async fn shapers_list_is_parsed_in_full() {
    let h = Harness::verified().await;
    let shapers = h.adapter.get_shapers().await.expect("GetShapers");
    let expected = corpus::enum_entries(
        &corpus::document(VERIFIED_PROFILE, "shapers_pcm"),
        "ShapersItem",
    )
    .len();
    assert_eq!(shapers.len(), expected, "every ShapersItem must be parsed");
    h.stop();
}

#[tokio::test]
async fn rates_list_reports_hz_and_has_no_enum_id() {
    let h = Harness::verified().await;
    let rates = h.adapter.get_rates().await.expect("GetRates");
    assert_eq!(
        rates.iter().map(|r| r.rate).collect::<Vec<_>>(),
        vec![0, 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000],
        "RatesItem carries index and rate (Hz) only; index 0 is rate 0, meaning auto"
    );
    h.stop();
}

#[tokio::test]
async fn enumerations_are_mode_relative_and_are_refetched_after_a_mode_change() {
    let h = Harness::verified().await;
    h.adapter.get_filters().await.expect("PCM filters");
    h.adapter
        .set_mode("SDM (DSD)")
        .await
        .applied()
        .expect("SetMode to SDM");
    let after = h.adapter.get_filters().await.expect("SDM filters");
    let expected = corpus::enum_entries(
        &corpus::document(VERIFIED_PROFILE, "filters_sdm"),
        "FiltersItem",
    )
    .len();
    assert_eq!(
        after.len(),
        expected,
        "GetFilters returns the CURRENT mode's list; a mode change swaps it wholesale"
    );
    h.stop();
}

// =============================================================================
// AC2 - framing: self-closing children, split reads, malformed/truncated,
//       timeouts, reconnect boundaries
// =============================================================================

/// Verified framing rule: a `Status` document carrying a track has a **self-closing** `<metadata/>`
/// child, so the document ends at `</Status>` and not at the child's `/>`. A client that stops at
/// the first `/>` leaves `</Status>` unread, and the *next* command reads that leftover as its own
/// reply — so the failure shows up as a desynchronised stream, not as a bad `Status`.
#[tokio::test]
async fn state_read_after_status_with_metadata_child_reports_the_daemon_state() {
    let h = Harness::with_policy(WirePolicy {
        // Force `</Status>` into a separate TCP segment from the child's `/>`, so the client cannot
        // pass by accident of buffering.
        chunking: Chunking::AfterMarker("/>".to_string()),
        ..WirePolicy::default()
    })
    .await;
    h.model.external_change(|s| {
        s.playback = 2;
        s.metadata = Some(Metadata::sample());
    });

    // Reads the Status document, whose metadata child arrives before the closing tag.
    h.adapter.get_playback_status().await.expect("Status");
    let state = h.adapter.get_state().await.expect("State");

    assert_eq!(
        state.state, 2,
        "the State read after a Status document with a self-closing metadata child must report the \
         daemon's playback state; got {} which means `</Status>` was consumed as this reply. \
         Full state: {:?}",
        state.state, state
    );
    h.stop();
}

#[tokio::test]
async fn a_document_split_mid_attribute_across_tcp_writes_is_still_parsed() {
    let h = Harness::with_policy(WirePolicy {
        // Cut inside the opening tag, before any attribute value is complete.
        chunking: Chunking::AfterMarker("<State st".to_string()),
        ..WirePolicy::default()
    })
    .await;
    h.model.external_change(|s| s.playback = 1);
    let state = h
        .adapter
        .get_state()
        .await
        .expect("State across a split read");
    assert_eq!(
        state.state, 1,
        "a reply split mid-attribute across TCP writes must be reassembled before parsing"
    );
    h.stop();
}

#[tokio::test]
async fn a_truncated_document_fails_instead_of_returning_partial_state() {
    let h = Harness::with_policy(WirePolicy {
        malformed_for_element: Some((
            "State".to_string(),
            "<?xml version=\"1.0\"?><State state=\"2\" mode=".to_string(),
        )),
        ..WirePolicy::default()
    })
    .await;
    let result = h.adapter.get_state().await;
    assert!(
        result.is_err(),
        "a document that never completes must surface an error, not a partially-parsed state: {:?}",
        result.map(|s| s.state)
    );
    h.stop();
}

#[tokio::test]
async fn a_stray_closing_tag_is_rejected_as_malformed() {
    let h = Harness::with_policy(WirePolicy {
        // Exactly the leftover a truncating reader used to produce.
        malformed_for_element: Some(("State".to_string(), "</Status>".to_string())),
        ..WirePolicy::default()
    })
    .await;
    let err = h
        .adapter
        .get_state()
        .await
        .expect_err("a closing tag with nothing open is not a document");
    assert!(
        err.to_string().contains("Malformed"),
        "a stray closing tag must be reported as malformed rather than parsed into defaults, got: {err}"
    );
    h.stop();
}

#[tokio::test]
async fn a_silent_daemon_surfaces_a_timeout_rather_than_hanging() {
    let h = Harness::with_policy(WirePolicy {
        silent_for_element: Some("State".to_string()),
        ..WirePolicy::default()
    })
    .await;
    let result = h.adapter.get_state().await;
    assert!(
        result.is_err(),
        "a daemon that never answers must produce an error bounded by the response timeout"
    );
    h.stop();
}

#[tokio::test]
async fn silencing_volume_does_not_silence_volumeup() {
    // `Volume` and `VolumeUp` are distinct protocol elements. A disruption armed for one must not
    // affect the other. The harness matched the element by substring (`"<Volume"` is a prefix of
    // `"<VolumeUp"`), so silencing `Volume` silently swallowed `VolumeUp`, `VolumeMute` and
    // `VolumeRange` too — a harness that manufactures a false absence, the exact class this boundary
    // exists to prevent. Assert that with `Volume` silenced, an unrelated `VolumeUp` still gets its
    // reply and the call succeeds.
    let h = Harness::with_policy(WirePolicy {
        silent_for_element: Some("Volume".to_string()),
        ..WirePolicy::default()
    })
    .await;
    let result = h.adapter.volume_up().await;
    assert!(
        result.is_ok(),
        "silent_for_element=\"Volume\" must not silence VolumeUp; only an exact element match may. \
         Got {result:?}"
    );
    h.stop();
}

#[tokio::test]
async fn a_silent_daemon_is_retried_exactly_the_configured_number_of_times() {
    let h = Harness::with_policy(WirePolicy {
        silent_for_element: Some("State".to_string()),
        ..WirePolicy::default()
    })
    .await;
    let _ = h.adapter.get_state().await;
    assert_eq!(
        h.server.stats().element_count("State"),
        fast_timeouts().max_attempts,
        "the retry budget is asserted by counting the State requests the daemon saw, never by timing"
    );
    h.stop();
}

#[tokio::test]
async fn a_connection_dropped_mid_command_is_recovered_by_reconnecting() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            disruption: Disruption::DropNextReplyOnce,
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("initial connect");
    h.model.external_change(|s| s.playback = 2);
    // Arm only now, so connection setup is never the thing under test.
    h.server.arm_disruption();

    // The next request vanishes with the connection; the adapter must reconnect and retry.
    let state = h
        .adapter
        .get_state()
        .await
        .expect("State after the daemon dropped the connection");

    assert_eq!(
        (state.state, h.server.stats().connections() >= 2),
        (2, true),
        "after a mid-command drop the adapter must reconnect and return real state; connections \
         seen: {}",
        h.server.stats().connections()
    );
    h.stop();
}

/// An unsolicited `Status` push frame. The daemon emits these of its own accord during playback,
/// which is how two documents come to share one TCP segment for a client that never pipelines.
const PUSH_STATUS_FRAME: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
    "<Status state=\"2\" track=\"3\" position=\"43\" length=\"215\" volume=\"-23.5\"/>"
);

/// Coalescing as it actually reaches a serial client: the daemon emits an unsolicited `Status` push
/// frame in the **same TCP write** as the reply we asked for, so both land in the receive buffer
/// together. The current command must still be answered from its own reply.
#[tokio::test]
async fn a_reply_coalesced_with_an_unsolicited_frame_still_answers_the_current_command() {
    let h = Harness::with_policy(WirePolicy {
        coalesce_extra_for_element: Some(("State".to_string(), PUSH_STATUS_FRAME.to_string())),
        ..WirePolicy::default()
    })
    .await;
    h.model.external_change(|s| s.playback = 2);

    let state = h.adapter.get_state().await.expect("State");
    assert_eq!(
        state.state, 2,
        "the reply to State must be read from the State document, not from whatever else shared \
         its TCP segment"
    );
    h.stop();
}

/// The other half of the same wire condition: the unsolicited frame is still buffered when the next
/// command goes out. A reader that takes the next complete document as its reply answers the
/// following command with the leftover — the same desynchronisation class as the metadata-child
/// defect, arriving by a different route.
///
/// Client expectation: a command is answered by *its own* reply.
#[tokio::test]
async fn an_unsolicited_frame_coalesced_with_a_reply_does_not_corrupt_the_next_command() {
    let h = Harness::with_policy(WirePolicy {
        coalesce_extra_for_element: Some(("State".to_string(), PUSH_STATUS_FRAME.to_string())),
        ..WirePolicy::default()
    })
    .await;

    // Leaves an unsolicited <Status .../> in the client's receive buffer.
    h.adapter.get_state().await.expect("State");
    // A different command, whose reply shape is unmistakably not a Status document.
    let range = h.adapter.get_volume_range().await.expect("VolumeRange");

    assert_eq!(
        range.min, -60,
        "VolumeRange must be answered by the VolumeRange document; min={} is what reading the \
         leftover Status frame produces",
        range.min
    );
    h.stop();
}

/// Framing must not accept a document whose nesting does not close properly. Depth counting alone
/// calls `<State …></Status>` complete, because one element opened and one closed, so a reader that
/// only counts depth hands back a document it cannot vouch for — and may have stopped in the middle
/// of a larger one.
#[tokio::test]
async fn a_document_with_mismatched_nesting_is_rejected_as_malformed() {
    let h = Harness::with_policy(WirePolicy {
        malformed_for_element: Some((
            "State".to_string(),
            "<?xml version=\"1.0\"?><State state=\"2\"></Status>".to_string(),
        )),
        ..WirePolicy::default()
    })
    .await;

    let result = h.adapter.get_state().await;
    assert!(
        result.is_err(),
        "a document whose end tag does not match its start tag must be rejected, not scanned for \
         whatever attributes happen to be present; got state={:?}",
        result.map(|s| s.state)
    );
    h.stop();
}

/// The pure framer, exercised without a socket. This is the layer a later sans-io extraction (#162)
/// would keep, so it is worth pinning independently of the adapter.
#[test]
fn the_framer_finds_the_end_of_a_document_at_every_split_point() {
    let doc = concat!(
        "<?xml version=\"1.0\"?><Status state=\"2\">",
        "<metadata song=\"x\"/>",
        "</Status>"
    );
    let complete_from = doc.find("</Status>").expect("closing tag") + "</Status>".len();
    let misjudged: Vec<usize> = (1..doc.len())
        .filter(|&at| {
            let expect_complete = at >= complete_from;
            (framing::classify(&doc[..at]) == framing::Framing::Complete) != expect_complete
        })
        .collect();
    assert!(
        misjudged.is_empty(),
        "the framer must call the document complete only at or after `</Status>`; wrong at byte \
         offsets {misjudged:?}"
    );
}

#[test]
fn the_framer_ends_a_coalesced_buffer_at_the_first_document() {
    let two =
        "<?xml version=\"1.0\"?><State state=\"2\"/><?xml version=\"1.0\"?><State state=\"0\"/>";
    assert_eq!(
        (
            framing::classify(two),
            framing::root_element(two).as_deref()
        ),
        (framing::Framing::Complete, Some("State")),
        "when two documents arrive together the framer must end at the first, not span both"
    );
}

// =============================================================================
// AC3 - command outcomes: explicit error, syntactic OK without application,
//       delayed application, and changes made by another controller
// =============================================================================

/// Verified daemon behaviour: "a setter can return `result="OK"` without the setting actually
/// applying. Never trust `result="OK"` alone; confirm via `State` readback."
///
/// Client expectation: a setter must not report success it has not confirmed. Reporting success for
/// a change that never happened is the false-success failure the epic forbids outright.
#[tokio::test]
async fn a_setter_accepted_but_not_applied_does_not_report_success() {
    let h = Harness::verified().await;
    h.model
        .arm(|f| f.accept_but_ignore.push("SetFilter".to_string()));

    let result = h
        .adapter
        .set_filter_1x("IIR")
        .await
        .and_then(SettingOutcome::into_applied_result);

    assert!(
        result.is_err(),
        "the daemon answered result=OK but left the filter at index {}; a setter that cannot \
         confirm its change must not report success",
        h.model.state().filter_1x_index
    );
    h.stop();
}

/// The same verified behaviour seen from the other side: a change that lands a poll later is a
/// success, not a failure. Readback must be patient enough to see it.
#[tokio::test]
async fn a_setter_whose_change_lands_after_a_poll_still_reports_success() {
    let h = Harness::verified().await;
    h.model
        .arm(|f| f.apply_after_polls.push(("SetFilter".to_string(), 1)));

    h.adapter
        .set_filter_1x("IIR")
        .await
        .applied()
        .expect("a delayed-but-real change is a success");

    let expected = corpus::index_of(
        &corpus::document(VERIFIED_PROFILE, "filters_pcm"),
        "FiltersItem",
        "IIR",
    )
    .expect("IIR is in the observed list");
    assert_eq!(
        h.model.state().filter_1x_index,
        expected,
        "the delayed change must have landed on the daemon"
    );
    h.stop();
}

#[tokio::test]
async fn an_explicitly_rejected_setter_reports_the_daemon_reason() {
    let h = Harness::verified().await;
    h.model.arm(|f| {
        f.reject_next
            .push(("SetShaping".to_string(), "invalid shaper".to_string()))
    });

    let err = h
        .adapter
        .set_shaper("NS5")
        .await
        .expect_err("an explicit result=Error is a failure");

    assert!(
        err.to_string().contains("invalid shaper"),
        "the daemon's reason text must reach the caller, got: {err}"
    );
    h.stop();
}

#[tokio::test]
async fn a_change_made_by_another_controller_is_visible_on_the_next_read() {
    let h = Harness::verified().await;
    let before = h.adapter.get_state().await.expect("State").shaper;
    // Another HQPlayer controller moves the shaper behind our back.
    h.model.external_change(|s| s.shaper_index = 7);
    let after = h.adapter.get_state().await.expect("State");

    assert_eq!(
        (before, after.shaper),
        (4, 7),
        "state is read from the daemon, so an external change must show up without UHC being told"
    );
    h.stop();
}

/// Retargeted by the HQPTuner amendment (bite 6). It used to drive `set_mode("99")`, which reaches
/// the daemon **only** through the numeric-string fallback in `resolve_mode_index` — a fallback
/// **#347 is going to delete**. So a #322 expectation was quietly depending on production behaviour
/// another issue must remove, and it would have failed for an unrelated reason when that landed.
///
/// It now uses the low-level `set_filter`, which takes an index directly and resolves no name, so the
/// rejection comes from the daemon rather than from a fallback.
///
/// Also renamed for accuracy: `set_mode("99")` sent a *known* element (`SetMode`) with an invalid
/// value, so this never exercised the model's unknown-*element* arm. That arm is reachable only by
/// sending an element no adapter method emits, so nothing covers it — recorded rather than papered
/// over.
#[tokio::test]
async fn a_rejected_setter_is_reported_as_an_error_without_dropping_the_connection() {
    let h = Harness::verified().await;
    let beyond_the_list = 9_999;

    // Low-level: takes indices, resolves no name, so this cannot depend on name-resolution
    // fallbacks in either direction. Both sides carry the same out-of-range number because
    // `SetFilter` writes both sides on the wire and the client can no longer express a one-sided
    // request (#347).
    let rejected = h.adapter.set_filter(beyond_the_list, beyond_the_list).await;
    // The connection must still be usable afterwards.
    let state_after = h.adapter.get_state().await;

    assert_eq!(
        (rejected.is_err(), state_after.is_ok()),
        (true, true),
        "an error response is reported but the connection survives it"
    );
    h.stop();
}

// =============================================================================
// AC4 - volume: fractional negative dB, fixed volume, adaptive volume,
//       min/max/step, mute
// =============================================================================

#[tokio::test]
async fn a_fractional_negative_db_volume_round_trips() {
    let h = Harness::verified().await;
    h.adapter
        .set_volume_db(-23.5)
        .await
        .expect("Volume accepts a double");

    let state = h.adapter.get_state().await.expect("State");
    assert_eq!(
        (h.model.state().volume_db, state.volume_db),
        (-23.5, -23.5),
        "a fractional negative dB level must survive the round trip in both directions"
    );
    h.stop();
}

#[tokio::test]
async fn a_whole_db_volume_is_not_turned_into_a_fraction_on_the_wire() {
    let h = Harness::verified().await;
    h.adapter.set_volume(-30).await.expect("Volume");
    let sent = h
        .model
        .last_request("Volume")
        .and_then(|line| request_attr(&line, "value"));
    assert_eq!(
        sent.as_deref(),
        Some("-30"),
        "a whole number of dB is sent without a decimal part, as the reference client does"
    );
    h.stop();
}

#[tokio::test]
async fn a_rounded_volume_is_never_reported_as_zero_db() {
    let h = Harness::verified().await;
    h.model.external_change(|s| s.volume_db = -23.5);
    let state = h.adapter.get_state().await.expect("State");
    assert_eq!(
        state.volume, -24,
        "the integer projection of -23.5 dB must round, never fall back to 0 dB, which is maximum \
         output"
    );
    h.stop();
}

#[tokio::test]
async fn a_fixed_volume_daemon_rejects_a_volume_change() {
    let h = Harness::verified().await;
    h.model.external_change(|s| s.volume_range.enabled = false);

    let result = h.adapter.set_volume_db(-10.0).await;
    assert!(
        result.is_err(),
        "a daemon with volume control disabled answers result=Error with no reason text, and the \
         level is left unchanged, so the call must not report success"
    );
    h.stop();
}

#[tokio::test]
async fn a_fixed_volume_daemon_reports_volume_as_unavailable() {
    let h = Harness::verified().await;
    h.model.external_change(|s| s.volume_range.enabled = false);
    let range = h.adapter.get_volume_range().await.expect("VolumeRange");
    assert!(
        !range.enabled,
        "VolumeRange.enabled=0 is how a fixed-volume daemon advertises itself"
    );
    h.stop();
}

#[tokio::test]
async fn an_adaptive_volume_daemon_reports_the_adaptive_flag() {
    let h = Harness::verified().await;
    h.model.external_change(|s| s.volume_range.adaptive = true);
    let range = h.adapter.get_volume_range().await.expect("VolumeRange");
    assert!(
        range.adaptive,
        "adaptive volume is advertised on VolumeRange and must be surfaced"
    );
    h.stop();
}

#[tokio::test]
async fn a_fractional_volume_step_is_preserved_rather_than_rounded_away() {
    let h = Harness::verified().await;
    let range = h.adapter.get_volume_range().await.expect("VolumeRange");
    assert_eq!(
        range.step_db,
        Some(0.5),
        "a 0.5 dB step must survive as a fraction; rounding it to 1 dB doubles every adjustment"
    );
    h.stop();
}

#[tokio::test]
async fn a_volume_range_that_omits_step_reports_it_as_absent() {
    let h = Harness::verified().await;
    // The verified live sample carries no step attribute at all.
    h.model.external_change(|s| s.volume_range.step_db = None);
    let range = h.adapter.get_volume_range().await.expect("VolumeRange");
    assert_eq!(
        range.step_db, None,
        "an absent step must be reported as absent, not invented"
    );
    h.stop();
}

#[tokio::test]
async fn a_volume_below_the_daemon_floor_is_rejected() {
    let h = Harness::verified().await;
    let result = h.adapter.set_volume_db(-90.0).await;
    assert!(
        result.is_err(),
        "the daemon's floor is -60 dB; a level below it is refused and must not read as success"
    );
    h.stop();
}

#[tokio::test]
async fn mute_is_absolute_and_idempotent_on_the_daemon() {
    // Verified live against a real HQPlayer 6.0.2 Embedded daemon (issue #322 live validation):
    // VolumeMute drives the output to the volume floor and is idempotent - a second and third call
    // keep it at the floor, it never toggles back, and State exposes no separate mute flag. Unmute is a
    // separate absolute `Volume` write, not a second VolumeMute.
    let h = Harness::verified().await;
    let floor = h.model.state().volume_range.min_db;
    h.adapter.volume_mute().await.expect("VolumeMute");
    let after_first = h.model.state().volume_db;
    h.adapter.volume_mute().await.expect("VolumeMute");
    let after_second = h.model.state().volume_db;
    assert_eq!(
        (after_first, after_second),
        (floor, floor),
        "VolumeMute is an absolute mute-to-floor and idempotent on the daemon, not a toggle"
    );
    h.stop();
}

#[tokio::test]
async fn a_volume_step_moves_the_level_by_the_advertised_step() {
    let h = Harness::verified().await;
    h.model.external_change(|s| s.volume_db = -20.0);
    h.adapter.volume_up().await.expect("VolumeUp");
    assert_eq!(
        h.model.state().volume_db,
        -19.5,
        "VolumeUp moves by the daemon's own 0.5 dB step"
    );
    h.stop();
}

// =============================================================================
// AC7 (continued) - the persistent-configuration HTTP lane's response family
// =============================================================================
//
// Covered as a **corpus contract** rather than by driving production parsing, deliberately:
//
// * The profile-list parse already has coverage through the public surface: the
//   `GET /hqplayer/profiles` route is exercised in `tests/protocol_integration.rs`. Re-asserting the
//   same behaviour here would duplicate that coverage and would mean widening production visibility
//   purely for test reach, which #322 does not need and should not do.
// * What #322 does owe this lane is an executable record of the verified response-family semantics:
//   which fields the read side carries, and the fact that the write side's HTTP 200 carries no
//   outcome at all. Both are properties of the documents, so the corpus is where they belong.
// * The restore transport itself - multipart upload, daemon self-restart, `/backup` readback
//   polling - is issue #330's, and nothing here implements or presumes it.

#[test]
fn the_persistent_config_form_carries_the_verified_field_names() {
    let page = corpus::document(VERIFIED_PROFILE, "config_profile_form");
    let missing: Vec<&str> = ["name=\"profile\"", "name=\"profile_name\""]
        .into_iter()
        .filter(|needle| !page.contains(needle))
        .collect();
    assert!(
        missing.is_empty(),
        "the /config read side is identified by its verified field names: a `profile` select of \
         existing configurations and a `profile_name` box for save-as-new; missing {missing:?}"
    );
}

#[test]
fn the_persistent_config_form_separates_the_unnamed_base_from_named_profiles() {
    let page = corpus::document(VERIFIED_PROFILE, "config_profile_form");
    let offered: Vec<&str> = [
        "value=\"[default]\"",
        "value=\"Speakers\"",
        "value=\"Headphones\"",
    ]
    .into_iter()
    .filter(|needle| page.contains(needle))
    .collect();
    assert_eq!(
        offered.len(),
        3,
        "the profile select offers the unnamed base configuration `[default]` alongside the named \
         ones, and the distinction matters: loading a NAMED profile restarts the daemon and empties \
         /backup/settings.zip, while `[default]` does not. Found {offered:?}"
    );
}

/// Verified rule for the persistent WRITE lane: "the 200 response body is the HTML restore page —
/// success is confirmed by a `/backup` readback, never by the POST". A rejected `POST /config` also
/// answers 200, with `Failed!` in the body and nothing written.
///
/// So this response family carries no machine-readable outcome, and pinning that is what stops a
/// later implementation treating HTTP 200 as proof.
#[test]
fn the_restore_response_family_carries_no_outcome_signal() {
    let page = corpus::document(VERIFIED_PROFILE, "restore_response");
    let false_signals: Vec<&str> = ["result=", "name=\"profile\"", "Failed!", "Success"]
        .into_iter()
        .filter(|needle| page.contains(needle))
        .collect();
    assert!(
        false_signals.is_empty(),
        "a restore response is a form page, not a result: it carries no outcome a client could read, \
         so success must come from a readback. Found {false_signals:?}"
    );
}

#[test]
fn the_restore_fixture_records_why_its_status_code_proves_nothing() {
    let fixture = corpus::load(VERIFIED_PROFILE, "restore_response");
    assert!(
        fixture.provenance.notes.contains("readback"),
        "the restore fixture must record why a 200 is not success, so the next reader does not have \
         to rediscover it; notes: {:?}",
        fixture.provenance.notes
    );
}

// =============================================================================
// Further State parsing the wire reference pins down
// =============================================================================

#[tokio::test]
async fn the_stop_requested_playback_state_is_reported_faithfully() {
    let h = Harness::verified().await;
    // The reference documents four playback states: 0 stopped, 1 paused, 2 playing,
    // 3 stop requested.
    h.model.external_change(|s| s.playback = 3);
    let state = h.adapter.get_state().await.expect("State");
    assert_eq!(
        state.state, 3,
        "state=3 (stop requested) must reach the caller intact rather than collapsing to 0"
    );
    h.stop();
}

#[tokio::test]
async fn the_junk_filter_is_read_as_a_list_index_not_a_boolean() {
    let h = Harness::verified().await;
    h.model.external_change(|s| s.filter_junk_index = 2);
    let state = h.adapter.get_state().await.expect("State");
    assert_eq!(
        state.filter_junk, 2,
        "State.filter_junk is an int index into GetJunkFilters, so a third option must be \
         distinguishable from the first two"
    );
    h.stop();
}

// =============================================================================
// AC5 - semantic name to native index, from observed list/state pairs
// =============================================================================

#[tokio::test]
async fn a_filter_name_is_sent_as_the_index_the_observed_list_gives_it() {
    let h = Harness::verified().await;
    let filters = corpus::document(VERIFIED_PROFILE, "filters_pcm");
    // Deliberately a filter the daemon is NOT already on. Since #347 a setter whose authoritative
    // field already reads the requested value is `AlreadySet` and nothing goes on the wire, so
    // requesting the current selection would assert about a request that was never sent.
    let index = corpus::index_of(&filters, "FiltersItem", "poly-sinc-xtr").expect("observed index");

    h.adapter
        .set_filter_1x("poly-sinc-xtr")
        .await
        .applied()
        .expect("SetFilter");

    let sent = h
        .model
        .last_request("SetFilter")
        .and_then(|line| request_attr(&line, "value1x"))
        .and_then(|v| v.parse::<u32>().ok());
    assert_eq!(
        sent,
        Some(index),
        "the 1x filter must be sent as the list index the observed GetFilters gives that name"
    );
    h.stop();
}

#[tokio::test]
async fn a_filter_name_is_not_sent_as_its_enum_id() {
    let h = Harness::verified().await;
    let filters = corpus::document(VERIFIED_PROFILE, "filters_pcm");
    let enum_id = corpus::enum_id_of(&filters, "FiltersItem", "poly-sinc-lp").expect("enum id");

    h.adapter
        .set_filter_1x("poly-sinc-lp")
        .await
        .applied()
        .expect("SetFilter");

    let sent = h
        .model
        .last_request("SetFilter")
        .and_then(|line| request_attr(&line, "value1x"))
        .and_then(|v| v.parse::<i32>().ok());
    assert_ne!(
        sent,
        Some(enum_id),
        "poly-sinc-lp has enum ID {enum_id} and list index 6; sending the enum ID would select a \
         different filter"
    );
    h.stop();
}

#[tokio::test]
async fn the_same_filter_name_resolves_to_a_different_index_on_a_differently_ordered_daemon() {
    // The legacy profile is UNVERIFIED and is used only to vary list ORDER: it must never be
    // treated as authoritative protocol truth.
    let h = Harness::start(LEGACY_PROFILE, WirePolicy::default(), fast_timeouts()).await;
    h.adapter.connect().await.expect("connect");
    let legacy_index = corpus::index_of(
        &corpus::document(LEGACY_PROFILE, "filters_pcm"),
        "FiltersItem",
        "poly-sinc-lp",
    )
    .expect("legacy index");

    h.adapter
        .set_filter_1x("poly-sinc-lp")
        .await
        .applied()
        .expect("SetFilter");

    let sent = h
        .model
        .last_request("SetFilter")
        .and_then(|line| request_attr(&line, "value1x"))
        .and_then(|v| v.parse::<u32>().ok());
    assert_eq!(
        sent,
        Some(legacy_index),
        "name resolution must follow the daemon's own list, so a reordered list changes the index \
         sent while the requested name stays the same"
    );
    h.stop();
}

// =============================================================================
// AC6 - cross-lane: live wire speaks list index, persistent config stores enum ID
// =============================================================================

#[test]
fn the_persistent_configuration_lane_stores_enum_ids_not_list_indices() {
    let config = corpus::document(VERIFIED_PROFILE, "persistent_config");
    let filters = corpus::document(VERIFIED_PROFILE, "filters_pcm");
    let stored: i32 = corpus::config_attr(&config, "output", "filter")
        .expect("filter attribute")
        .parse()
        .expect("numeric");

    let name = corpus::enum_entries(&filters, "FiltersItem")
        .into_iter()
        .find(|e| e.enum_id == Some(stored))
        .map(|e| e.name)
        .expect("the stored number resolves as an ENUM ID");

    assert_eq!(
        name, "poly-sinc-gauss-long",
        "hqplayerd.xml stores enum IDs: filter={stored} is poly-sinc-gauss-long"
    );
}

#[test]
fn the_two_lanes_give_the_same_filter_name_different_numbers() {
    let filters = corpus::document(VERIFIED_PROFILE, "filters_pcm");
    let index = corpus::index_of(&filters, "FiltersItem", "poly-sinc-gauss-long").expect("index");
    let enum_id = corpus::enum_id_of(&filters, "FiltersItem", "poly-sinc-gauss-long").expect("id");
    assert_ne!(
        i64::from(index),
        i64::from(enum_id),
        "poly-sinc-gauss-long is list index {index} on the wire and enum ID {enum_id} in the config \
         file; a conversion that served both lanes would be wrong in one of them"
    );
}

#[tokio::test]
async fn feeding_a_persistent_enum_id_to_the_live_lane_is_rejected() {
    let h = Harness::verified().await;
    let config = corpus::document(VERIFIED_PROFILE, "persistent_config");
    let stored: u32 = corpus::config_attr(&config, "output", "filter")
        .expect("filter attribute")
        .parse()
        .expect("numeric");

    // Both sides carry the number: `SetFilter` writes both on the wire, and since #347 the client
    // cannot express a one-sided call at all.
    let result = h.adapter.set_filter(stored, stored).await;
    assert!(
        result.is_err(),
        "enum ID {stored} is not a valid list index for this daemon's 12-entry list; sending a \
         persistent-lane number on the live lane must not silently succeed"
    );
    h.stop();
}

// =============================================================================
// AC7 - executable examples for the transport, seek, pipeline and
//       persistent-configuration response families
// =============================================================================

#[tokio::test]
async fn the_transport_family_moves_the_daemon_between_playback_states() {
    let h = Harness::verified().await;
    h.adapter.play().await.expect("Play");
    let playing = h.model.state().playback;
    h.adapter.pause().await.expect("Pause");
    let paused = h.model.state().playback;
    h.adapter.stop().await.expect("Stop");
    let stopped = h.model.state().playback;
    assert_eq!(
        (playing, paused, stopped),
        (2, 1, 0),
        "Play/Pause/Stop must each be applied by the daemon"
    );
    h.stop();
}

#[tokio::test]
async fn the_track_change_family_advances_and_rewinds_the_queue() {
    let h = Harness::verified().await;
    h.model.external_change(|s| s.track = 5);
    h.adapter.next().await.expect("Next");
    let after_next = h.model.state().track;
    h.adapter.previous().await.expect("Previous");
    let after_previous = h.model.state().track;
    assert_eq!(
        (after_next, after_previous),
        (6, 5),
        "Next and Previous must move the daemon's track cursor"
    );
    h.stop();
}

#[tokio::test]
async fn the_seek_family_moves_the_playback_position() {
    let h = Harness::verified().await;
    h.model.external_change(|s| s.length = 215);
    h.adapter.seek(42).await.expect("Seek");
    assert_eq!(
        h.model.state().position,
        42,
        "Seek must carry the requested position to the daemon"
    );
    h.stop();
}

#[tokio::test]
async fn the_pipeline_family_resolves_indices_back_to_display_names() {
    let h = Harness::verified().await;
    let pipeline = h.adapter.get_pipeline_status().await.expect("pipeline");
    assert_eq!(
        (
            pipeline.status.mode.as_str(),
            pipeline.settings.filter1x.selected.value.as_str(),
            pipeline.settings.shaper.selected.value.as_str()
        ),
        ("PCM", "poly-sinc-lp", "NS9"),
        "the pipeline view must turn State's list indices back into names via the observed lists"
    );
    h.stop();
}

#[tokio::test]
async fn the_matrix_profile_family_round_trips_a_name_containing_an_entity() {
    let h = Harness::verified().await;
    let profiles = h.adapter.get_matrix_profiles().await.expect("profiles");
    assert!(
        profiles.iter().any(|p| p.name == "Rock & Roll"),
        "an attribute value carrying an escaped ampersand must be decoded, got {:?}",
        profiles.iter().map(|p| &p.name).collect::<Vec<_>>()
    );
    h.stop();
}

// =============================================================================
// AC8 - hermetic in CI, with a documented opt-in real-daemon mode (the tier-1 merge gate, not a
// smoke check: it captures every read-only family and diffs it against the corpus)
// =============================================================================

/// Opt-in **tier-1 read-only live verification** — the real-daemon merge gate from ADR 003.
///
/// ```text
/// UHC_HQP_CONFORMANCE_HOST=192.168.1.50 \
///   cargo test --test hqplayer_conformance -- --nocapture tier1_live
/// ```
///
/// Optional environment:
/// * `UHC_HQP_CONFORMANCE_PORT` — control port, default `4321`
/// * `UHC_HQP_CONFORMANCE_PROFILE` — corpus profile to diff against, default the verified one
/// * `UHC_HQP_CONFORMANCE_HARDWARE` — required when the selected profile carries a hardware marker;
///   must match that marker so choosing a profile cannot masquerade as hardware evidence
/// * `UHC_HQP_CONFORMANCE_WEB_PORT` — HTTP port, default `8088`
/// * `UHC_HQP_CONFORMANCE_WEB_USER` / `_PASS` — Digest credentials; supply both to include the
///   `/config` read side. Omit them and that lane is recorded as not-captured
///
/// Without `UHC_HQP_CONFORMANCE_HOST` this prints that it skipped and passes, so the default suite
/// stays hermetic without leaving an acceptance test permanently `#[ignore]`d.
///
/// **Read-only.** Every call is a query — no `Set*`, no `Volume*`, no transport, no matrix set. Safe
/// against a daemon someone is listening to. It fails on divergence, because a divergence means the
/// corpus needs re-provenancing before it can be trusted as protocol truth, and it prints the whole
/// report either way so the operator can act on it.
#[tokio::test]
async fn tier1_live_read_only_verification_when_opted_in() {
    let Ok(host) = std::env::var("UHC_HQP_CONFORMANCE_HOST") else {
        eprintln!(
            "skipping tier-1 live verification: set UHC_HQP_CONFORMANCE_HOST to opt in. \
             Read-only; it captures every read-only protocol family and diffs it against the corpus."
        );
        return;
    };
    // Misconfiguration fails the gate instead of quietly changing what it verifies. Both ports are
    // strict about an *explicit* value and silent about an absent one, so the hermetic default is
    // unchanged.
    let port = tier1_port("UHC_HQP_CONFORMANCE_PORT", 4321).unwrap_or_else(|e| panic!("{e}"));
    let web_port =
        tier1_port("UHC_HQP_CONFORMANCE_WEB_PORT", 8088).unwrap_or_else(|e| panic!("{e}"));
    let profile = std::env::var("UHC_HQP_CONFORMANCE_PROFILE")
        .unwrap_or_else(|_| VERIFIED_PROFILE.to_string());
    tier1_hardware_marker(
        &profile,
        std::env::var("UHC_HQP_CONFORMANCE_HARDWARE")
            .ok()
            .as_deref(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let (user, pass) = tier1_credentials(
        std::env::var("UHC_HQP_CONFORMANCE_WEB_USER").ok(),
        std::env::var("UHC_HQP_CONFORMANCE_WEB_PASS").ok(),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    isolate_config_dir();
    let adapter = HqpAdapter::new(create_bus());
    adapter
        .configure(host.clone(), Some(port), Some(web_port), user, pass)
        .await;
    adapter
        .connect()
        .await
        .unwrap_or_else(|e| panic!("connect to HQPlayer at {host}:{port}: {e}"));

    let capture = tier1::capture(&adapter)
        .await
        .unwrap_or_else(|e| panic!("tier-1 capture from {host}:{port}: {e}"));
    let report = tier1::diff(&capture, &profile);

    // Printed unconditionally: the report is the deliverable, not the pass/fail bit. Both forms go
    // out — the render for whoever is in the room, the marker-bracketed artifact for CI to store and
    // a later run to diff against.
    eprintln!("\n{}", report.render());
    eprintln!("\n{}", report.artifact_block());
    if let Ok(path) = std::env::var("UHC_HQP_CONFORMANCE_ARTIFACT") {
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&report.to_json()).expect("serialise"),
        )
        .unwrap_or_else(|e| panic!("write tier-1 artifact to {path}: {e}"));
        eprintln!("tier-1 artifact written to {path}");
    }

    // `merge_gate_pass`, not `is_clean`. "Clean" only means the differ found nothing, which a differ
    // that compared nothing also satisfies — a run where the raw lane observed nothing at all would
    // `warn!` each family and report zero divergences. The gate additionally requires that every claim
    // ADR 003 lists was actually compared and that nothing required is left unverified, which is the
    // property this module's own header already claims for it.
    assert!(
        report.merge_gate_pass(),
        "tier-1 did not pass the merge gate against corpus profile `{profile}`: {} divergence(s) and \
         {:?} unverified claim(s). Each divergence must be resolved by re-provenancing the fixture \
         from this capture or by shipping it as a stated gap - not by loosening the differ. An \
         unverified claim means the run never compared it, which is not a pass.",
        report.divergences.len(),
        report.unverified
    );
}

// =============================================================================
// AC9 - provenance travels with every fixture
// =============================================================================

#[test]
fn every_corpus_fixture_records_its_provenance() {
    let missing: Vec<String> = corpus::profiles()
        .iter()
        .flat_map(|p| corpus::all_in(p))
        .filter(|f| f.provenance.source.is_empty() || f.provenance.daemon.is_empty())
        .map(|f| f.name)
        .collect();
    assert!(
        missing.is_empty(),
        "every fixture must record source and daemon so a live-observed fact is distinguishable \
         from a transcription; missing: {missing:?}"
    );
}

#[test]
fn the_legacy_profile_is_marked_unverified_so_it_cannot_pass_as_protocol_truth() {
    let unverified: Vec<String> = corpus::all_in(LEGACY_PROFILE)
        .into_iter()
        .filter(|f| !f.provenance.is_verified())
        .map(|f| f.name)
        .collect();
    assert_eq!(
        unverified.len(),
        corpus::all_in(LEGACY_PROFILE).len(),
        "no fixture in the source-derived 5.x profile may claim verified status; unverified: \
         {unverified:?}"
    );
}

#[test]
fn the_verified_profile_marks_excerpts_honestly() {
    // Words a fixture uses when it is admitting its content was constructed rather than captured.
    const ADMISSIONS: [&str; 4] = [
        "excerpt",
        "representative",
        "illustrative",
        "not a byte-for-byte capture",
    ];
    let overclaimed: Vec<(String, String)> = corpus::all_in(VERIFIED_PROFILE)
        .into_iter()
        .filter(|f| {
            let notes = f.provenance.notes.to_lowercase();
            // Guard everything `is_verified()` accepts, not just the bare `verified` status: any
            // `verified*` label is read as a verification claim elsewhere in the corpus, so the
            // honesty check has to cover the same set or a `verified-shape` fixture slips through.
            f.provenance.is_verified() && ADMISSIONS.iter().any(|a| notes.contains(a))
        })
        .map(|f| (f.name, f.provenance.status))
        .collect();
    assert!(
        overclaimed.is_empty(),
        "a fixture whose notes admit its content was constructed must not claim any `verified` \
         status — only a byte-for-byte capture may. Use a `derived-*` status and say in the notes \
         which property is verified. Overclaimed: {overclaimed:?}"
    );
}

// =============================================================================
// Document-style coverage: both legal layouts must behave identically
// =============================================================================

#[tokio::test]
async fn a_compact_single_line_container_is_parsed_the_same_as_a_multiline_one() {
    let h = Harness::verified().await;
    let multiline = h.adapter.get_filters().await.expect("multiline container");
    h.model.set_style(DocumentStyle::Compact);
    let compact = h.adapter.get_filters().await.expect("compact container");
    assert_eq!(
        compact.len(),
        multiline.len(),
        "container layout is a wire detail; both legal shapes must parse to the same list"
    );
    h.stop();
}

// =============================================================================
// Guards on the conformance seam itself
// =============================================================================

/// `set_timeouts` has to be `pub` for an integration-test crate to reach it, which means nothing in
/// the type system stops production code from quietly retuning retry behaviour through it. This
/// lint closes that gap the way the repo already closes similar ones (`tests/architecture_lint.rs`,
/// `tests/arbitrary_find_lint.rs`): the seam exists for the suite, and `src/` must not use it.
#[test]
fn no_production_code_retunes_the_timeout_seam() {
    let callers: Vec<String> = walkdir::WalkDir::new("src")
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("rs"))
        .filter_map(|e| {
            let body = std::fs::read_to_string(e.path()).ok()?;
            body.contains(".set_timeouts(")
                .then(|| e.path().display().to_string())
        })
        .collect();
    assert!(
        callers.is_empty(),
        "set_timeouts is the conformance suite's seam; production defaults must stay the shipped \
         constants. Called from {callers:?}"
    );
}

/// Found by stage-2 dissent, not by the tests: `verify_applied` compares a `State` readback against
/// the value *we* asked for, so it cannot tell "the daemon dropped our change" from "the daemon took
/// our change and another controller then moved it". Epic #311 requires both multi-controller
/// operation and no false success, so the semantics have to be pinned rather than left to chance.
///
/// Decision pinned here: the setter **fails**, because it genuinely cannot confirm the state it was
/// asked to produce — and the error names the value the daemon actually reports, so an operator can
/// tell an override apart from a silently dropped command.
#[tokio::test]
async fn a_setter_overridden_by_another_controller_fails_and_names_the_observed_value() {
    let h = Harness::verified().await;
    // Our SetShaping is acknowledged but not applied...
    h.model
        .arm(|f| f.accept_but_ignore.push("SetShaping".to_string()));
    // ...and another controller moves the same setting to something else entirely.
    h.model.external_change(|s| s.shaper_index = 7);

    let err = h
        .adapter
        .set_shaper("NS5")
        .await
        .and_then(SettingOutcome::into_applied_result)
        .expect_err("a setting we cannot confirm is not a success, however it came to differ");

    assert!(
        err.to_string().contains('7'),
        "the error must name the value the daemon actually reports, so an override is \
         distinguishable from a dropped command; got: {err}"
    );
    h.stop();
}

/// The skip bound has to clear the daemon's real push cadence with margin, not by luck. Verified
/// behaviour during active playback is a steady ~1-2 Hz of unsolicited `Status` frames; the shipped
/// response timeout is 3 s, so one slow command can legitimately have ~6 frames land behind it. A
/// bound of 8 leaves almost no margin.
///
/// Exceeding it is not fatal, because `send_command`'s retry loop reconnects and the fresh socket
/// has no backlog — but that is recovery by accident, and it costs a dropped connection per burst.
///
/// Client expectation: a burst comfortably larger than one response window's worth of push frames is
/// skipped **on the existing connection**, without reconnecting.
#[tokio::test]
async fn a_burst_of_unsolicited_frames_is_skipped_without_dropping_the_connection() {
    // 12 frames ~ six seconds of 2 Hz push, i.e. twice a response window. Newline-separated to match
    // what the daemon emits. Each is skipped and drained individually now: the reader drops one
    // document's bytes and re-examines the remainder rather than clearing the buffer, so the count
    // reflects documents rather than reads. (An earlier revision cleared the whole buffer per skip,
    // which meant several documents sharing one line cost a single skip and hid the bound; the
    // newline separation predates the fix and is kept because it is also what the wire does.)
    let burst = vec![PUSH_STATUS_FRAME; 12].join("\n");
    let h = Harness::with_policy(WirePolicy {
        coalesce_extra_for_element: Some(("State".to_string(), burst)),
        ..WirePolicy::default()
    })
    .await;
    let connections_before = h.server.stats().connections();

    // Leaves 12 unsolicited <Status .../> documents buffered ahead of the next reply.
    h.adapter.get_state().await.expect("State");
    let range = h.adapter.get_volume_range().await.expect("VolumeRange");

    assert_eq!(
        (
            range.min,
            h.server.stats().connections() - connections_before
        ),
        (-60, 0),
        "a 12-frame push burst must be skipped on the live connection; a new connection means the \
         skip bound was exhausted and the retry loop papered over it. min={}, new connections={}",
        range.min,
        h.server.stats().connections() - connections_before
    );
    h.stop();
}

/// Previously, `response` bounded a single *read*, so every skipped document reset it: a daemon that
/// streamed unsolicited `Status` frames and never answered kept the command alive for as long as the
/// stream lasted — at a verified 1-2 Hz that is tens of seconds, far worse than the plain no-reply
/// case it was meant to protect. `response` is now a whole-command deadline, and this expectation is
/// what holds it that way.
///
/// Client expectation: unsolicited traffic cannot extend how long a command waits. Asserted on the
/// **count** of frames the client consumed, not on elapsed time: bounded by the deadline, that count
/// stays small, whereas a per-read reset lets it run to whatever ceiling the skip logic allows.
#[tokio::test]
async fn continuous_unsolicited_traffic_cannot_extend_the_command_deadline() {
    let h = Harness::with_policy(WirePolicy {
        unsolicited_stream_for_element: Some((
            "VolumeRange".to_string(),
            PUSH_STATUS_FRAME.to_string(),
            Duration::from_millis(20),
        )),
        ..WirePolicy::default()
    })
    .await;
    let before = h.server.stats().replies();

    let result = h.adapter.get_volume_range().await;
    let consumed = h.server.stats().replies() - before;

    assert!(
        result.is_err() && consumed < 60,
        "a command that is never answered must be bounded by its own deadline, not by the length of \
         the push stream. err={} frames_streamed={consumed} (a 300ms deadline at one frame per 20ms \
         should consume well under 60 across both attempts)",
        result.is_err()
    );
    h.stop();
}

// =============================================================================
// Tier-1 read-only live verification (ADR 003) — exercised hermetically here
// =============================================================================

/// The differ is the whole value of tier 1, so it has to be proven to *detect* rather than assumed.
/// Run against the fake serving the very corpus it is diffing, a clean run must find nothing.
#[tokio::test]
async fn tier1_finds_no_divergence_when_the_daemon_serves_the_corpus_it_is_diffed_against() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter)
        .await
        .expect("capture the fake daemon");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    assert!(
        report.is_clean(),
        "a daemon serving this exact corpus must produce no divergences, else the differ is \
         reporting noise:\n{}",
        report.render()
    );
    h.stop();
}

/// The other half, and the one that matters: a daemon whose lists disagree with the corpus must be
/// caught, naming the family, the entry and both numbers. The legacy profile reorders the same filter
/// names deliberately, so diffing a legacy daemon against the 6.0.4 corpus is a known mismatch.
#[tokio::test]
async fn tier1_reports_index_divergence_when_the_daemon_orders_a_list_differently() {
    let h = Harness::start(LEGACY_PROFILE, WirePolicy::default(), fast_timeouts()).await;
    h.adapter.connect().await.expect("connect");
    let capture = tier1::capture(&h.adapter).await.expect("capture");

    let report = tier1::diff(&capture, VERIFIED_PROFILE);

    let index_mismatches: Vec<&tier1::Divergence> = report
        .divergences
        .iter()
        .filter(|d| d.kind == tier1::DivergenceKind::IndexMismatch)
        .collect();
    assert!(
        !index_mismatches.is_empty()
            && index_mismatches
                .iter()
                .any(|d| d.detail.contains("poly-sinc-lp")),
        "poly-sinc-lp sits at index 2 on the legacy daemon and index 6 in the 6.0.4 corpus; tier 1 \
         must report that as an index divergence naming both numbers. Got:\n{}",
        report.render()
    );
    h.stop();
}

/// A tier-1 report is only useful if it is honest about what it could not reach. A read-only run
/// cannot see the inactive mode's lists, because reaching them needs `SetMode`.
#[tokio::test]
async fn tier1_records_the_inactive_mode_lists_as_not_captured() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    assert!(
        report
            .not_captured
            .iter()
            .any(|n| n.contains("SetMode") && n.contains("tier 2")),
        "the report must say the inactive mode's lists were not reached and why, so a clean run \
         cannot imply per-mode coverage it did not have. not_captured={:?}",
        report.not_captured
    );
    h.stop();
}

/// Under a configured `[source]` mode the loaded chain is material-dependent — it can be PCM or SDM
/// depending on what the source is feeding — so the configured mode index does not identify the
/// chain. `diff` used to fall through `active_mode_index != 2` to the PCM corpus, which meant a
/// `[source]` daemon serving the SDM chain was diffed against PCM and every entry read as a
/// divergence. That is a false absence manufactured by the differ, the exact class this gate exists
/// to prevent. Under `[source]` the mode-relative families must be recorded not-captured/unverified
/// with a reason, never compared against a guessed chain; mode-independent families (matrix) still
/// compare. (CodeRabbit review at `bc9158e`, finding 4.)
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn tier1_does_not_diff_mode_relative_lists_under_source() {
    let h = Harness::verified().await;
    // Configure `[source]` (mode index 0) with the SDM chain actually loaded — the case the finding
    // describes: the daemon reports mode 0 while serving SDM enumerations.
    h.model.external_change(|s| s.mode_index = 0);
    h.model.source_loads_chain(LoadedChain::Sdm);
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    h.stop();
    assert_eq!(
        capture.active_mode_index, 0,
        "precondition: the capture is under [source]"
    );

    let report = tier1::diff(&capture, VERIFIED_PROFILE);

    // No mode-relative family may be diffed against a chosen chain under [source].
    let false_divergences: Vec<&tier1::Divergence> = report
        .divergences
        .iter()
        .filter(|d| {
            matches!(
                d.family.as_str(),
                "filters_pcm" | "filters_sdm" | "shapers_pcm" | "shapers_sdm" | "rates"
            )
        })
        .collect();
    assert!(
        false_divergences.is_empty(),
        "under [source] the loaded chain is unknown to the configured mode, so filters/shapers/rates \
         must not be diffed against PCM or SDM; got false divergences:\n{false_divergences:#?}"
    );

    // They must be recorded unverified with an explicit [source] reason, and not counted as checked.
    for family in ["filters", "shapers", "rates"] {
        assert!(
            report
                .unverified
                .iter()
                .any(|u| u.starts_with(family) && u.contains("source")),
            "{family} must be recorded unverified with a [source] reason; unverified={:?}",
            report.unverified
        );
        assert!(
            !report.checked.contains(family),
            "{family} must not be marked checked under [source]; checked={:?}",
            report.checked
        );
    }

    // Mode-independent families still compare: matrix is not chain-scoped.
    assert!(
        report.checked.contains("matrix"),
        "matrix is mode-independent and must still be compared under [source]; checked={:?}",
        report.checked
    );
}

/// Container delivery time is the evidence `HqpTimeouts::response` should be set from, so a capture
/// has to record it per family rather than leaving the value inherited.
#[tokio::test]
async fn tier1_records_container_delivery_time_per_family() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let missing: Vec<&str> = [
        "getinfo",
        "state",
        "status",
        "volume_range",
        "modes",
        "filters",
        "shapers",
        "rates",
    ]
    .into_iter()
    .filter(|f| !capture.latencies.contains_key(*f))
    .collect();
    assert!(
        missing.is_empty(),
        "every captured family needs a delivery time, else response cannot be set from evidence; \
         missing {missing:?}"
    );
    h.stop();
}

/// Skips should be zero against a well-behaved daemon; a non-zero count on real hardware is the
/// signal that the reply-element invariant is narrower than the reference implies. Either way the
/// count has to be observable, which means the client has to actually track it.
#[tokio::test]
async fn tier1_records_how_many_unsolicited_documents_the_client_skipped() {
    let h = Harness::with_policy(WirePolicy {
        coalesce_extra_for_element: Some(("State".to_string(), PUSH_STATUS_FRAME.to_string())),
        ..WirePolicy::default()
    })
    .await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    assert!(
        capture.unsolicited_skipped > 0,
        "this daemon emits an unsolicited frame after every State, so the capture must report a \
         non-zero skip count; got {}",
        capture.unsolicited_skipped
    );
    h.stop();
}

/// Coverage for the identity branch of the differ, which superego correctly noted had none. A daemon
/// reporting a different product/version than the corpus profile claims means every other comparison
/// is against the wrong baseline, and the report should say so first.
#[tokio::test]
async fn tier1_reports_identity_divergence_when_the_daemon_is_a_different_build() {
    let h = Harness::start(LEGACY_PROFILE, WirePolicy::default(), fast_timeouts()).await;
    h.adapter.connect().await.expect("connect");
    let capture = tier1::capture(&h.adapter).await.expect("capture");

    let report = tier1::diff(&capture, VERIFIED_PROFILE);

    let identity: Vec<&tier1::Divergence> = report
        .divergences
        .iter()
        .filter(|d| d.kind == tier1::DivergenceKind::Identity)
        .collect();
    assert!(
        identity.iter().any(|d| d.detail.contains("version"))
            && identity.iter().any(|d| d.detail.contains("engine")),
        "a 5.2.30 Desktop daemon diffed against the 6.0.4 Embedded corpus must report version and \
         engine divergences, naming both sides. Got: {:?}",
        report
            .divergences
            .iter()
            .map(|d| &d.detail)
            .collect::<Vec<_>>()
    );
    h.stop();
}

/// The persistent-lane branch cannot be reached hermetically — the fake daemon speaks the native
/// protocol only, so `has_web_credentials()` is false and the capture records the lane as unreached.
/// Pinning that keeps the honest-partial-coverage promise from silently regressing into a claim.
#[tokio::test]
async fn tier1_records_the_config_read_side_as_unreached_without_credentials() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    assert!(
        capture.config_profiles.is_none()
            && report
                .not_captured
                .iter()
                .any(|n| n.contains("config read side")),
        "without web credentials the /config lane must be reported unreached, not treated as empty. \
         config_profiles={:?} not_captured={:?}",
        capture.config_profiles,
        report.not_captured
    );
    h.stop();
}

/// `State.filter_junk` is an int index into `GetJunkFilters`, and the corpus carries that fixture, so
/// a tier-1 run that never asks for the list leaves the one family whose *type* the client previously
/// got wrong entirely unverified.
#[tokio::test]
async fn tier1_captures_and_diffs_the_junk_filter_list() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let expected = corpus::enum_entries(
        &corpus::document(VERIFIED_PROFILE, "junkfilters"),
        "JunkFiltersItem",
    );
    assert_eq!(
        capture
            .enumerations
            .get("junkfilters")
            .map(|e| e.len())
            .unwrap_or(0),
        expected.len(),
        "the junk-filter list must be captured so it can be diffed; corpus has {} entries",
        expected.len()
    );
    h.stop();
}

/// `MatrixGetProfile` is read-only and reports which profile is current. Capturing the list without
/// the current selection leaves the shape of that reply unverified.
#[tokio::test]
async fn tier1_captures_the_current_matrix_profile_read_only() {
    let h = Harness::verified().await;
    h.model
        .external_change(|s| s.matrix_profile = "Speakers".to_string());
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    assert_eq!(
        capture
            .current_matrix_profile
            .as_ref()
            .map(|(_, name)| name.as_str()),
        Some("Speakers"),
        "the current matrix profile must be captured via MatrixGetProfile; got {:?}",
        capture.current_matrix_profile
    );
    h.stop();
}

/// A human-readable render is for the operator in the room. CI needs something it can store and a
/// later run can diff against, and it must carry a schema version so that comparison stays meaningful.
#[tokio::test]
async fn tier1_emits_a_versioned_machine_readable_artifact() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let json = tier1::diff(&capture, VERIFIED_PROFILE).to_json();
    assert_eq!(
        (
            json.get("schema").and_then(|v| v.as_str()),
            json.get("daemon").is_some(),
            json.get("families").and_then(|v| v.as_array()).is_some(),
            json.get("divergences").and_then(|v| v.as_array()).is_some(),
        ),
        (Some(tier1::ARTIFACT_SCHEMA), true, true, true),
        "the artifact needs a stable schema id and the daemon/families/divergences sections; got {json}"
    );
    h.stop();
}

/// A verification artifact that leaks the credentials used to obtain it is worse than none.
#[tokio::test]
async fn tier1_artifact_never_contains_connection_secrets() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let text = tier1::diff(&capture, VERIFIED_PROFILE)
        .to_json()
        .to_string();
    let leaked: Vec<&str> = [
        "127.0.0.1",
        "password",
        "passwd",
        "secret",
        "digest",
        "127.",
    ]
    .into_iter()
    .filter(|needle| text.to_lowercase().contains(&needle.to_lowercase()))
    .collect();
    assert!(
        leaked.is_empty(),
        "the artifact must carry no host or credential material; found {leaked:?} in {text}"
    );
    h.stop();
}

/// Delivery time on its own does not answer the question the deadline poses. The report has to record
/// the budget that actually applied and say, per family and overall, whether it was met — that is the
/// evidence `HqpTimeouts::response` gets validated from.
#[tokio::test]
async fn tier1_reports_a_within_deadline_verdict_against_the_configured_budget() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    assert_eq!(
        (
            capture.response_deadline,
            report.within_deadline.len() == capture.latencies.len(),
            report.overall_within_deadline,
        ),
        (fast_timeouts().response, true, true),
        "the report must carry the configured deadline, a verdict per captured family, and an \
         overall verdict. deadline={:?} verdicts={:?}",
        capture.response_deadline,
        report.within_deadline
    );
    h.stop();
}

/// A summary is not a capture. If the artifact records only family names and counts, a later run
/// cannot be compared against this one without scraping the human render — which defeats the point of
/// having a machine-readable form at all.
#[tokio::test]
async fn tier1_artifact_carries_the_full_normalized_enumeration_entries() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let json = tier1::diff(&capture, VERIFIED_PROFILE).to_json();

    let filters = json
        .pointer("/capture/enumerations/filters")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let first = filters.first().cloned().unwrap_or(serde_json::Value::Null);
    assert!(
        filters.len() > 1
            && first.get("index").is_some()
            && first.get("name").is_some()
            && first.get("enum_id").is_some(),
        "the artifact must carry every enumeration entry with index/name/enum_id, not just a count; \
         got {} entries, first={first}",
        filters.len()
    );
    h.stop();
}

#[tokio::test]
async fn tier1_artifact_carries_rate_values_and_scalar_attribute_maps() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let json = tier1::diff(&capture, VERIFIED_PROFILE).to_json();

    let rate_hz = json
        .pointer("/capture/enumerations/rates/1/rate")
        .and_then(|v| v.as_u64());
    let state_mode = json.pointer("/capture/scalars/state/mode");
    let vr_min = json.pointer("/capture/scalars/volume_range/min");
    assert!(
        rate_hz.is_some() && state_mode.is_some() && vr_min.is_some(),
        "rates need their Hz value and the scalar families need their attribute maps: \
         rate_hz={rate_hz:?} state.mode={state_mode:?} volume_range.min={vr_min:?}"
    );
    h.stop();
}

#[tokio::test]
async fn tier1_artifact_records_the_config_profile_observation_explicitly() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let json = tier1::diff(&capture, VERIFIED_PROFILE).to_json();
    assert!(
        json.pointer("/capture/config_profiles").is_some(),
        "the config read side must appear in the artifact as an explicit observation — null when \
         unreached — rather than being absent and therefore ambiguous; got {json}"
    );
    h.stop();
}

/// The live runner has to emit the artifact, not just the render, and it needs stable markers so a
/// caller can lift the JSON out of captured output deterministically.
#[tokio::test]
async fn tier1_emits_the_artifact_between_stable_markers() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    let block = report.artifact_block();

    let inner = block
        .split_once(tier1::ARTIFACT_BEGIN)
        .and_then(|(_, rest)| rest.split_once(tier1::ARTIFACT_END))
        .map(|(json, _)| json.trim().to_string())
        .unwrap_or_default();
    let parsed: Option<serde_json::Value> = serde_json::from_str(&inner).ok();
    assert!(
        parsed.as_ref().and_then(|v| v.get("schema")).is_some(),
        "the emitted block must contain the artifact between {:?} and {:?} and parse as JSON; \
         block={block}",
        tier1::ARTIFACT_BEGIN,
        tier1::ARTIFACT_END
    );
    h.stop();
}

/// `MatrixListProfiles` was captured but never diffed, because the matrix family was missing from the
/// list of families the differ walks. A corpus that names a profile the daemon does not have has to be
/// reported like any other missing entry.
#[tokio::test]
async fn tier1_diffs_the_matrix_profile_list_against_the_corpus() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    // Drop a profile the corpus claims, leaving matrix as the only thing that can diverge.
    // "Speakers" is in matrix_profiles.xml; "Headphones" is in the /config form fixture, which is a
    // different family entirely — an easy confusion, and picking the wrong one is how this test first
    // passed vacuously.
    let mut trimmed = capture.clone();
    trimmed
        .enumerations
        .get_mut("matrix")
        .expect("matrix captured")
        .retain(|e| e.name != "Speakers");

    let report = tier1::diff(&trimmed, VERIFIED_PROFILE);
    assert!(
        report.divergences.iter().any(|d| d.family.contains("matrix")
            && d.kind == tier1::DivergenceKind::MissingEntry
            && d.detail.contains("Speakers")),
        "a matrix profile the corpus claims and the daemon lacks must be a MissingEntry divergence \
         on the matrix family. Got: {:?}",
        report.divergences
    );
    h.stop();
}

/// A current selection that is not in the daemon's own list is incoherent, and so is one that
/// disagrees with `State.matrix_profile`. Capturing both and comparing neither would miss it.
#[tokio::test]
async fn tier1_reports_a_current_matrix_profile_missing_from_the_daemons_own_list() {
    let h = Harness::verified().await;
    h.model
        .external_change(|s| s.matrix_profile = "Ghost".to_string());
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    assert!(
        report
            .divergences
            .iter()
            .any(|d| d.detail.contains("Ghost") && d.family.contains("matrix")),
        "a current profile absent from MatrixListProfiles must be a divergence. Got: {:?}",
        report.divergences
    );
    h.stop();
}

#[tokio::test]
async fn tier1_reports_a_current_matrix_profile_that_disagrees_with_state() {
    let h = Harness::verified().await;
    // State says Default; MatrixGetProfile says Speakers. Both are in the list, so the only fault is
    // that the daemon's two views of one setting disagree.
    h.model
        .arm(|f| f.matrix_current_override = Some("Speakers".to_string()));
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    assert!(
        report
            .divergences
            .iter()
            .any(|d| d.detail.contains("State") && d.detail.contains("Speakers")),
        "MatrixGetProfile disagreeing with State.matrix_profile must be reported, naming both. \
         Got: {:?}",
        report.divergences
    );
    h.stop();
}

/// A read failure and a daemon that legitimately reports no selection are different facts, and `None`
/// alone cannot tell them apart. Warning to the log and moving on loses the distinction.
#[tokio::test]
async fn tier1_distinguishes_a_matrix_read_failure_from_no_current_selection() {
    let h = Harness::with_policy(WirePolicy {
        malformed_for_element: Some((
            "MatrixGetProfile".to_string(),
            "</MatrixGetProfile>".to_string(),
        )),
        ..WirePolicy::default()
    })
    .await;
    let capture = tier1::capture(&h.adapter)
        .await
        .expect("capture survives the failure");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    assert!(
        capture.matrix_current_read_failed
            && report
                .not_captured
                .iter()
                .any(|n| n.contains("MatrixGetProfile")),
        "a MatrixGetProfile read failure must be recorded as unreached, not collapsed into None. \
         read_failed={} not_captured={:?}",
        capture.matrix_current_read_failed,
        report.not_captured
    );
    h.stop();
}

// =============================================================================
// Tier-1 acceptance: coverage completeness, not just detection capability
// =============================================================================

/// The meta-fix. Every previous tier-1 test picked one family, mutated it, and proved a divergence
/// appeared — which validates the mechanism for families that were wired up and is structurally blind
/// to one that was forgotten. Three scalar families were captured and never compared for exactly that
/// reason. ADR 003's required list is held as data so the differ is asserted against the spec rather
/// than against whatever it happens to walk.
#[tokio::test]
async fn tier1_checks_every_family_adr_003_requires() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    let unchecked: Vec<&str> = tier1::REQUIRED_CLAIMS
        .into_iter()
        .filter(|claim| !report.checked.contains(*claim))
        .collect();
    assert!(
        unchecked.is_empty(),
        "ADR 003 requires these claims to be compared and this run did not compare them: \
         {unchecked:?}. checked={:?}",
        report.checked
    );
    h.stop();
}

/// "Clean" must not be able to mean "we never looked". A required claim that could not be observed is
/// an unverified claim, and it has to withhold a merge-gate pass unless it is one of the two
/// deliberately accepted limits.
#[tokio::test]
async fn tier1_withholds_merge_gate_pass_when_a_required_claim_is_unobserved() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let mut blinded = capture.clone();
    blinded.status_metadata_child = tier1::MetadataChild::Unobserved;
    blinded.enumerations.remove("junkfilters");

    let report = tier1::diff(&blinded, VERIFIED_PROFILE);
    assert!(
        !report.merge_gate_pass() && !report.unverified.is_empty(),
        "with the metadata child unobserved and the junk-filter family missing, no merge-gate pass \
         may be reported. pass={} unverified={:?} divergences={}",
        report.merge_gate_pass(),
        report.unverified,
        report.divergences.len()
    );
    h.stop();
}

// --- scalar families: captured but never diffed until now ---

#[tokio::test]
async fn tier1_reports_divergence_when_state_shape_differs() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let mut mutated = capture.clone();
    mutated
        .scalars
        .get_mut("state")
        .expect("state captured")
        .remove("filter_junk");
    let report = tier1::diff(&mutated, VERIFIED_PROFILE);
    assert!(
        report
            .divergences
            .iter()
            .any(|d| d.family == "state" && d.detail.contains("filter_junk")),
        "a State document missing filter_junk must diverge — the client read that attribute as the \
         wrong type once already. Got: {:?}",
        report.divergences
    );
    h.stop();
}

#[tokio::test]
async fn tier1_reports_divergence_when_status_shape_differs() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let mut mutated = capture.clone();
    mutated
        .scalars
        .get_mut("status")
        .expect("status captured")
        .remove("active_filter");
    let report = tier1::diff(&mutated, VERIFIED_PROFILE);
    assert!(
        report
            .divergences
            .iter()
            .any(|d| d.family == "status" && d.detail.contains("active_filter")),
        "a Status document missing active_filter must diverge. Got: {:?}",
        report.divergences
    );
    h.stop();
}

#[tokio::test]
async fn tier1_reports_divergence_when_volume_range_shape_differs() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let mut mutated = capture.clone();
    mutated
        .scalars
        .get_mut("volume_range")
        .expect("volume_range captured")
        .remove("enabled");
    let report = tier1::diff(&mutated, VERIFIED_PROFILE);
    assert!(
        report
            .divergences
            .iter()
            .any(|d| d.family == "volume_range" && d.detail.contains("enabled")),
        "VolumeRange without `enabled` cannot tell a fixed-volume daemon from a variable one, so it \
         must diverge. Got: {:?}",
        report.divergences
    );
    h.stop();
}

// --- complete normalized scalar capture ---

#[tokio::test]
async fn tier1_captures_every_parsed_state_field() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let recorded = capture.scalars.get("state").cloned().unwrap_or_default();
    let missing: Vec<&str> = [
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
    ]
    .into_iter()
    .filter(|k| !recorded.contains_key(*k))
    .collect();
    assert!(
        missing.is_empty(),
        "every field the client parses out of State must reach the artifact, or a live run cannot be \
         compared on it; missing {missing:?}"
    );
    h.stop();
}

#[tokio::test]
async fn tier1_captures_every_parsed_status_field() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let recorded = capture.scalars.get("status").cloned().unwrap_or_default();
    let missing: Vec<&str> = [
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
    ]
    .into_iter()
    .filter(|k| !recorded.contains_key(*k))
    .collect();
    assert!(
        missing.is_empty(),
        "every field the client parses out of Status must reach the artifact; missing {missing:?}"
    );
    h.stop();
}

/// The bug class this whole boundary exists to remove, committed inside the boundary itself: presence
/// was inferred from `samplerate > 0`, so a present child carrying zeros read as no child at all.
#[tokio::test]
async fn tier1_distinguishes_a_present_zero_valued_metadata_child_from_absence() {
    let with_zero_child = Harness::verified().await;
    with_zero_child.model.external_change(|s| {
        s.metadata = Some(mock_servers::hqplayer::model::Metadata {
            artist: "Zero".to_string(),
            album: String::new(),
            song: String::new(),
            samplerate: 0,
            bits: 0,
            channels: 0,
            bitrate: 0,
        })
    });
    let present = tier1::capture(&with_zero_child.adapter)
        .await
        .expect("capture with a zero-valued child");
    with_zero_child.stop();

    let without = Harness::verified().await;
    without.model.external_change(|s| s.metadata = None);
    let absent = tier1::capture(&without.adapter)
        .await
        .expect("capture with no child");
    without.stop();

    assert_eq!(
        (present.status_metadata_child, absent.status_metadata_child),
        (
            tier1::MetadataChild::Present,
            tier1::MetadataChild::Absent
        ),
        "a metadata child carrying zeros is still present; inferring presence from a value that has \
         a legitimate zero is exactly the defect this harness exists to catch"
    );
}

// --- filter/shaper wire-shape claims ADR 003 requires ---

#[tokio::test]
async fn tier1_captures_and_diffs_filter_arg_flags() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    // poly-sinc-ext2 carries arg=1 (apodizing) in the corpus.
    let shape = capture.entry_shapes.get("filters/poly-sinc-ext2").cloned();
    assert_eq!(
        shape.and_then(|s| s.arg),
        Some(1),
        "FiltersItem.arg is a flags bitfield ADR 003 requires diffing; it was dropped at the \
         adapter-to-corpus boundary. entry_shapes keys={:?}",
        capture.entry_shapes.keys().take(4).collect::<Vec<_>>()
    );
    h.stop();
}

#[tokio::test]
async fn tier1_captures_and_diffs_filter_description_presence() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let shape = capture.entry_shapes.get("filters/poly-sinc-ext2").cloned();
    assert_eq!(
        shape.and_then(|s| s.has_description),
        Some(true),
        "`description` is not on FilterItem at all, so its presence can only be observed from the raw \
         document — and ADR 003 requires it. Calling it verified without looking is the failure mode."
    );
    h.stop();
}

// --- identity, rates, matrix, config read side ---

#[tokio::test]
async fn tier1_diffs_the_full_getinfo_attribute_contract() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let mut mutated = capture.clone();
    // `name` and `platform` are instance-specific in value but contractually present.
    mutated.identity.remove("platform");
    let report = tier1::diff(&mutated, VERIFIED_PROFILE);
    assert!(
        report
            .divergences
            .iter()
            .any(|d| d.family == "getinfo" && d.detail.contains("platform")),
        "GetInfo's attribute set is part of the contract even where values are instance-specific; a \
         missing platform must diverge. Got: {:?}",
        report.divergences
    );
    h.stop();
}

#[tokio::test]
async fn tier1_reports_a_daemon_only_rate_mapping() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let mut mutated = capture.clone();
    mutated
        .enumerations
        .get_mut("rates")
        .expect("rates captured")
        .push(mock_servers::hqplayer::corpus::EnumEntry {
            index: 99,
            name: String::new(),
            enum_id: None,
            rate: Some(1_536_000),
            arg: None,
            description: None,
        });
    let report = tier1::diff(&mutated, VERIFIED_PROFILE);
    assert!(
        report
            .divergences
            .iter()
            .any(|d| d.family == "rates" && d.detail.contains("99")),
        "rates had no reverse pass, so a daemon-only index was silently accepted. Got: {:?}",
        report.divergences
    );
    h.stop();
}

#[tokio::test]
async fn tier1_requires_rate_index_zero_to_be_auto() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let mut mutated = capture.clone();
    if let Some(first) = mutated
        .enumerations
        .get_mut("rates")
        .and_then(|r| r.iter_mut().find(|e| e.index == 0))
    {
        first.rate = Some(44_100);
    }
    let report = tier1::diff(&mutated, VERIFIED_PROFILE);
    assert!(
        report
            .divergences
            .iter()
            .any(|d| d.family == "rates" && d.detail.contains("auto")),
        "index 0 is rate 0 meaning auto/source-based; a daemon reporting otherwise must diverge. \
         Got: {:?}",
        report.divergences
    );
    h.stop();
}

#[tokio::test]
async fn tier1_requires_the_current_matrix_index_and_name_to_match_the_list() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let mut mutated = capture.clone();
    // Right name, wrong index: membership-by-name alone accepts this.
    mutated.current_matrix_profile = Some((7, "Default".to_string()));
    let report = tier1::diff(&mutated, VERIFIED_PROFILE);
    assert!(
        report
            .divergences
            .iter()
            .any(|d| d.family.contains("matrix") && d.detail.contains('7')),
        "MatrixGetProfile reports an index as well as a name, and the pair must match the list. \
         Got: {:?}",
        report.divergences
    );
    h.stop();
}

#[tokio::test]
async fn tier1_diffs_the_config_form_field_names_and_default_structure() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let mut mutated = capture.clone();
    mutated.config_form = Some(tier1::ConfigFormObs {
        field_names: ["profile".to_string()].into_iter().collect(),
        offers_default: false,
        named_profiles: vec![("Speakers".to_string(), "Speakers".to_string())],
    });
    let report = tier1::diff(&mutated, VERIFIED_PROFILE);
    let details: Vec<&String> = report.divergences.iter().map(|d| &d.detail).collect();
    assert!(
        details.iter().any(|d| d.contains("profile_name"))
            && details.iter().any(|d| d.contains("[default]")),
        "the read-side contract is the field names and the [default]-versus-named structure, not just \
         the profile values. Got: {details:?}"
    );
    h.stop();
}

#[tokio::test]
async fn tier1_serializes_config_profile_value_and_title() {
    let h = Harness::verified().await;
    let mut capture = tier1::capture(&h.adapter).await.expect("capture");
    capture.config_profiles = Some(vec![("Speakers".to_string(), "Living room".to_string())]);
    let json = tier1::diff(&capture, VERIFIED_PROFILE).to_json();
    let text = json.to_string();
    assert!(
        text.contains("Living room"),
        "HqpProfile.title was discarded; the artifact must carry value and title. Got {text}"
    );
    h.stop();
}

#[tokio::test]
async fn tier1_artifact_excludes_auth_and_hidden_token_material() {
    let h = Harness::verified().await;
    let mut capture = tier1::capture(&h.adapter).await.expect("capture");
    capture.raw_documents.insert(
        "config_form".to_string(),
        "<form><input type=\"hidden\" name=\"csrf\" value=\"SEKRET\"/></form>".to_string(),
    );
    let text = tier1::diff(&capture, VERIFIED_PROFILE)
        .to_json()
        .to_string();
    assert!(
        !text.contains("SEKRET") && !text.to_lowercase().contains("csrf"),
        "hidden-token and auth material must be stripped before anything reaches the artifact. \
         Got {text}"
    );
    h.stop();
}

#[tokio::test]
async fn tier1_artifact_carries_the_raw_shape_observations_each_diff_used() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let json = tier1::diff(&capture, VERIFIED_PROFILE).to_json();
    let raw = json.pointer("/capture/raw_documents").cloned();
    let shapes = json.pointer("/capture/entry_shapes").cloned();
    assert!(
        raw.as_ref().and_then(|v| v.as_object()).map(|o| !o.is_empty()) == Some(true)
            && shapes.as_ref().and_then(|v| v.as_object()).map(|o| !o.is_empty()) == Some(true),
        "the artifact must carry the evidence the shape diffs actually used, not a reconstruction. \
         raw={raw:?} shapes={shapes:?}"
    );
    h.stop();
}

/// `fetch_config_page_raw` exists for tier-1 shape observation. Like the timeout seam, it has to be
/// `pub` for an integration-test crate to reach it, so the constraint is a lint rather than a type.
#[test]
fn no_production_code_reads_the_raw_config_page() {
    let callers: Vec<String> = walkdir::WalkDir::new("src")
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("rs"))
        .filter_map(|e| {
            let body = std::fs::read_to_string(e.path()).ok()?;
            body.matches(".fetch_config_page_raw(")
                .count()
                .gt(&0)
                .then(|| e.path().display().to_string())
        })
        .collect();
    assert!(
        callers.is_empty(),
        "fetch_config_page_raw is the tier-1 shape-observation seam; production should reach the \
         persistent lane through fetch_profiles and friends. Called from {callers:?}"
    );
}

// =============================================================================
// Tier-1 acceptance pass 4: the raw lane's own safety and fidelity
// =============================================================================

/// The lane guarded its element name against a mutating command and then interpolated a free-form
/// attribute string straight into the request, so read-only rested on caller discipline rather than on
/// the type. A closed request type removes the injection surface instead of policing it.
#[test]
fn the_raw_lane_cannot_express_a_mutating_request() {
    use mock_servers::hqplayer::raw::Query;
    let rendered: Vec<String> = Query::ALL.into_iter().map(|q| q.request()).collect();

    let mutating = [
        "Set", "Volume", "Play", "Pause", "Stop", "Next", "Previous", "Seek", "Reset",
    ];
    let offenders: Vec<&String> = rendered
        .iter()
        .filter(|r| {
            // Exactly one element, and it is not a command. Two '<' means something was appended.
            r.matches('<').count() != 2
                || mutating.iter().any(|m| {
                    r.split_once(char::is_whitespace)
                        .map(|(head, _)| head.contains(m))
                        .unwrap_or_else(|| r.contains(m))
                })
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "every raw query must render to exactly one query element with no room for an appended \
         command; offenders={offenders:?} all={rendered:?}"
    );
}

#[test]
fn the_raw_lane_query_set_covers_every_read_only_family() {
    use mock_servers::hqplayer::raw::Query;
    let elements: Vec<&str> = Query::ALL.into_iter().map(|q| q.element()).collect();
    let missing: Vec<&str> = [
        "GetInfo",
        "State",
        "Status",
        "VolumeRange",
        "GetModes",
        "GetFilters",
        "GetShapers",
        "GetRates",
        "GetJunkFilters",
        "MatrixListProfiles",
        "MatrixGetProfile",
    ]
    .into_iter()
    .filter(|e| !elements.contains(e))
    .collect();
    assert!(
        missing.is_empty(),
        "the raw lane must be able to observe every read-only family the ADR names; missing {missing:?}"
    );
}

/// Naming a reply root by assuming it equals the request element turns any family where they differ
/// into a silent timeout. The mapping has to be explicit and asserted.
#[test]
fn the_raw_lane_states_the_reply_root_for_every_query() {
    use mock_servers::hqplayer::raw::Query;
    let pairs: Vec<(&str, &str)> = Query::ALL
        .into_iter()
        .map(|q| (q.element(), q.reply_element()))
        .collect();
    let unstated: Vec<&(&str, &str)> = pairs
        .iter()
        .filter(|(el, reply)| *el == "STUB" || *reply == "STUB" || reply.is_empty())
        .collect();
    assert!(
        unstated.is_empty(),
        "each query must state the element the daemon actually answers with, so a naming difference \
         is a comparison rather than a hang; unstated={unstated:?}"
    );
}

/// Legal XML the daemon may send and this parser previously dropped on the floor.
#[test]
fn the_raw_attribute_parser_handles_legal_whitespace_and_both_quote_styles() {
    use mock_servers::hqplayer::raw::root_attrs;
    let cases = [
        ("<X index=\"0\" name=\"none\"/>", "plain double quotes"),
        ("<X index = \"0\" name = \"none\"/>", "whitespace around ="),
        ("<X index='0' name='none'/>", "single quotes"),
        ("<X index='0' name=\"none\"/>", "mixed quote styles"),
        ("<X\n  index=\"0\"\n  name=\"none\"/>", "newline separated"),
    ];
    let failures: Vec<&str> = cases
        .iter()
        .filter(|(doc, _)| {
            {
                let attrs = root_attrs(doc);
                attrs.iter().any(|(k, v)| k == "index" && v == "0")
                    && attrs.iter().any(|(k, v)| k == "name" && v == "none")
            }
            .eq(&false)
        })
        .map(|(_, label)| *label)
        .collect();
    assert!(
        failures.is_empty(),
        "under-observing a legal document is worse than failing loudly, because the diff then reports \
         a false absence. Failed on: {failures:?}"
    );
}

/// Tag-level redaction left the secret between the tags. `<password>SEKRET</password>` became
/// `<!-- redacted -->SEKRET<!-- redacted -->`.
#[test]
fn the_config_evidence_projection_cannot_leak_sensitive_element_text() {
    use mock_servers::hqplayer::tier1;
    let hostile = concat!(
        "<html><form method=\"post\" action=\"/config\">",
        "<select name=\"profile\"><option value=\"[default]\">[default]</option>",
        "<option value=\"Speakers\">Living room</option></select>",
        "<input type=\"text\" name=\"profile_name\" value=\"\"/>",
        "<input type='hidden' name='csrf' value='S3CR3T'/>",
        "<password>SEKRET</password>",
        "<token>tok-abcdef</token>",
        "<div>Authorization: Digest cnonce=DEADBEEF</div>",
        "</form></html>"
    );
    let obs = tier1::project_config_form(hostile);
    let serialized = serde_json::to_string(&serde_json::json!({
        "field_names": obs.field_names,
        "offers_default": obs.offers_default,
        "named_profiles": obs.named_profiles,
    }))
    .expect("serialise");

    let leaked: Vec<&str> = [
        "SEKRET",
        "S3CR3T",
        "tok-abcdef",
        "DEADBEEF",
        "csrf",
        "password",
        "token",
        "Authorization",
    ]
    .into_iter()
    .filter(|needle| serialized.to_lowercase().contains(&needle.to_lowercase()))
    .collect();
    assert!(
        leaked.is_empty(),
        "an allowlisted projection must carry only the field names, the [default] flag and the named \
         profiles - never element text, hidden inputs or auth material. Leaked {leaked:?} in \
         {serialized}"
    );
}

#[test]
fn the_config_evidence_projection_still_observes_the_read_side_contract() {
    use mock_servers::hqplayer::tier1;
    let page = corpus::document(VERIFIED_PROFILE, "config_profile_form");
    let obs = tier1::project_config_form(&page);
    assert!(
        obs.field_names.contains("profile")
            && obs.field_names.contains("profile_name")
            && obs.offers_default
            && obs
                .named_profiles
                .iter()
                .any(|(v, t)| v == "Speakers" && !t.is_empty()),
        "the projection must still see the field names, the [default] option and each named profile's \
         value AND title, read from the form rather than reconstructed. Got {obs:?}"
    );
}

/// The claim set has to be a faithful rendering of ADR 003's rows, or a family can be dropped from the
/// spec's coverage without any test noticing.
#[test]
fn required_claims_faithfully_renders_every_adr_003_row() {
    use mock_servers::hqplayer::tier1;
    let missing: Vec<&str> = tier1::ADR_CLAIM_ROWS
        .into_iter()
        .filter(|row| !tier1::REQUIRED_CLAIMS.contains(row))
        .collect();
    assert!(
        missing.is_empty(),
        "ADR 003 names these claims and REQUIRED_CLAIMS omits them, so a run can pass without \
         comparing them: {missing:?}"
    );
}

/// Without credentials the config lane is a declared accepted limit; with them it is a required claim.
/// Collapsing those two makes an absent credential look like a satisfied contract.
#[tokio::test]
async fn tier1_treats_the_config_lane_as_an_accepted_limit_only_without_credentials() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");

    // Both halves state the credential fact outright instead of inheriting it. `isolate_config_dir`
    // shares one directory across this binary and `configure` persists what it is given, so an adapter
    // built with no credentials can still observe a previous test's — which made this comparison depend
    // on test order once the differ started reading the flag. Set explicitly, the two cases are the two
    // classifications and nothing else.
    let mut without = capture.clone();
    without.has_web_credentials = false;
    let no_creds = tier1::diff(&without, VERIFIED_PROFILE);

    let mut with_form = capture.clone();
    // Stated as the fact itself rather than implied by a non-empty profile list. Inferring credentials
    // from `config_profiles.is_some()` is what let a credentialed run that observed nothing be read as
    // the accepted no-credentials limit: the proxy is absent in exactly the failure case it needed to
    // detect.
    with_form.has_web_credentials = true;
    with_form.config_form = None;
    with_form.config_profiles = Some(vec![("Speakers".to_string(), "Living room".to_string())]);
    let creds_but_no_form = tier1::diff(&with_form, VERIFIED_PROFILE);

    assert!(
        no_creds
            .not_captured
            .iter()
            .any(|n| n.contains("config read side"))
            && !no_creds
                .unverified
                .iter()
                .any(|u| u.contains("config_form"))
            && creds_but_no_form
                .unverified
                .iter()
                .any(|u| u.contains("config_form")),
        "no credentials is an accepted limit; credentials present with no form observation is an \
         unverified required claim. without={:?} with={:?}",
        no_creds.unverified,
        creds_but_no_form.unverified
    );
    h.stop();
}

/// Evidence must come off the wire through the raw lane, not be rebuilt from parsed values afterwards.
/// The fake speaks the native protocol, so every native family can and must be observed that way.
#[tokio::test]
async fn tier1_observes_every_native_family_through_the_raw_lane() {
    let h = Harness::verified().await;
    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let missing: Vec<&str> = [
        "getinfo",
        "state",
        "status",
        "volume_range",
        "modes",
        "filters",
        "shapers",
        "rates",
        "junkfilters",
        "matrix",
        "matrix_current",
    ]
    .into_iter()
    .filter(|f| !capture.raw_documents.contains_key(*f))
    .collect();
    assert!(
        missing.is_empty(),
        "every native family's evidence must be a document the raw lane read off the socket, not a \
         reconstruction after semantic parsing; missing raw evidence for {missing:?}"
    );
    h.stop();
}

/// The sanitiser is applied to raw protocol XML, which is allowed to be retained, so its own holes
/// matter. Tag-level redaction left `<password>SEKRET</password>` as
/// `<!-- redacted -->SEKRET<!-- redacted -->` — the secret stranded between the removed tags.
#[test]
fn the_sanitiser_drops_sensitive_element_text_not_just_the_tags() {
    use mock_servers::hqplayer::raw::sanitize;
    let cases = [
        "<config><password>SEKRET</password></config>",
        "<config><token>tok-abcdef</token></config>",
        "<config><x auth_token=\"SEKRET\"/></config>",
        "<config><input type='hidden' name='csrf' value='SEKRET'/></config>",
        "<config><div>Authorization: Digest cnonce=SEKRET</div></config>",
        "<config><wrapper><secret>SEKRET</secret></wrapper></config>",
    ];
    let leaks: Vec<&str> = cases
        .into_iter()
        .filter(|doc| {
            let clean = sanitize(doc);
            clean.contains("SEKRET") || clean.contains("tok-abcdef")
        })
        .collect();
    assert!(
        leaks.is_empty(),
        "a sensitive element's text must go with the element, not be stranded in the output. \
         Leaked from: {leaks:?}"
    );
}

#[test]
fn the_sanitiser_keeps_ordinary_protocol_content_intact() {
    use mock_servers::hqplayer::raw::sanitize;
    let doc = "<?xml version=\"1.0\"?><GetInfo engine=\"6.0.4\" name=\"Opal\" version=\"6\"/>";
    let clean = sanitize(doc);
    assert!(
        clean.contains("engine=\"6.0.4\"")
            && clean.contains("name=\"Opal\"")
            && clean.contains("GetInfo"),
        "sanitising must not destroy the evidence it exists to make safe to keep; got {clean}"
    );
}

/// The redaction list must have exactly one definition. Three copies existed briefly and had already
/// diverged (`"apikey"` in the sanitiser, `"key"` in the config projection) — which is the very
/// failure mode the list exists to prevent: extend one copy, miss the others, reopen the hole. This
/// is a lint, not a behaviour test, so it fails on the duplication rather than on a later leak.
#[test]
fn the_redaction_marker_list_has_exactly_one_definition() {
    let mut definitions = Vec::new();
    for entry in walkdir::WalkDir::new("tests")
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
    {
        let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
        for (n, line) in body.lines().enumerate() {
            // A literal array of redaction markers, wherever it is declared.
            // Needles are split so this lint's own source line is not a match for itself.
            let decl = concat!("SENSITIVE", "");
            // Only a marker ARRAY trips this. A future `const SENSITIVE_TIMEOUT: Duration` is not a
            // second redaction list, and failing on it would point the next reader at the wrong idea.
            let is_marker_array = line.contains("const ") && line.contains(": [&str;");
            if line.contains(decl) && is_marker_array {
                definitions.push(format!("{}:{}", entry.path().display(), n + 1));
            }
        }
    }
    assert_eq!(
        definitions.len(),
        1,
        "expected one definition of the redaction markers, found {}: {definitions:?}. \
         Import `raw::is_sensitive` instead of declaring a second list.",
        definitions.len()
    );
}

// =============================================================================
// Tier-1 config read side: the evidence must come from the raw lane
// =============================================================================

/// A minimal HTTP/1.1 fake for the `/config` read side. The web lane had no hermetic coverage at all,
/// which is how a defect in it survived: `capture()` sourced `config_profiles` from
/// `fetch_profiles()` — the *semantic* parser — silently discarding the raw projection taken from
/// `/config`. Serving deliberately different content on the two paths makes the source observable.
struct FakeConfigWeb {
    port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeConfigWeb {
    /// `config_page` is served at `/config`; `profile_page` at `/config/profile/load`.
    async fn start(config_page: &'static str, profile_page: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake config web");
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    // Accumulate until the header terminator. TCP does not promise the request line
                    // arrives in one read, and `/config` is a prefix of `/config/profile/load`, so
                    // routing on a partial head can serve the wrong page for a request that was fine —
                    // a latent nondeterminism in every test that reads this lane. Capped so a client
                    // that never terminates its head cannot grow this buffer without bound.
                    let mut head = Vec::new();
                    loop {
                        let mut chunk = [0u8; 1024];
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 || head.len() + n > 16 * 1024 {
                            return;
                        }
                        head.extend_from_slice(&chunk[..n]);
                        if head.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    // Routed on the parsed request target rather than a substring of the whole head, so
                    // a path appearing in a header value cannot decide the route either.
                    let request = String::from_utf8_lossy(&head);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1));
                    let body = match path {
                        Some("/config/profile/load") => profile_page,
                        Some("/config") => config_page,
                        _ => return,
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        Self { port, handle }
    }

    fn stop(self) {
        self.handle.abort();
    }
}

/// The two pages name different profiles. Whichever set lands in the capture names its source.
const CONFIG_PAGE_RAW_LANE: &str = r#"<html><body><form>
    <input type="text" name="matrix_profile"/>
    <select name="profile">
      <option value="raw-a">Raw Lane A</option>
      <option value="raw-b">Raw Lane B</option>
    </select>
</form></body></html>"#;

const PROFILE_PAGE_SEMANTIC_LANE: &str = r#"<html><body><form>
    <select name="profile">
      <option value="semantic-z">Semantic Lane Z</option>
    </select>
</form></body></html>"#;

/// Codex finding 7: evidence must be read off the raw lane, not reconstructed after semantic parsing.
/// `config_profiles` was assigned from the `/config` projection and then unconditionally overwritten
/// in *both* arms of the `fetch_profiles()` match, so the projection could never survive.
#[tokio::test]
async fn tier1_takes_the_config_profile_evidence_from_the_raw_page_not_the_semantic_parser() {
    let web = FakeConfigWeb::start(CONFIG_PAGE_RAW_LANE, PROFILE_PAGE_SEMANTIC_LANE).await;
    let h = Harness::start(VERIFIED_PROFILE, WirePolicy::default(), fast_timeouts()).await;
    h.adapter.connect().await.expect("connect to fake daemon");
    configure_without_persisting(
        &h.adapter,
        "127.0.0.1",
        h.server.port(),
        web.port,
        "conformance",
        "conformance",
    )
    .await;

    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let profiles = capture
        .config_profiles
        .clone()
        .expect("the read side was reachable, so profiles must be captured");
    let values: Vec<&str> = profiles.iter().map(|(v, _)| v.as_str()).collect();

    assert!(
        values.contains(&"raw-a") && values.contains(&"raw-b"),
        "config_profiles must be the /config raw-lane projection, but got {values:?}. \
         If this reads `semantic-z`, the evidence was reconstructed from fetch_profiles() \
         after semantic parsing — the exact thing tier 1 must not do."
    );
    assert!(
        !values.contains(&"semantic-z"),
        "the semantic parser's output must not stand in as raw-lane evidence; got {values:?}"
    );

    h.stop();
    web.stop();
}

/// The semantic parser is still worth running — as a cross-check. When the two lanes disagree that is
/// a finding about the client, so it must surface as a divergence rather than be silently resolved.
#[tokio::test]
async fn tier1_reports_a_divergence_when_the_semantic_parser_disagrees_with_the_raw_page() {
    let web = FakeConfigWeb::start(CONFIG_PAGE_RAW_LANE, PROFILE_PAGE_SEMANTIC_LANE).await;
    let h = Harness::start(VERIFIED_PROFILE, WirePolicy::default(), fast_timeouts()).await;
    h.adapter.connect().await.expect("connect to fake daemon");
    configure_without_persisting(
        &h.adapter,
        "127.0.0.1",
        h.server.port(),
        web.port,
        "conformance",
        "conformance",
    )
    .await;

    let capture = tier1::capture(&h.adapter).await.expect("capture");
    let report = tier1::diff(&capture, VERIFIED_PROFILE);
    // Assert on the typed divergence, not on a substring of the rendering: "config_profiles" appears
    // in the report for several unrelated reasons, so a substring check here passes vacuously.
    let lane = report.divergences.iter().find(|d| {
        d.family == "config_profiles" && d.kind == tier1::DivergenceKind::LaneDisagreement
    });
    assert!(
        lane.is_some(),
        "the raw lane named raw-a/raw-b and the parser named semantic-z; that disagreement must be \
         reported as a LaneDisagreement, not silently resolved. Divergences were: {:?}",
        report.divergences
    );

    h.stop();
    web.stop();
}

/// Sanitising must leave the document reparseable. The rewrite unescapes text with quick-xml and
/// pushes the result straight back out, so a document whose text contained `&amp;` would emit a bare
/// `&` — no longer well-formed, and the artifact embeds these documents as evidence.
#[test]
fn the_sanitiser_emits_a_document_that_still_reparses() {
    use mock_servers::hqplayer::raw::sanitize;
    let doc = "<?xml version=\"1.0\"?><Status active_filter=\"poly-sinc\">Rock &amp; Roll &lt;live&gt;</Status>";
    let clean = sanitize(doc);

    let mut reader = quick_xml::Reader::from_str(&clean);
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Text(t)) => {
                text.push_str(&t.unescape().expect("text must unescape after sanitising"));
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(e) => panic!("sanitised output no longer reparses: {e}\noutput was: {clean}"),
        }
    }
    assert_eq!(
        text, "Rock & Roll <live>",
        "text must survive the round trip with its meaning intact; got {text:?} from {clean:?}"
    );
}

// =============================================================================
// HQPTuner Stage 1 amendment — Stage 2 bites
//
// Every expectation below carries an explicit label:
//
//   * `client-conformance` — can fail against the UNMODIFIED client. RED-first, always.
//   * `model-fidelity`     — proves a fake capability or invariant. Written alongside the
//                            capability and NEVER claimed as a client red, because a production
//                            stub added just to make one fail would be manufactured evidence
//                            (see PR #337's disclosures of `bd87e21` and `d7f62c2`).
// Those two are the whole contract. A pre-existing property that is merely being pinned is still
// classified under one of them and is described in prose as a regression pin; there is no third label,
// and no label is ever attached to known-broken behaviour.
//
// Plan and classification: `.oh/issue-322-hqplayer-protocol-conformance.md`, section
// "HQPTuner Stage 1 amendment".
// =============================================================================

// -----------------------------------------------------------------------------
// Bite 1 — recover a closed root whose children cannot be parsed
// -----------------------------------------------------------------------------

/// Upstream evidence: the daemon emits malformed XML inside `<metadata>`, and without a recovery
/// path the receive loop runs to timeout **on every poll while a track is loaded**.
///
/// Inspection narrowed which shapes actually reach the client. A hostile *attribute value* does
/// not: UHC never parses the children and scopes attribute reads to the root's opening tag, so
/// unescaped `<`, `"`, `>` and bare `&` inside `<metadata …/>` are already tolerated — pinned by
/// [`the_framer_already_tolerates_hostile_attribute_values_in_children`] below.
///
/// What does reach the client is a **structurally** malformed child: a child tag that never
/// terminates. `</Status>` has already arrived and the root frame is closed, but the parser cannot
/// reach it, so the buffer reads as incomplete and the command spends its whole budget waiting for
/// bytes that already came.
///
/// #322 acceptance: framing "can recover a complete root when malformed metadata children would
/// otherwise wedge every poll".
///
/// **Label: client-conformance.**
#[tokio::test]
async fn a_status_whose_child_tag_never_terminates_still_reports_the_root_fields() {
    let hostile = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<Status state=\"2\" track=\"3\" position=\"41\" length=\"293\" volume=\"-23.5\" ",
        "active_rate=\"44100\" active_bits=\"24\">\n",
        // The child tag is never terminated: no `/>` and no `>`. Nothing downstream can parse it,
        // yet the root's own closing tag is right there.
        "<metadata artist=\"Bill Evans\" song=\"Alice in Wonderland\"\n",
        "</Status>"
    );
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            malformed_for_element: Some(("Status".to_string(), hostile.to_string())),
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");

    let status = h.adapter.get_playback_status().await.expect(
        "a Status whose root frame is closed must be readable even when a child tag never \
         terminates; otherwise every poll while a track is loaded costs the whole response budget",
    );

    assert_eq!(
        (status.state, status.position, status.active_rate),
        (2, 41, 44100),
        "the root element's own attributes are readable by a quote-aware scan regardless of its \
         children, so a closed root must yield the playback fields; got {status:?}"
    );
    h.stop();
}

/// The pure framing boundary for the same rule.
///
/// Recovery must be **narrow**: it applies only when the root's own matching closing tag has
/// actually arrived. A truncated document and a mismatched-nesting document stay rejected, or the
/// recovery has bought child tolerance at the price of the framing guarantees #322 exists to hold.
///
/// **Label: client-conformance** — `framing` is production code.
#[test]
fn the_framer_recovers_a_closed_root_whose_child_tag_never_terminates() {
    let unterminated_child = "<Status state=\"2\">\n<metadata song=\"x\"\n</Status>";
    assert_eq!(
        framing::classify(unterminated_child),
        framing::Framing::Complete,
        "a closed root frame is a complete document even when a child tag never terminates"
    );

    // A stray `<` in child position is the same class of damage.
    let stray_open = "<Status state=\"2\">\n< \n</Status>";
    assert_eq!(
        framing::classify(stray_open),
        framing::Framing::Complete,
        "a stray `<` among the children must not outrank the root's own closing tag"
    );

    // Without the root's closing tag, more may still arrive: recovery must not invent it.
    assert_eq!(
        framing::classify("<Status state=\"2\">\n<metadata song=\"x\"\n"),
        framing::Framing::Incomplete,
        "recovery must not invent a closing tag that has not arrived"
    );

    // Mismatched nesting stays malformed — recovery keys on the root's OWN closing tag.
    assert_eq!(
        framing::classify("<State state=\"2\"></Status>"),
        framing::Framing::Malformed,
        "recovery must not weaken the mismatched-nesting rejection"
    );

    // A stray closing tag with no root stays malformed.
    assert_eq!(
        framing::classify("</Status>"),
        framing::Framing::Malformed,
        "recovery must not rescue a leftover closing tag into a document"
    );
}

/// UHC's substring-and-root-scope design is already immune to the hostile *attribute value* class,
/// so no production change was needed for it: the children are never parsed and attribute reads stop
/// at the root tag's own `>`. The reference implementation needs a recovery path for these because it
/// XML-parses whole documents; UHC does not.
///
/// **Label: client-conformance** (a regression pin: it holds today, and is pinned so a future change cannot remove the property silently). This passed before the amendment and is not a fix. It exists so a future
/// change that starts parsing children cannot silently reintroduce a poll-wedging failure.
#[test]
fn the_framer_already_tolerates_hostile_attribute_values_in_children() {
    for (what, doc) in [
        (
            "unescaped <",
            "<Status state=\"2\">\n<metadata song=\"Blue < Green\"/>\n</Status>",
        ),
        (
            "unescaped \"",
            "<Status state=\"2\">\n<metadata song=\"A \"quoted\" name\"/>\n</Status>",
        ),
        (
            "unescaped >",
            "<Status state=\"2\">\n<metadata song=\"a > b\"/>\n</Status>",
        ),
        (
            "bare &",
            "<Status state=\"2\">\n<metadata song=\"R & B\"/>\n</Status>",
        ),
    ] {
        assert_eq!(
            framing::classify(doc),
            framing::Framing::Complete,
            "a hostile attribute value in a child ({what}) must not affect framing"
        );
        assert_eq!(
            framing::root_open_tag(doc),
            Some("<Status state=\"2\">"),
            "the root opening tag must be recovered verbatim despite a hostile child ({what})"
        );
    }
}

// -----------------------------------------------------------------------------
// Bite 2 — an explicit accumulated-response byte cap, and recovery after it
// -----------------------------------------------------------------------------

/// The client accumulates a reply into a `String` per command. Before this, the only thing bounding
/// that buffer was the response deadline — a **time** bound, not a **memory** bound. A daemon whose
/// container never closes therefore grows the buffer for as long as the deadline allows, at whatever
/// rate the link sustains.
///
/// Two properties are asserted together, because a cap that wedges the connection is not a fix:
/// the oversized reply must fail *naming the ceiling*, and the very next command must succeed.
///
/// Ownership: the maintainer decision on issue #322 assigns the bounded framing primitive to this
/// issue because this PR already owns the framer; #347 inherits it rather than reimplementing it.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn an_unbounded_reply_is_rejected_by_an_explicit_byte_cap_and_the_next_command_still_works() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            // Comfortably past any real document: the largest observed container is a 77-entry
            // filter list, a few KB.
            oversized_for_element: Some(("GetFilters".to_string(), 6 * 1024 * 1024)),
            ..WirePolicy::default()
        },
        HqpTimeouts {
            // Deliberately generous. A deadline this long means a timeout would be the WRONG
            // answer: the cap has to be what ends this, and the assertion below checks which did.
            response: Duration::from_secs(4),
            max_attempts: 1,
            ..fast_timeouts()
        },
    )
    .await;
    h.adapter.connect().await.expect("connect");

    let err = h
        .adapter
        .get_filters()
        .await
        .expect_err("a reply that never closes must not be accumulated without limit");
    let message = err.to_string();

    assert!(
        message.contains("exceeded") && message.contains("bytes"),
        "the failure must name the byte ceiling it hit, so an operator can tell an oversized reply \
         from a slow one; a bare timeout here would mean the buffer is still unbounded and only the \
         clock stopped it. Got: {message}"
    );
    assert!(
        h.server.oversized_fired(),
        "the oversized reply must actually have been served"
    );

    // Recovery: the fault is one-shot, so the next command meets a healthy daemon. It can only
    // succeed if the failed command left no junk in the client's stream.
    let state = h
        .adapter
        .get_state()
        .await
        .expect("an oversized reply must not wedge the connection for later commands");
    assert_eq!(
        state.state, 0,
        "the recovered command must read the daemon's real state, not leftovers from the \
         oversized reply; got {state:?}"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// Bite 3 — the loaded-chain axis
//
// Capability, not client behaviour. #347 owns every client-side consequence: noticing a chain
// change, invalidating enumerations, suppressing a rate pin under `[source]`, skipping a no-op mode
// write. The tests below prove the fake can now *express* those situations, which is what unblocks
// #347. They are model-fidelity and are not client reds — no production stub was added to make any
// of them fail first, because a stub would be manufactured evidence rather than a defect.
// -----------------------------------------------------------------------------

/// Before this bite, `Inner::sdm()` was `mode_index == 2`, so `[source]` served the PCM lists
/// unconditionally and a source-following session was inexpressible.
///
/// Provenance of the behaviour being modelled: **derived-upstream, tier-2-only.** Source-following is
/// an upstream observation on hqplayerd 6.0.4 (Opal), one host, not UHC-qualified (#332), and the
/// tier-1 read-only gate cannot promote it — tier 1 sees only the chain already loaded.
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn the_loaded_chain_moves_under_source_while_the_configured_mode_stays_source() {
    let h = Harness::verified().await;
    h.model.external_change(|s| s.mode_index = 0); // configured `[source]`

    let pcm_filters = h.model.current_enumeration("GetFilters");
    h.model.source_loads_chain(LoadedChain::Sdm);
    let sdm_filters = h.model.current_enumeration("GetFilters");

    assert_eq!(
        h.model.state().mode_index,
        0,
        "the configured mode must be untouched: a source change is not a mode change"
    );
    assert_ne!(
        pcm_filters, sdm_filters,
        "the enumeration served must follow the LOADED chain, so a source change swaps the lists \
         while `State.mode` stays 0"
    );
    assert_eq!(
        sdm_filters,
        corpus::document(VERIFIED_PROFILE, "filters_sdm"),
        "under a loaded SDM chain the SDM list is the one in force"
    );
    h.stop();
}

/// The fake must be **unable** to settle the independent, unresolved semantics of
/// `State.active_mode` and `Status.active_mode`, which **#341** owns.
///
/// These are not in contradiction. `Status.active_mode` is *measured* to echo the configured mode
/// under `[source]` — which is why the upstream client rejects it as a chain resolver and derives the
/// chain from `Status.active_rate` instead. `State.active_mode` under `[source]` is simply
/// *unmeasured*. Before this bite both fields were derived from `mode_index`, so they agreed by
/// construction and the fake quietly answered the unmeasured half.
///
/// This asserts expressibility in both directions rather than picking a winner: a test that asserted
/// one reading would be the fake deciding.
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn the_fake_does_not_settle_the_independent_state_and_status_active_mode_semantics() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.mode_index = 0; // configured `[source]`
        s.playback = 2;
    });
    h.model.source_loads_chain(LoadedChain::Sdm);

    // Reading 1: Status echoes the configured mode (verified upstream), State resolves the loaded
    // chain (the claim in UHC's own reference document). The two therefore disagree.
    h.model.set_active_mode_reporting(
        ActiveModeReporting::ResolvesLoadedChain,
        ActiveModeReporting::EchoesConfiguredMode,
    );
    let state = h.adapter.get_state().await.expect("State");
    let status = h.adapter.get_playback_status().await.expect("Status");
    assert_eq!(
        (state.active_mode, status.active_mode.as_str()),
        (2, "[source]"),
        "with the two policies set apart, the documents must be able to differ: State resolving the \
         loaded SDM chain while Status echoes `[source]`. Neither reading is asserted as correct - \
         Status echoing is measured, State resolving is unmeasured, and #341 owns settling it"
    );

    // Reading 2: both echo. Now they agree, and `[source]` resolves nothing on either side.
    h.model.set_active_mode_reporting(
        ActiveModeReporting::EchoesConfiguredMode,
        ActiveModeReporting::EchoesConfiguredMode,
    );
    let state = h.adapter.get_state().await.expect("State");
    let status = h.adapter.get_playback_status().await.expect("Status");
    assert_eq!(
        (state.active_mode, status.active_mode.as_str()),
        (0, "[source]"),
        "with both policies echoing, neither source resolves the loaded chain — which is the \
         behaviour the reference document warns about"
    );
    h.stop();
}

/// `SetMode` clears the single rate pin **even when the mode does not change**. Measured upstream
/// 2026-07-28.
///
/// **Label: model-fidelity**, and asserted straight through [`Responder`] with **no adapter in the
/// path**. It drove the real client until #347, which is the issue this expectation named as the
/// owner of the consequence — the client now refuses to send a mode it is already in, so a test that
/// reached this daemon behaviour through `set_mode` could no longer reach it at all. The daemon-side
/// fact is unchanged and is what this pins; the client-side consequence is
/// `a_no_op_mode_write_is_not_sent_so_the_rate_pin_survives`.
///
/// Provenance: **derived-upstream**, pending #332.
#[test]
fn a_same_mode_set_mode_still_clears_the_rate_pin() {
    let model = DaemonModel::verified();
    // Pin a rate. Index 1 is 44100 Hz in the observed PCM list.
    model
        .respond("<SetRate value=\"1\"/>")
        .expect("the fake answers SetRate");
    let pinned = model.state();
    assert_ne!(
        pinned.rate_index, 0,
        "precondition: the pin must actually be set before a mode write can clear it"
    );

    // The mode is already PCM (index 1). This write changes nothing about the mode.
    model
        .respond("<SetMode value=\"1\"/>")
        .expect("the fake answers SetMode");

    let after = model.state();
    assert_eq!(
        (after.mode_index, after.rate_index, after.active_rate_hz),
        (1, 0, 0),
        "a no-op mode write still clears the pin, so a reconciliation loop that writes the mode \
         unconditionally destroys user state; got {after:?}"
    );
}

/// Records the removal of unevidenced fake behaviour.
///
/// The model used to reset `filter_1x`, `filter_nx` and `shaper` to index 0 on every `SetMode`.
/// Upstream says `SetMode` clears the *rate pin* and reloads the chain; nothing says selections
/// return to the first entry. A fake that invents behaviour is the failure mode this issue exists to
/// end — inventing it in `tests/` is only harder to see.
///
/// Indices are still kept inside the loaded chain's list bounds, which is a self-consistency
/// invariant (a daemon never reports an index outside its own list) and not a claim that selections
/// survive a chain change.
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn set_mode_does_not_reset_the_filter_and_shaper_selections() {
    let h = Harness::verified().await;
    let chosen = corpus::index_of(
        &corpus::document(VERIFIED_PROFILE, "filters_pcm"),
        "FiltersItem",
        "poly-sinc-gauss-long",
    )
    .expect("the name is in the observed PCM list");
    h.adapter
        .set_filter_nx("poly-sinc-gauss-long")
        .await
        .applied()
        .expect("SetFilter");
    assert_eq!(h.model.state().filter_nx_index, chosen, "precondition");

    h.adapter
        .set_mode("PCM")
        .await
        .applied()
        .expect("same-mode SetMode");

    assert_eq!(
        h.model.state().filter_nx_index,
        chosen,
        "a mode write must not invent a return to the first filter: that reset was never observed"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// Bite 4 — the stale-chain hazard, made expressible and in-range
// -----------------------------------------------------------------------------

/// The hazard shape, on the **synthetic** profile.
///
/// A stale index from the other chain must land **in range** and name a **different** filter. Out of
/// range would draw `result="Error"`, the client would surface a failure, and the test would prove the
/// opposite of the hazard: a loud rejection is safe, a quiet wrong answer is not.
///
/// This lives in `synthetic-chain-hazard`, whose every name is fictional (`SYN-*`). An earlier
/// revision padded the Opal SDM fixture with rows copied from the PCM one, which was a new SDM claim
/// even though no number changed. The evidence corpus and a constructed hazard are different things
/// and now live in different places.
///
/// **Label: model-fidelity.**
#[test]
fn a_stale_cross_chain_filter_index_lands_in_range_on_a_different_filter() {
    let pcm = corpus::document(SYNTHETIC_HAZARD_PROFILE, "filters_pcm");
    let sdm = corpus::document(SYNTHETIC_HAZARD_PROFILE, "filters_sdm");
    let pcm_entries = corpus::enum_entries(&pcm, "FiltersItem");
    let sdm_entries = corpus::enum_entries(&sdm, "FiltersItem");

    assert!(
        !pcm_entries.is_empty() && pcm_entries.len() == sdm_entries.len(),
        "both synthetic chains must be the same length, so every index resolves in both"
    );
    for entry in &pcm_entries {
        let facing = sdm_entries
            .iter()
            .find(|e| e.index == entry.index)
            .map(|e| e.name.as_str());
        assert!(
            facing.is_some(),
            "index {} must exist in the other chain, or a stale write would be rejected out of \
             range instead of silently selecting the wrong filter",
            entry.index
        );
        assert_ne!(
            facing,
            Some(entry.name.as_str()),
            "index {} must name a DIFFERENT filter in the other chain, which is what makes a cached \
             list from one chain wrong for the other",
            entry.index
        );
    }
}

/// The fake capability, exercised **without the adapter**.
///
/// The previous form of this expectation drove the real client through its stale cache and asserted
/// the daemon ended on the wrong filter. That required today's broken cache behaviour to hold, so it
/// would have failed the moment #347 correctly invalidates and re-enumerates — omitting an assertion
/// on the adapter's return value did not make it client-independent. The observed probe is recorded on
/// #347, where the fix lives.
///
/// What #322 owns is that the fake can serve a chain-relative list at all: the *same* `SetFilter`
/// index must select a *different* filter depending only on which chain is loaded, with the configured
/// mode untouched. That is asserted here straight through [`Responder`], so it stays green whatever
/// #347 does to the client.
///
/// **Label: model-fidelity.**
#[test]
fn the_same_filter_index_selects_a_different_filter_per_loaded_chain() {
    let model = DaemonModel::with_profile(SYNTHETIC_HAZARD_PROFILE);
    model.external_change(|s| {
        s.mode_index = 0; // configured `[source]`: the source decides the chain
        s.playback = 2;
    });
    let index = 7;

    let active_filter_after_setting = |chain: LoadedChain| -> String {
        model.source_loads_chain(chain);
        model
            .respond(&format!("<SetFilter value=\"{index}\"/>"))
            .expect("the fake answers SetFilter");
        let status = model.respond("<Status/>").expect("the fake answers Status");
        framing::root_open_tag(&status)
            .and_then(|tag| {
                let key = " active_filter=\"";
                tag.find(key).map(|at| {
                    tag[at + key.len()..]
                        .split('"')
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
            })
            .expect("Status carries active_filter")
    };

    let in_pcm = active_filter_after_setting(LoadedChain::Pcm);
    let in_sdm = active_filter_after_setting(LoadedChain::Sdm);

    assert_ne!(
        in_pcm, in_sdm,
        "the same index {index} must select a different filter per loaded chain, which is the whole \
         reason a cached enumeration cannot outlive a chain change; got {in_pcm:?} both times"
    );
    assert_eq!(
        model.state().mode_index,
        0,
        "and the configured mode never moved: a source change is not a mode change"
    );
}

/// A synthetic profile must never be mistaken for, or promoted to, evidence.
///
/// Guards the boundary the Codex gate drew: constructed hazard shapes and observed protocol evidence
/// live in different profiles, and the corpus must be able to tell them apart mechanically rather
/// than by a reader noticing a header.
///
/// **Label: model-fidelity.**
#[test]
fn a_synthetic_profile_is_never_evidence() {
    for fixture in corpus::all_in(SYNTHETIC_HAZARD_PROFILE) {
        assert_eq!(
            fixture.provenance.status, "synthetic",
            "{}/{} must be marked synthetic",
            SYNTHETIC_HAZARD_PROFILE, fixture.name
        );
        assert!(
            !fixture.provenance.is_verified(),
            "a synthetic fixture must never read as verified"
        );
        assert_eq!(
            fixture.provenance.tier, "never-promotable",
            "a synthetic fixture must be marked never-promotable, so no tier can lift it into evidence"
        );
        assert!(
            fixture.document.contains("SYN-"),
            "every synthetic name must be visibly fictional, so a row cannot be copied into the \
             evidence corpus by mistake"
        );
    }
}

/// `source_chain` is a closed vocabulary, asserted **exactly** (#341 execute review, HQP-C-057).
///
/// The other source_chain checks use `source_chain.contains("read-via-report")`, which passes even
/// when the field also carries explanatory prose — which is how thirteen fixtures kept whole
/// sentences inside the closed-vocabulary field, five caught in an earlier pass and eight more here.
/// This asserts the field is *exactly* one of the allowed tokens, so prose (or any drift) fails here
/// rather than being masked by a substring match. The explanation of how a fixture was obtained
/// belongs in `notes`, never in this field.
///
/// **Label: model-fidelity.**
#[test]
fn source_chain_is_exactly_a_closed_vocabulary_token() {
    // `read-via-report` is the only recorded chain; `unrecorded` is the parser default for fixtures
    // that cite no upstream file. Kept here, outside the parser, so the corpus cannot widen its own
    // vocabulary.
    const KNOWN: [&str; 2] = ["read-via-report", "unrecorded"];
    let offenders: Vec<String> = corpus::profiles()
        .iter()
        .flat_map(|p| {
            let p = p.clone();
            corpus::all_in(&p)
                .into_iter()
                .map(move |f| (p.clone(), f.name, f.provenance.source_chain))
        })
        .filter(|(_, _, chain)| !KNOWN.contains(&chain.as_str()))
        .map(|(profile, name, chain)| format!("{profile}/{name}: source_chain = {chain:?}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "source_chain must be exactly one of {KNOWN:?}; explanations belong in `notes`, not embedded \
         in this closed-vocabulary field. Offenders (prose or unknown value):\n{offenders:#?}"
    );
}

/// `corpus::carries` is extension-aware, not `.xml`-only (CodeRabbit review 4823413965, finding 4).
///
/// The stateful model's fixture lookup used to hardcode `{profile}/{stem}.xml` to decide whether a
/// profile carried a document; a fixture the profile carries only as `.html` was therefore missed and
/// the lookup silently fell back to the verified profile. `carries` centralises the extension/root
/// policy `load` already owns, and this pins that it honours both extensions so the hardcoded `.xml`
/// cannot creep back.
///
/// **Label: model-fidelity.**
#[test]
fn corpus_carries_is_extension_aware_not_xml_only() {
    assert!(
        corpus::carries("hqpd-6.0.4-opal", "modes"),
        "modes is present as .xml"
    );
    assert!(
        corpus::carries("hqpd-6.0.4-opal", "config_profile_form"),
        "config_profile_form is present as .html — a .xml-only check would miss it"
    );
    assert!(
        !corpus::carries("hqpd-6.0.4-opal", "no_such_fixture_stem"),
        "a stem the profile does not carry must be false"
    );
}

/// The `tier` vocabulary is closed (CodeRabbit review 4816484338).
///
/// `tier` answers "which kind of run could promote this fixture's claims to evidence", and only one
/// assertion consumes it today, so a typo or an invented value would sit unnoticed and a reader would
/// draw a conclusion from a label that means nothing. Pinning the set is what makes the field
/// auditable rather than decorative.
///
/// **Label: model-fidelity.**
#[test]
fn every_fixture_tier_is_a_known_classification() {
    // Kept here rather than in `corpus.rs` deliberately: the corpus must not be able to widen its own
    // vocabulary, so the list a fixture is checked against lives outside the parser that reads it.
    const KNOWN: [&str; 4] = [
        "tier-1",
        "tier-2-only",
        "never-promotable",
        // The default for fixtures written before the distinction existed.
        "unspecified",
    ];

    let unknown: Vec<String> = corpus::profiles()
        .iter()
        .flat_map(|p| {
            let p = p.clone();
            corpus::all_in(&p)
                .into_iter()
                .map(move |f| (p.clone(), f.name, f.provenance.tier))
        })
        .filter(|(_, _, tier)| !KNOWN.contains(&tier.as_str()))
        .map(|(profile, name, tier)| format!("{profile}/{name}: tier = {tier:?}"))
        .collect();

    assert!(
        unknown.is_empty(),
        "these fixtures carry a tier outside the documented vocabulary {KNOWN:?}: {unknown:#?}"
    );
}

/// A device-dependent read-only claim is tier 1 (CodeRabbit review 4816484338).
///
/// `GetModes` is a query, so verifying a PCM-only device's mode list requires no mutation. The tier-1
/// run must use a daemon configured with the hardware the fixture describes; a different DAC does not
/// verify this hardware-dependent claim.
///
/// **Label: model-fidelity.**
#[test]
fn a_device_dependent_modes_claim_is_tier_one() {
    let fixture = corpus::load("hqpd-6.0.4-pcm-only-dac", "modes");
    assert_eq!(
        fixture.provenance.tier, "tier-1",
        "GetModes is read-only; the qualifying tier-1 run must use the described hardware"
    );
    assert_eq!(
        fixture.provenance.hardware, "pcm-only-dac",
        "the fixture must carry a machine-checkable hardware requirement, not only prose"
    );
}

/// Selecting a hardware-dependent profile is not proof that the matching device is attached.
/// The live gate must require an independently supplied marker and refuse absent or mismatched ones.
///
/// **Label: model-fidelity.**
#[test]
fn a_hardware_dependent_tier_one_profile_requires_a_matching_operator_marker() {
    let profile = "hqpd-6.0.4-pcm-only-dac";
    assert!(tier1_hardware_marker(VERIFIED_PROFILE, None).is_ok());

    let absent =
        tier1_hardware_marker(profile, None).expect_err("missing marker must refuse the run");
    assert!(absent.contains("UHC_HQP_CONFORMANCE_HARDWARE=pcm-only-dac"));

    let wrong = tier1_hardware_marker(profile, Some("dsd-capable-dac"))
        .expect_err("a different marker must refuse the run");
    assert!(wrong.contains("requires hardware `pcm-only-dac`"));

    assert!(tier1_hardware_marker(profile, Some("pcm-only-dac")).is_ok());
}

// -----------------------------------------------------------------------------
// Bite 5 — the persistent lane has TWO separately numbered filter domains
// -----------------------------------------------------------------------------

/// The evidenced half of the chain-numbering question, and the one #322 can settle now.
///
/// `hqplayerd.xml` stores the PCM chain's filter under `filter` and the SDM chain's under
/// `oversampling`, and the **same semantic filter carries a different number in each**: 40 under
/// `filter`, 38 under `oversampling`. So the persistent lane is not one enum-ID domain plus a list
/// index — it is *two* stored-ID domains, and a converter that takes a bare number without knowing
/// which attribute produced it cannot be correct for both.
///
/// The upstream evidence names the persistent **field names** `filter` and `oversampling`; it is not
/// a `GetFilters` capture, and the distinction is load-bearing. Whether the *live* enum ID also differs between chains is an open question
/// on #341, so this corpus still reports `value="40"` for the name in both live chain lists and this
/// test deliberately does not resolve 38 against a live list. Renumbering `filters_sdm.xml` on this
/// evidence would have turned a persistent-lane fact into an unmeasured live-lane claim.
///
/// **Label: model-fidelity** (a corpus property; no client reads `oversampling` yet — the persistent
/// write lane is #330's).
#[test]
fn the_persistent_lane_numbers_the_same_filter_differently_per_chain() {
    let config = corpus::document(VERIFIED_PROFILE, "persistent_config");
    let pcm_stored = corpus::config_attr(&config, "output", "filter").expect("filter attribute");
    let sdm_stored =
        corpus::config_attr(&config, "output", "oversampling").expect("oversampling attribute");

    assert_ne!(
        pcm_stored, sdm_stored,
        "the PCM `filter` domain and the SDM `oversampling` domain number the same semantic filter \
         differently, so a stored number is meaningless without the attribute it came from"
    );

    // And neither stored number is the live list index of that filter, which is the original
    // cross-lane property extended to the second domain.
    let filters = corpus::document(VERIFIED_PROFILE, "filters_pcm");
    let live_index = corpus::index_of(&filters, "FiltersItem", "poly-sinc-gauss-long")
        .expect("the name is in the observed PCM list");
    for stored in [&pcm_stored, &sdm_stored] {
        assert_ne!(
            stored.parse::<u32>().ok(),
            Some(live_index),
            "a persistent stored ID ({stored}) must not coincide with the live list index \
             ({live_index}); if it did, a conversion that served both lanes could pass by accident"
        );
    }
}

/// The `oversampling` number must not be usable on the live lane either, for the same reason
/// `filter` is not: it is a stored ID from a different domain.
///
/// **Label: client-conformance** — this drives the real client and would fail if a future change made
/// the live lane accept a persistent number.
#[tokio::test]
async fn feeding_the_persistent_oversampling_id_to_the_live_lane_is_rejected() {
    let h = Harness::verified().await;
    let config = corpus::document(VERIFIED_PROFILE, "persistent_config");
    let stored: u32 = corpus::config_attr(&config, "output", "oversampling")
        .expect("oversampling attribute")
        .parse()
        .expect("numeric");

    // Put the SDM chain in force, so this is the live lane the stored SDM domain corresponds to. The
    // source only decides the chain in configured `[source]` mode, so that is set first rather than
    // asserting a configured-PCM/loaded-SDM state no daemon produces.
    h.model.external_change(|s| s.mode_index = 0);
    h.model.source_loads_chain(LoadedChain::Sdm);
    let sdm_entries = corpus::enum_entries(
        &corpus::document(VERIFIED_PROFILE, "filters_sdm"),
        "FiltersItem",
    );
    assert!(
        !sdm_entries.iter().any(|e| e.index == stored),
        "precondition: the stored oversampling ID {stored} must be outside the live SDM list's \
         index range for this to be a rejection rather than a misselection"
    );

    let result = h.adapter.set_filter(stored, stored).await;
    assert!(
        result.is_err(),
        "the persistent SDM domain's stored ID {stored} is not a live list index; sending it on the \
         live lane must not silently succeed"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// Bite 6 — the fidelity tail
// -----------------------------------------------------------------------------

/// Under a configured `[source]` mode the daemon accepts every rate pin and applies none of it.
/// Verified upstream 2026-07-29 mid-playback, twice, and confirmed on the output (`Status.active_rate`)
/// as well as on the slot (`State.rate`). Playback is not what blocks it — `[source]` is. What governs
/// the rate there is a persistent config limit for which **no wire command exists**, so retrying on
/// the live lane can never succeed.
///
/// **Provenance: derived-upstream, tier-2-only, pending #332.**
///
/// **Label: model-fidelity**, asserted straight through [`Responder`] with **no adapter in the path**.
/// It drove the real client until #347 — the issue it named as the owner — which now *suppresses* the
/// write under `[source]` rather than sending it and failing on readback arithmetic. A client that
/// does not send the pin cannot demonstrate a daemon that ignores it, so the daemon-side fact is
/// pinned here directly. The client-side consequence is
/// `a_rate_pin_under_source_is_suppressed_before_it_reaches_the_daemon`.
#[test]
fn a_nonzero_rate_pin_under_source_is_accepted_and_ignored() {
    let model = DaemonModel::verified();
    model.external_change(|s| {
        s.mode_index = 0; // configured `[source]`
        s.playback = 2;
    });
    model.arm(|f| f.source_refuses_rate_pin = true);

    // Index 9 is 705600 Hz in the observed PCM list.
    let reply = model
        .respond("<SetRate value=\"9\"/>")
        .expect("the fake answers SetRate");
    assert!(
        reply.contains("result=\"OK\""),
        "the daemon answers OK, which is exactly why OK is not proof; got {reply}"
    );

    let after = model.state();
    assert_eq!(
        (after.rate_index, after.active_rate_hz),
        (0, 0),
        "under `[source]` the pin must be refused on BOTH the slot and the output, which is what the \
         upstream probes checked and why checking only `State.rate` produced a plausible wrong \
         answer; got {after:?}"
    );
}

/// The narrower form of the same defect, and the one readback **cannot** catch.
///
/// A request for Auto (index 0) under `[source]` is ignored exactly like any other, but the readback
/// compares expected 0 against observed 0 and therefore reports **success** for a command that had no
/// effect. Equality with pre-existing state is not proof that a setter applied.
///
/// Recorded on #347 in comment 5125915480. Asserted straight through [`Responder`] with **no adapter
/// in the path**: the daemon answers `OK` and moves nothing, so a readback comparing 0 against 0
/// would call it applied. That is the reason #347's client suppresses the write instead of verifying
/// it — the client-side consequence is
/// `an_auto_rate_request_under_source_is_never_reported_as_applied`.
///
/// **Label: model-fidelity.**
#[test]
fn an_auto_rate_request_under_source_is_ignored_and_readback_cannot_tell() {
    let model = DaemonModel::verified();
    model.external_change(|s| {
        s.mode_index = 0;
        s.playback = 2;
    });
    model.arm(|f| f.source_refuses_rate_pin = true);
    let before = model.state();
    assert_eq!(
        before.rate_index, 0,
        "precondition: the rate must already be unpinned, which is what makes the readback blind"
    );

    // Auto is rate 0, which the observed list carries at index 0.
    let reply = model
        .respond("<SetRate value=\"0\"/>")
        .expect("the fake answers SetRate");
    assert!(
        reply.contains("result=\"OK\""),
        "the daemon acknowledges it, got {reply}"
    );

    let after = model.state();
    assert_eq!(
        (after.rate_index, after.active_rate_hz),
        (before.rate_index, before.active_rate_hz),
        "nothing moved, which is the point: a readback comparing 0 to 0 sees a match and calls an \
         ignored command applied"
    );
}

/// A DAC that cannot do DSD yields a modes list with **no SDM entry**, and the remaining entries keep
/// their own indices — `[source]` stays 0, PCM stays 1, nothing is renumbered to close the gap. That
/// is what makes a hardcoded position dangerous rather than merely wrong: a caller that assumed index
/// 2 is SDM still finds something on a device that has it, and finds the wrong thing on one that does
/// not.
///
/// The client-side consequence — matching a mode by semantic prefix or alias, so that the daemon's
/// `"SDM (DSD)"` resolves for a caller who asked for `"DSD"` — is **#347's** acceptance criterion.
/// #322 supplies the device fixture.
///
/// **Provenance: derived-upstream, tier-2-only, pending #332.** **Label: model-fidelity.**
#[tokio::test]
async fn a_device_without_dsd_omits_sdm_while_the_remaining_mode_indices_stay_intact() {
    let h = Harness::start(
        "hqpd-6.0.4-pcm-only-dac",
        WirePolicy::default(),
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");

    let modes = h.adapter.get_modes().await.expect("GetModes");
    let names: Vec<&str> = modes.iter().map(|m| m.name.as_str()).collect();
    let indices: Vec<u32> = modes.iter().map(|m| m.index).collect();

    assert_eq!(
        (names.as_slice(), indices.as_slice()),
        (["[source]", "PCM"].as_slice(), [0, 1].as_slice()),
        "SDM is absent and the survivors keep their original indices rather than being renumbered"
    );
    assert!(
        h.adapter.set_mode("SDM (DSD)").await.is_err(),
        "a mode the device does not offer must not resolve to some other mode's index"
    );
    h.stop();
}

/// Every fixture records the playback state it was captured under.
///
/// The upstream evidence base's own caveat is that its probes ran with the engine **stopped**, while
/// UHC's users are playing — so a behaviour verified idle is not thereby verified under load, and a
/// corpus that does not record which cannot tell the difference. Most of this corpus is derived from a
/// protocol reference rather than captured from a session, so most fixtures honestly say `unknown`.
/// **That most say `unknown` is the finding**, not a gap in the check.
///
/// **Label: model-fidelity.**
#[test]
fn every_corpus_fixture_records_the_playback_state_it_was_captured_under() {
    // `not-applicable` is for a synthetic fixture: it was never captured, so there is no playback
    // state to record and `unknown` would wrongly imply an observation whose conditions were lost.
    let allowed = ["active", "idle", "unknown", "not-applicable"];
    let mut checked = 0;
    for profile in corpus::profiles() {
        for fixture in corpus::all_in(&profile) {
            assert!(
                allowed.contains(&fixture.provenance.playback.as_str()),
                "fixture {}/{} records playback {:?}, which is not one of {allowed:?}",
                profile,
                fixture.name,
                fixture.provenance.playback
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 20,
        "the whole corpus must be covered, got {checked} fixtures"
    );
}

/// A bare `&` in an attribute value survives decoding rather than swallowing the text after it.
///
/// **Label: client-conformance** (a regression pin: it holds today, and is pinned so a future change cannot remove the property silently). This passed before the amendment: `decode_entities`' fall-through
/// already keeps a bare `&` and advances. Pinned because nothing covered it, so a future change to
/// entity handling could have removed the property silently.
#[test]
fn a_bare_ampersand_in_an_attribute_value_is_preserved() {
    assert_eq!(
        framing::decode_entities("R & B and Rock &amp; Roll"),
        "R & B and Rock & Roll",
        "a bare ampersand is kept verbatim while a real entity beside it still decodes"
    );
    assert_eq!(
        framing::decode_entities("Simon & Garfunkel"),
        "Simon & Garfunkel",
        "and a bare ampersand must not consume the words that follow it"
    );
}

/// A closed-but-malformed `Status` followed by a **coalesced unsolicited `Status`** must still be
/// recovered. Both halves are documented daemon behaviour — it emits malformed XML inside
/// `<metadata>`, and it pushes `Status` frames of its own accord — so the combination is not a corner
/// case, and a client that waits out its deadline on it is broken for the same reason it was broken
/// on the malformed document alone.
///
/// The earlier form of this expectation asserted `Incomplete` here and called it a stated limit. That
/// was encoding a known defect as expected behaviour, which is exactly what this suite claims not to
/// do.
///
/// Recovery finds the **first root-frame boundary it can defend**, and the defences are kept:
/// a `</Status>` literal inside an attribute value is not a boundary, a truncated document is still
/// incomplete, and a mismatched root is still malformed.
///
/// **Label: client-conformance.**
#[test]
fn a_closed_malformed_root_is_recovered_even_with_a_coalesced_push_frame() {
    let hostile_alone = "<Status state=\"2\">\n<metadata song=\"x\"\n</Status>\n";
    assert_eq!(
        framing::classify(hostile_alone),
        framing::Framing::Complete,
        "a hostile document arriving alone is recovered"
    );

    let hostile_then_push =
        "<Status state=\"2\">\n<metadata song=\"x\"\n</Status>\n<Status state=\"1\"/>\n";
    assert_eq!(
        framing::classify(hostile_then_push),
        framing::Framing::Complete,
        "and so is the same document with one of the daemon's push frames coalesced behind it: both \
         halves are documented behaviour, so waiting out the deadline here would be a defect rather \
         than a limit"
    );

    // Defences that the first-defensible-boundary rule must not give up.
    //
    // These two exercise the quote-awareness specifically. A shorter case —
    // `<Status note="</Status>"` — also reads Incomplete, but for an unrelated reason: the root tag
    // itself never closes, so no root name is recovered and the scan never runs. It would have passed
    // whatever the boundary rule did, which makes it worthless as a defence.
    assert_eq!(
        framing::classify("<Status a=\"1\">\n<metadata song=\"</Status>\"/>\n"),
        framing::Framing::Incomplete,
        "with the root open and a closing-tag literal sitting inside a CHILD attribute value, that \
         literal is data and must not be taken for the frame boundary"
    );
    assert_eq!(
        framing::classify("<Status a=\"1\">\n<metadata song=\"</Status>\"\n</Status>\n"),
        framing::Framing::Complete,
        "and when a real closing tag follows the literal, the boundary is the real one — the literal \
         is skipped rather than ending the scan"
    );
    assert_eq!(
        framing::classify("<Status state=\"2\">\n<metadata song=\"x\"\n"),
        framing::Framing::Incomplete,
        "a document truncated before its root close is still incomplete"
    );
    assert_eq!(
        framing::classify("<State state=\"2\"></Status>"),
        framing::Framing::Malformed,
        "a mismatched root is still malformed"
    );
    assert_eq!(
        framing::classify("</Status>"),
        framing::Framing::Malformed,
        "a leftover closing tag is still malformed"
    );

    // XML permits either quote form, and a literal inside a SINGLE-quoted attribute is data just as
    // much as one inside a double-quoted attribute. Tracking only `"` would let `'</Status>'` end the
    // frame, so a truncated document would read as complete.
    assert_eq!(
        framing::classify("<Status a='1'>\n<metadata song='</Status>'/>\n"),
        framing::Framing::Incomplete,
        "a closing-tag literal inside a SINGLE-quoted attribute value is data, not a frame boundary"
    );
    assert_eq!(
        framing::classify("<Status a='1'>\n<metadata song='</Status>'\n</Status>\n"),
        framing::Framing::Complete,
        "and with a real closing tag after it, the boundary is the real one"
    );
    // Mixed forms: a double quote inside a single-quoted value is content and must not open a region.
    assert_eq!(
        framing::classify("<Status a='he said \"hi\"'>\n<metadata song='</Status>'/>\n"),
        framing::Framing::Incomplete,
        "a double quote inside a single-quoted value must not toggle quoting, or the scan loses track \
         of which regions are data"
    );

    // A closing-tag literal is content, not markup, in every region XML says so — and that set is
    // CLOSED, not open-ended: a quoted attribute value (above), a comment, a CDATA section, and a
    // processing instruction. Taking any of them for a boundary would let a TRUNCATED document pass
    // as complete, the one failure recovery must never produce.
    //
    // Enumerated deliberately. Three of these were found one at a time by probing, each after the
    // previous was called complete; the fourth came from asking what the whole set is.
    for (what, open, close) in [
        ("a comment", "<!--", "-->"),
        ("CDATA", "<![CDATA[", "]]>"),
        ("a processing instruction", "<?pi", "?>"),
    ] {
        assert_eq!(
            framing::classify(&format!("<Status a=\"1\">{open} </Status> {close}")),
            framing::Framing::Incomplete,
            "a closing tag inside {what} must not end the frame, or a truncated document reads as \
             complete"
        );
        assert_eq!(
            framing::classify(&format!(
                "<Status a=\"1\">{open} </Status> {close}</Status>"
            )),
            framing::Framing::Complete,
            "and with a real closing tag after {what}, the boundary is the real one"
        );
        assert_eq!(
            framing::classify(&format!("<Status a=\"1\">{open} </Status>")),
            framing::Framing::Incomplete,
            "an unterminated {what} consumes the remainder: there is no boundary inside something \
             that has not ended"
        );
    }

    // A declaration that is neither comment nor CDATA — a DOCTYPE is only legal in the prologue, so
    // one here is already malformed — is skipped to its closing `>` for the same reason.
    assert_eq!(
        framing::classify("<Status a=\"1\"><!DOCTYPE x [ </Status> ]>"),
        framing::Framing::Incomplete,
        "a closing tag inside a declaration must not end the frame"
    );
}

/// A **newline-free** oversized reply. The cap must be a bound on what is *allocated*, not merely on
/// what is *retained*.
///
/// A line-oriented reader cannot defend against this shape: `read_line` grows its target until it
/// finds a `\n` or the peer closes, so a ceiling checked after the read has already allocated
/// whatever the daemon chose to send. The previous implementation documented its peak as "the cap
/// plus one line" as though naming the hole closed it; one line is unbounded.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn a_newline_free_oversized_reply_is_refused_before_it_is_allocated() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            oversized_newline_free_for_element: Some(("GetFilters".to_string(), 6 * 1024 * 1024)),
            ..WirePolicy::default()
        },
        HqpTimeouts {
            // Generous on purpose: a timeout here would mean the ceiling never ran.
            response: Duration::from_secs(4),
            max_attempts: 1,
            ..fast_timeouts()
        },
    )
    .await;
    h.adapter.connect().await.expect("connect");

    let err = h
        .adapter
        .get_filters()
        .await
        .expect_err("a reply with no newline must still be refused");
    let message = err.to_string();

    assert!(
        message.contains("exceeded") && message.contains("bytes"),
        "the failure must name the byte ceiling. A timeout instead would mean the reader waited for a \
         newline that never came while accumulating without limit. Got: {message}"
    );
    assert!(
        h.server.oversized_fired(),
        "the newline-free reply must actually have been served"
    );

    let state = h
        .adapter
        .get_state()
        .await
        .expect("the connection must be usable again after the refusal");
    assert_eq!(state.state, 0, "and it must read the daemon, not leftovers");
    h.stop();
}

/// A **delayed** `SetMode` must not leave a chain-scoped index outside the newly loaded chain's lists.
///
/// This is the case the review found and the repair had no coverage for. `apply()` defers a delayed
/// change onto the pending queue, so the setter arm's clamp runs against state the mode change has not
/// touched yet; the real change lands later, inside `tick_pending`, where the invariant has to be
/// re-applied. Without that, the fake reports an index its own enumeration cannot resolve — which is
/// the one thing a conformance fake must never do, because every downstream assertion trusts it.
///
/// The synthetic chains are equal length, so this uses the Opal profile deliberately: its SDM excerpt
/// is *shorter* than its PCM excerpt, which is what makes an out-of-range index reachable at all.
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn a_delayed_set_mode_still_clamps_indices_into_the_loaded_chain() {
    let h = Harness::verified().await;
    let pcm_len = corpus::enum_entries(
        &corpus::document(VERIFIED_PROFILE, "filters_pcm"),
        "FiltersItem",
    )
    .len();
    let sdm_len = corpus::enum_entries(
        &corpus::document(VERIFIED_PROFILE, "filters_sdm"),
        "FiltersItem",
    )
    .len();
    assert!(
        sdm_len < pcm_len,
        "precondition: the SDM excerpt must be shorter than the PCM one, or no index can fall out of \
         range across the change; got {sdm_len} and {pcm_len}"
    );

    // Sit on a PCM index that does not exist in the SDM chain.
    let beyond_sdm = (pcm_len - 1) as u32;
    h.model.external_change(|s| {
        s.filter_1x_index = beyond_sdm;
        s.filter_nx_index = beyond_sdm;
    });

    // The mode change is accepted now and applies two polls later.
    h.model
        .arm(|f| f.apply_after_polls.push(("SetMode".to_string(), 2)));
    let _ = h.adapter.set_mode("SDM (DSD)").await.applied();

    // Poll until the deferred change has landed. Each State read ticks the pending queue.
    for _ in 0..4 {
        let _ = h.adapter.get_state().await;
    }

    let after = h.model.state();
    assert_eq!(
        after.loaded_chain,
        LoadedChain::Sdm,
        "precondition: the deferred mode change must actually have landed; got {after:?}"
    );
    assert!(
        (after.filter_1x_index as usize) < sdm_len && (after.filter_nx_index as usize) < sdm_len,
        "a delayed mode change must leave every chain-scoped index inside the newly loaded chain's \
         list; SDM has {sdm_len} entries and the fake reports 1x={} Nx={}",
        after.filter_1x_index,
        after.filter_nx_index
    );

    // And the fake's own enumeration must be able to resolve what it reports.
    let status = h.adapter.get_playback_status().await.expect("Status");
    assert!(
        !status.active_filter.is_empty(),
        "an index the enumeration cannot resolve renders as an empty active_filter, which is how a \
         self-inconsistent fake leaks into every assertion that trusts it"
    );
    h.stop();
}

/// The **source** rate and the **output** rate are different facts and must not collapse into one.
///
/// `Status` carries the source's `samplerate` (mirrored from the `metadata` child) alongside
/// `active_rate`, which is what the engine is actually clocking the output at. Reading either for the
/// other misreports the signal path: a 44.1 kHz source upsampled to 705.6 kHz is one stream with two
/// true rates, and `active_bits` belongs to the output rather than the source.
///
/// The upstream audit found its own UI inferring output depth from the rate and having to be corrected
/// to read `Status.active_bits`. UHC already parses all three fields separately, so this pins the
/// property rather than fixing it.
///
/// The *consuming* semantics — which rate a zone publishes, and how a surface labels them — belong to
/// **#328**. #322 owns the fixture being able to tell them apart at all.
///
/// **Label: client-conformance** (a regression pin: it holds today, and is pinned so a future change
/// cannot collapse the two rates silently).
#[tokio::test]
async fn the_source_rate_and_the_output_rate_are_reported_separately() {
    let h = Harness::verified().await;
    // A 44.1 kHz source being clocked out at 705.6 kHz: deliberately different numbers, and an
    // output depth that could not have been inferred from either rate.
    h.model.external_change(|s| {
        s.playback = 2;
        s.active_rate_hz = 705_600;
        s.metadata = Some(Metadata {
            samplerate: 44_100,
            bits: 24,
            ..Metadata::sample()
        });
    });

    let status = h.adapter.get_playback_status().await.expect("Status");

    assert_eq!(
        (status.samplerate, status.active_rate),
        (44_100, 705_600),
        "the source rate and the output rate must both survive as themselves; collapsing them would \
         make an upsampled stream indistinguishable from a native one. Got {status:?}"
    );
    assert_eq!(
        status.active_bits, 24,
        "and the output depth is a reported field, not something inferred from a rate"
    );
    h.stop();
}

/// Rate resolution is **chain-relative**: the same Hz value is a valid selection in one chain and
/// absent from the other.
///
/// The PCM and SDM rate enumerations do not overlap at all — 44.1 kHz–768 kHz against
/// 2.8 MHz–24.6 MHz — so a client resolving Hz to an index must resolve against the list currently in
/// force. Upstream states the dependency is on mode *and* selected filter, so mode alone is not the
/// whole story; the filter half is recorded as a gap below rather than modelled here.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn a_rate_valid_in_one_chain_is_refused_in_the_other() {
    let h = Harness::verified().await;
    let pcm_only_hz = 705_600;
    let sdm_rates = corpus::enum_entries(
        &corpus::document(VERIFIED_PROFILE, "rates_sdm"),
        "RatesItem",
    );
    assert!(
        !sdm_rates.iter().any(|e| e.rate == Some(pcm_only_hz)),
        "precondition: {pcm_only_hz} Hz must be absent from the SDM enumeration"
    );

    h.adapter
        .set_rate(pcm_only_hz)
        .await
        .applied()
        .expect("a PCM rate resolves and applies while the PCM chain is loaded");

    // Configure SDM through the client, which pins the loaded chain to SDM. This used to move the
    // chain under a configured `[source]`, which since #347 short-circuits before the rate list is
    // ever consulted — the write is suppressed for the mode, so the test would have passed without
    // exercising rate resolution at all. A configured SDM mode reaches the same loaded chain and
    // leaves the resolution path the thing under test.
    h.adapter
        .set_mode("SDM (DSD)")
        .await
        .applied()
        .expect("configure SDM");
    let refused = h.adapter.set_rate(pcm_only_hz).await.applied();

    assert!(
        refused.is_err(),
        "a rate absent from the loaded chain's enumeration must not resolve to some other chain's \
         index; rate resolution is relative to the list in force"
    );
    assert!(
        h.model.state().rate_index == 0,
        "and nothing may have been pinned on the way to that refusal"
    );
    h.stop();
}

/// A default fake claims nothing that is not UHC-qualified.
///
/// The fake now carries several switches whose behaviour rests on upstream observation of one daemon
/// on one host rather than on a UHC capture. Nothing prevents a test combining them into a daemon
/// that has never existed, and a conformance verdict about an impossible daemon is worse than none.
/// This makes the unqualified surface enumerable, so arming it is a visible act rather than an
/// implicit one, and pins that the default arms nothing.
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn a_default_fake_arms_no_unqualified_upstream_claim() {
    let h = Harness::verified().await;
    assert_eq!(
        h.model.armed_upstream_claims(),
        Vec::<&str>::new(),
        "a default model must claim nothing beyond what UHC has qualified, so any test relying on an \
         upstream-only behaviour has to ask for it where a reader can see the request"
    );

    // Arming one makes it enumerable rather than silent.
    h.model.arm(|f| f.source_refuses_rate_pin = true);
    let armed = h.model.armed_upstream_claims();
    assert_eq!(
        armed.len(),
        1,
        "arming an upstream-only behaviour must show up in the claim list; got {armed:?}"
    );
    assert!(
        armed[0].contains("#332"),
        "and each entry must name the qualification it is still waiting on; got {armed:?}"
    );
    h.stop();
}

/// Every fixture whose evidence came through a salvage report says so **in its provenance**.
///
/// Three errors in this amendment came through one channel: a report *about* HQPTuner read as if it
/// were HQPTuner. The Stage 1 dissent caught one (an enum-ID renumbering), and the Codex Stage 2 gate
/// caught two more (a path that does not exist at the ref cited, and a claim the report's own newer
/// companion had already superseded). None of my own review passes caught any of the three.
///
/// So this is a channel rather than three mistakes, and the fix is structural: a fixture that cites an
/// upstream URL must also record that the URL is the *report's* citation and not something read here.
/// A reader can then weigh the claim correctly, and #341 knows exactly which claims still need a
/// first-hand or live confirmation.
///
/// **Label: model-fidelity.**
#[test]
fn every_upstream_citation_records_how_it_was_obtained() {
    let mut checked = 0;
    for profile in corpus::profiles() {
        for fixture in corpus::all_in(&profile) {
            if fixture
                .provenance
                .source
                .contains("github.com/ohshitgorillas")
            {
                assert!(
                    fixture.provenance.source_chain.contains("read-via-report"),
                    "{}/{} cites an upstream URL but does not record that the URL came from a salvage \
                     report rather than from reading the file",
                    profile,
                    fixture.name
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked >= 14,
        "every upstream-citing fixture must be covered, got {checked}"
    );
}

/// A bare `verified` status means **UHC** confirmed it against a live daemon. Nothing in this corpus
/// has that, so nothing may claim it.
///
/// Three fixtures did. Their evidence is a salvage report stating that *upstream* verified the
/// behaviour on a live hqplayerd 6.0.4 — a real and useful claim, but upstream's rather than ours, and
/// second-hand at that. `verified-upstream` says so in the status itself, where a reader weighing the
/// fixture will actually see it, rather than only in the notes.
///
/// `is_verified()` is a prefix match, so these still read as observation claims wherever the corpus
/// distinguishes observed from derived — which is correct. What changes is that the claim now names
/// whose observation it is.
///
/// **Label: model-fidelity.**
#[test]
fn no_fixture_claims_uhc_verified_status_on_second_hand_evidence() {
    for profile in corpus::profiles() {
        for fixture in corpus::all_in(&profile) {
            if fixture.provenance.source_chain.contains("read-via-report") {
                assert_ne!(
                    fixture.provenance.status, "verified",
                    "{}/{} rests on a salvage report, so it must not claim bare `verified` — that \
                     status is reserved for a first-hand UHC capture, which this corpus does not yet \
                     have. Use `verified-upstream`",
                    profile, fixture.name
                );
            }
        }
    }
}

/// An unsolicited document arriving **ahead of** the wanted reply in the *same* read must not send the
/// client back to the socket: the reply it wants is already in the buffer.
///
/// The daemon pushes `Status` frames unprompted, so one landing just before a reply is ordinary
/// traffic, and one read can deliver both. A reader that drains the follower and then immediately
/// blocks on another read is waiting for bytes it already holds — the same class of mistake as the
/// malformed-root wedge, one layer up: correct framing, wrong loop structure.
///
/// The wire falls silent after that single write, so if the client goes back to the socket it can only
/// end in a timeout. That is what makes this observable rather than merely inefficient.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn an_unsolicited_document_ahead_of_the_reply_in_one_read_does_not_block_for_more() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            coalesce_leading_for_element: Some((
                "State".to_string(),
                PUSH_STATUS_FRAME.to_string(),
            )),
            ..WirePolicy::default()
        },
        HqpTimeouts {
            // Short: if the client goes back to the socket it waits this out and fails.
            response: Duration::from_millis(300),
            max_attempts: 1,
            ..fast_timeouts()
        },
    )
    .await;
    h.adapter.connect().await.expect("connect");
    h.model.external_change(|s| s.playback = 2);

    let state = h
        .adapter
        .get_state()
        .await
        .expect("the reply was in the same read as the unsolicited frame ahead of it");

    assert_eq!(
        state.state, 2,
        "the client must answer from the reply already in its buffer rather than reading again; \
         got {state:?}"
    );
    assert!(
        h.adapter.unsolicited_skipped().await > 0,
        "and the leading document must still be counted as skipped, not silently swallowed"
    );
    h.stop();
}

/// The skip ceiling must actually trip.
///
/// The inner drain loop iterates once per buffered document without touching the socket, so the
/// per-command deadline — which only gates socket reads — cannot bound it. `MAX_UNSOLICITED_BACKLOG`
/// is the only thing that does, which makes it load-bearing in a way it was not when every skip cost
/// a read. A burst larger than the ceiling must therefore end in a reported error rather than a spin.
///
/// The existing burst expectation uses twelve frames, comfortably under the ceiling, so it proves the
/// skipping works and says nothing about the bound.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn a_burst_larger_than_the_skip_ceiling_is_reported_rather_than_drained_forever() {
    // Comfortably past the 256 ceiling, delivered ahead of the reply in one write so the whole burst
    // lands in the buffer and the drain loop has to deal with all of it without reading again.
    let burst = vec![PUSH_STATUS_FRAME; 400].join("\n");
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            coalesce_leading_for_element: Some(("State".to_string(), burst)),
            ..WirePolicy::default()
        },
        HqpTimeouts {
            response: Duration::from_secs(4),
            max_attempts: 1,
            ..fast_timeouts()
        },
    )
    .await;
    h.adapter.connect().await.expect("connect");

    let err = h
        .adapter
        .get_state()
        .await
        .expect_err("a burst past the ceiling must be reported");
    let message = err.to_string();

    assert!(
        message.contains("Gave up after") && message.contains("unsolicited"),
        "the failure must name the skip ceiling it hit. A timeout would mean the deadline stopped it, \
         and the deadline only gates socket reads — it cannot bound a loop that never reads. Got: \
         {message}"
    );
    h.stop();
}

/// A wanted reply followed by only the **prefix** of a coalesced unsolicited frame, with the suffix
/// arriving before the next real reply.
///
/// This is ordinary TCP behaviour, not an exotic peer: the daemon emits unsolicited `Status`
/// documents, coalescing them into a reply's write is already an accepted wire condition, and TCP may
/// split any write at any byte — including in the middle of the follower.
///
/// Composed from two existing `WirePolicy` knobs so the sequence is deterministic and nothing asserts
/// on elapsed time: `coalesce_extra_for_element` puts the push in the same write as every `State`
/// reply, and `Chunking::AfterMarker("<Status")` cuts that write immediately after the follower's root
/// name. `chunk_delay` only *orders* the two segments.
///
/// So the first command sees `<State …/>` plus `<?xml …?><Status`, and the follower's remainder —
/// carrying a **conflicting `volume`** — arrives afterwards, ahead of the next `State` reply.
///
/// The corruption is silent and same-element: the second command uses one connection and one attempt,
/// reports the right `state`, and takes `volume` from the push. `framing::root_element` finds the later
/// expected root, so the reply looks legitimate, while `parse_attr` falls back to scanning the whole
/// string because `root_open_tag` cannot start at an orphaned suffix — and the orphan's attribute is
/// found first.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn a_fragmented_follower_suffix_cannot_override_the_next_reply() {
    // Deliberately conflicting: the push claims -1 dB where the daemon's real level is -23.5, which
    // the client reports rounded as -24. A shared attribute name is what makes the corruption silent.
    const PUSH_WITH_CONFLICTING_VOLUME: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<Status state=\"2\" track=\"3\" position=\"43\" length=\"215\" volume=\"-1\"/>"
    );

    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            // Cut the combined write immediately after the follower's root name, so the first read
            // ends mid-follower. `split` runs after the coalesce, so this cuts the joined buffer.
            chunking: Chunking::AfterMarker("<Status".to_string()),
            coalesce_extra_for_element: Some((
                "State".to_string(),
                PUSH_WITH_CONFLICTING_VOLUME.to_string(),
            )),
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");
    // A state the push does not also claim, so a wrong `state` would be a different bug than a wrong
    // `volume`. The push says state="2"; the daemon says 1.
    h.model.external_change(|s| s.playback = 1);

    // First command: reply + follower prefix in one segment, follower suffix in the next.
    h.adapter.get_state().await.expect("first State");

    // Second command on the same connection. The orphaned suffix is ahead of its reply.
    let state = h.adapter.get_state().await.expect("second State");

    assert_eq!(
        state.volume, -24,
        "a shared attribute in the fragmented push must not override the following State reply. The \
         daemon's level is -23.5 dB, reported rounded as -24; -1 is the push's value, reached because \
         the follower's discarded prefix left its suffix to be concatenated with this reply"
    );
    assert_eq!(
        state.state, 1,
        "and the reply's own fields must still be the reply's: a wrong volume beside a right state is \
         exactly what makes this silent"
    );
    h.stop();
}

/// Same fragmentation, but the next command asks for a **different** element.
///
/// The previous case relied on the orphan and the reply sharing a root name, which is what made the
/// corruption invisible. A different-name reply is the other half: the orphan must not be mistaken for
/// this reply, and its attributes must not reach it either. `VolumeRange` shares no attribute with
/// `Status` except by accident, so the check is that the range is the daemon's own.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn a_fragmented_follower_suffix_cannot_pollute_a_different_next_reply() {
    const PUSH: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<Status state=\"2\" volume=\"-1\" min=\"-99\" max=\"99\"/>"
    );
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            chunking: Chunking::AfterMarker("<Status".to_string()),
            coalesce_extra_for_element: Some(("State".to_string(), PUSH.to_string())),
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");

    h.adapter
        .get_state()
        .await
        .expect("State leaves a fragment");
    let range = h
        .adapter
        .get_volume_range()
        .await
        .expect("VolumeRange after the fragment");

    assert_eq!(
        (range.min, range.max),
        (-60, 0),
        "the orphaned fragment carries min/max of its own; a different-element reply must still \
         report the daemon's range. Got {range:?}"
    );
    h.stop();
}

/// A **partial** follower is not a skipped document until it finishes, and the command that finishes
/// it is the one that counts it.
///
/// Attribution matters because the skip counter is what tier 1 reports as evidence about the daemon's
/// push behaviour. Counting a fragment when it is first glimpsed would double-count it; never counting
/// it would hide it.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn a_partial_follower_is_counted_by_the_command_that_completes_it() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            chunking: Chunking::AfterMarker("<Status".to_string()),
            coalesce_extra_for_element: Some(("State".to_string(), PUSH_STATUS_FRAME.to_string())),
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");

    h.adapter.get_state().await.expect("first State");
    let after_first = h.adapter.unsolicited_skipped().await;
    h.adapter.get_state().await.expect("second State");
    let after_second = h.adapter.unsolicited_skipped().await;

    assert!(
        after_second > after_first,
        "the follower that was only a prefix during the first command must be counted once it \
         completes during the second; got {after_first} then {after_second}"
    );
    h.stop();
}

/// A reply cut **inside a multi-byte code point** must arrive with the character intact.
///
/// The accumulation loop classifies only the longest valid UTF-8 prefix of its buffer, precisely so a
/// character straddling a read is excluded from that attempt rather than mangled. Nothing exercised
/// that: `Chunking::AfterMarker` cuts at a `&str` boundary and so can never land mid-character.
/// `Chunking::BytesAfterMarker` can, and this is the case where the split character's value is
/// **observable**, so the claim is about content and not merely about parseability.
///
/// The test asserts its own premise. An earlier version of this coverage claimed a multi-byte carry it
/// never performed — its cut landed after an ASCII root name — so the arithmetic here is checked rather
/// than trusted: the prefix up to the cut must be *invalid* UTF-8, which is only true if the cut fell
/// between the bytes of a character.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn a_reply_split_inside_a_multi_byte_character_arrives_intact() {
    // `☃` is three bytes (E2 98 83). Cutting one byte past the opening quote leaves a lone E2 at the
    // end of the first read: a valid prefix boundary excludes it, and the next read completes it.
    const SNOWMAN: &str = "☃";
    // The character must be FIRST in the value, so that one byte past the opening quote is inside it.
    // An earlier draft put it mid-string and the premise assertion below caught the arithmetic.
    let profile_name = format!("{SNOWMAN} Rock Roll");

    // The vehicle is `State.matrix_profile`. It was `MatrixGetProfile` until #347 stopped treating
    // that command as the current-profile authority, at which point the client no longer sent it on
    // this path — and a split-read claim is only worth anything on a document the client reads. The
    // claim itself is unchanged: a reply cut inside a multi-byte code point must reassemble exactly.
    // Premise check, before any wire is involved: one byte into the character is not a char boundary.
    let marker = "matrix_profile=\"";
    let doc = format!("<State mode=\"1\" matrix_profile=\"{profile_name}\"/>");
    let cut = doc.find(marker).expect("marker present") + marker.len() + 1;
    assert!(
        std::str::from_utf8(&doc.as_bytes()[..cut]).is_err(),
        "premise: the cut must fall INSIDE a multi-byte code point, or this test proves nothing about \
         split tails. Prefix was valid UTF-8 up to byte {cut}"
    );

    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            chunking: Chunking::BytesAfterMarker {
                marker: marker.to_string(),
                extra: 1,
            },
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");
    h.model
        .external_change(|st| st.matrix_profile = profile_name.clone());

    let current = h
        .adapter
        .get_state()
        .await
        .expect("a reply split mid-character must still parse")
        .matrix_profile;

    assert_eq!(
        current, profile_name,
        "the character split across two reads must be reassembled exactly; a lossy read would show \
         replacement characters here. Got {current:?}"
    );
    h.stop();
}

/// An **incomplete UTF-8 tail must survive in the carry** and complete on the next read.
///
/// The previous case splits a character inside one command. This one splits a *follower* so that the
/// incomplete sequence is what the connection carries between commands — the path the carry exists for.
/// If the carry were stored as a lossy `String` rather than raw bytes, the partial sequence would be
/// replaced before its remaining bytes arrived and could never reassemble.
///
/// Premise asserted, as above. What this proves and what it does not: the follower completes as a
/// document and is counted as skipped, and the next reply is uncorrupted. It does **not** prove the
/// follower's own content byte-exact, because a skipped document's content is never surfaced through
/// the public API — [`a_reply_split_inside_a_multi_byte_character_arrives_intact`] carries that claim.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn an_incomplete_utf8_tail_survives_in_the_carry_and_completes_on_the_next_read() {
    const SNOWMAN: &str = "☃";
    let push = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Status state=\"2\" volume=\"-1\"          track_id=\"{SNOWMAN} carried\"/>"
    );
    let marker = "track_id=\"";

    // Premise: cutting one byte into the snowman leaves an incomplete sequence.
    let cut = push.find(marker).expect("marker present") + marker.len() + 1;
    assert!(
        std::str::from_utf8(&push.as_bytes()[..cut]).is_err(),
        "premise: the cut must land inside the multi-byte character so the carry ends mid-sequence"
    );

    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            // The cut is applied to the JOINED write, so the first read holds the whole State reply
            // plus a follower prefix ending mid-character.
            chunking: Chunking::BytesAfterMarker {
                marker: marker.to_string(),
                extra: 1,
            },
            coalesce_extra_for_element: Some(("State".to_string(), push.clone())),
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");
    h.model.external_change(|st| st.playback = 1);

    let first = h
        .adapter
        .get_state()
        .await
        .expect("first State, leaving an incomplete UTF-8 tail in the carry");
    assert_eq!(
        (first.state, first.volume),
        (1, -24),
        "the FIRST reply must also be clean: the partial follower behind it, ending mid-character, \
         must not have leaked into the document returned here. Got {first:?}"
    );
    let skipped_before = h.adapter.unsolicited_skipped().await;

    let state = h
        .adapter
        .get_state()
        .await
        .expect("second State completes the carried sequence");

    assert_eq!(
        (state.state, state.volume),
        (1, -24),
        "the carried partial character must not corrupt this reply, and the follower must not supply \
         its volume. Got {state:?}"
    );
    assert!(
        h.adapter.unsolicited_skipped().await > skipped_before,
        "the follower whose first read ended mid-character must complete and be counted, which it can \
         only do if the incomplete tail was carried as raw bytes"
    );
    h.stop();
}

/// A reconnect must not inherit the previous socket's unread bytes.
///
/// The carry lives on the connection rather than the adapter precisely so this holds by construction:
/// a new socket starts with an empty carry.
///
/// Verified to **pass against the pre-fix code too**, and recorded as such rather than presented as a
/// fourth defect proof — before the carry existed there was nothing to inherit. Its value is forward:
/// it fails the moment someone hoists the field onto the adapter for convenience, which is exactly how
/// "by construction" stops being true.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn a_reconnect_starts_with_no_carried_bytes() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            chunking: Chunking::AfterMarker("<Status".to_string()),
            coalesce_extra_for_element: Some(("State".to_string(), PUSH_STATUS_FRAME.to_string())),
            disruption: Disruption::DropNextReplyOnce,
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");
    h.model.external_change(|s| s.playback = 1);

    // Leaves a follower fragment carried on this connection.
    h.adapter.get_state().await.expect("first State");
    // Now force the connection away mid-command; the retry reconnects onto a fresh socket.
    h.server.arm_disruption();
    let state = h
        .adapter
        .get_state()
        .await
        .expect("the command recovers by reconnecting");

    assert_eq!(
        (state.state, state.volume),
        (1, -24),
        "a reply read on a fresh socket must be the reply, not something assembled from the dead \
         connection's leftovers. Got {state:?}"
    );
    assert!(
        h.server.stats().connections() >= 2,
        "precondition: the disruption must actually have forced a reconnect"
    );
    h.stop();
}

/// The second layer: an attribute lookup with no identifiable root element reports **nothing** rather
/// than searching the whole string.
///
/// This is the mechanism that turned a stray fragment into silent corruption. `root_element` found the
/// expected root further along, so the reply passed every structural check, while the attribute came
/// from the orphan because a whole-string scan finds whatever appears first. The framing fix stops the
/// orphan forming; this stops an orphan mattering if one ever does.
///
/// `None` is the honest answer, and every caller already treats it as "not present" rather than
/// assuming a value. Both legitimate input shapes keep working: a returned reply is exactly one
/// document, and `parse_items` hands over one element at a time.
///
/// **Label: client-conformance.**
#[tokio::test]
async fn an_attribute_lookup_without_a_root_element_reports_nothing() {
    // A reply preceded by an orphaned fragment carrying a conflicting `volume`. Served as a raw
    // malformed body so the shape is exact rather than incidental.
    let orphan_then_reply = concat!(
        " state=\"2\" volume=\"-1\"/>\n",
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<State state=\"1\" volume=\"-23.5\" mode=\"1\"/>"
    );

    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            malformed_for_element: Some(("State".to_string(), orphan_then_reply.to_string())),
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");

    // Whatever the client concludes, it must not be the orphan's volume. Either it rejects the buffer
    // or it reads the real document — both are defensible; taking -1 is not.
    match h.adapter.get_state().await {
        Ok(state) => assert_ne!(
            state.volume, -1,
            "a leading orphan must never supply an attribute; got {state:?}"
        ),
        Err(_) => {} // Refusing the buffer outright is also correct.
    }
    h.stop();
}

// ===========================================================================================
// At-most-once semantics for one-shot commands (CodeRabbit threads 4 and 17)
//
// `Next`, `Previous`, `VolumeUp` and `VolumeDown` are relative or sequential: applying one twice is
// not the same as applying it once. The protocol carries no request identity, so a client that writes
// such a command and then loses the connection cannot learn whether it landed. Retrying is therefore a
// *choice to double the side effect* on the chance that it did not.
//
// `VolumeMute` deliberately is *not* in this family. Live validation against a real HQPlayer 6.0.2
// Embedded daemon (issue #322) showed it is an absolute mute-to-floor and idempotent - repeated calls
// keep the level at the floor and never toggle back - so a lost reply is safe to retry and converges.
// See `volume_mute_retries_and_converges_when_the_reply_is_lost_after_the_daemon_applied_it`.
//
// These use `ApplyThenDropReplyOnce`, which is indistinguishable from `DropNextReplyOnce` at the
// socket and the opposite of it at the model. The assertion is deliberately on the model's state and
// on the call's `Result` together. Both halves are the contract: exactly one side effect, and an *error*
// rather than a success the client cannot justify. Epic #311's no-false-success rule applies here as
// much as to a rejected setter — the reply was lost, so the client does not know the command landed, and
// reporting `Ok` would assert something it cannot know. Asserting only the side effect would let a future
// implementation swallow the ambiguity and return `Ok`.
//
// **What these tests do not cover.** They exercise the *reply-loss* half only: the request was fully
// written and flushed, and the reply vanished. The production guard is deliberately wider — it treats a
// one-shot as possibly-applied from the moment the write is attempted, because `write_all` can put part
// of the request on the stream before erroring and `flush` can fail after bytes have left for the peer.
// Verified: moving the flag back to after the flush leaves all four of these tests green, so they are
// insensitive to that boundary and must not be read as proving it.
//
// That half is not covered because this harness cannot schedule it. A write error needs the peer to RST
// between the adapter's two `write_all` calls; closing the server instead lets the small request into
// the kernel buffer, so the failure surfaces on the *read* as EOF — the case already covered above. The
// boundary is therefore argued from the `tokio::io` contract rather than demonstrated, and it is a strict
// widening: it can only ever refuse a retry that the narrower rule would have allowed, never permit one.
// ===========================================================================================

/// Arrange a daemon that applies the next command and then vanishes without replying.
async fn apply_then_drop_harness() -> Harness {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            disruption: Disruption::ApplyThenDropReplyOnce,
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("initial connect");
    // Arm only now, so connection setup is never the thing under test.
    h.server.arm_disruption();
    h
}

#[tokio::test]
async fn next_advances_one_track_when_the_reply_is_lost_after_the_daemon_applied_it() {
    let h = apply_then_drop_harness().await;
    let before = h.model.state().track;

    let outcome = h.adapter.next().await;

    assert!(
        outcome.is_err(),
        "the reply was lost, so the client cannot know Next landed; reporting success would assert what \
         it does not know. Got {outcome:?}"
    );
    assert_eq!(
        h.model.state().track,
        before + 1,
        "Next was applied then its reply was lost; retrying it skips a second track. Track went \
         {before} -> {}",
        h.model.state().track
    );
    h.stop();
}

#[tokio::test]
async fn volume_mute_retries_and_converges_when_the_reply_is_lost_after_the_daemon_applied_it() {
    let h = apply_then_drop_harness().await;
    let floor = h.model.state().volume_range.min_db;

    let outcome = h.adapter.volume_mute().await;

    // Unlike `Next`/`Previous`/`VolumeUp`/`VolumeDown`, VolumeMute is absolute and idempotent - verified
    // live on 6.0.2, it drives the level to the floor and repeated calls keep it there. A lost reply is
    // therefore safe to retry: the retry re-applies the same absolute mute and converges, so the call
    // succeeds rather than surfacing an error the way a genuine one-shot must. This is the point of the
    // fix - excluding an idempotent command from retry made a mute whose reply was lost fail needlessly.
    assert!(
        outcome.is_ok(),
        "VolumeMute is idempotent, so a lost reply is retried and converges to muted rather than \
         erroring; got {outcome:?}"
    );
    assert_eq!(
        h.model.state().volume_db,
        floor,
        "the retry re-applies the absolute mute-to-floor and converges to {floor}"
    );
    h.stop();
}

#[tokio::test]
async fn volume_up_steps_once_when_the_reply_is_lost_after_the_daemon_applied_it() {
    let h = apply_then_drop_harness().await;
    let before = h.model.state().volume_db;
    let step = h
        .model
        .state()
        .volume_range
        .step_db
        .expect("the verified profile publishes a volume step");

    let outcome = h.adapter.volume_up().await;

    assert!(
        outcome.is_err(),
        "a lost reply must surface as an error, not a success the client cannot justify; got {outcome:?}"
    );

    let after = h.model.state().volume_db;
    assert!(
        (after - (before + step)).abs() < f64::EPSILON,
        "VolumeUp is relative; a retry after it applied moves {step} dB twice. {before} -> {after}"
    );
    h.stop();
}

#[tokio::test]
async fn a_query_still_retries_after_a_lost_reply_because_reading_twice_is_harmless() {
    // The guard above must be scoped to commands that carry a side effect. Queries are idempotent and
    // must keep their existing reconnect-and-retry recovery, or this fix trades one defect for another.
    let h = apply_then_drop_harness().await;
    h.model.external_change(|s| s.playback = 2);

    let state = h
        .adapter
        .get_state()
        .await
        .expect("a query must still recover by reconnecting after a lost reply");

    assert_eq!(
        state.state, 2,
        "the retry must return real state, not a fabricated default"
    );
    h.stop();
}
#[tokio::test]
async fn a_follower_burst_past_the_ceiling_is_refused_like_a_leading_burst() {
    // The documented ceiling is a bound on unsolicited documents processed per command. The leading
    // path enforces it; the follower path counted without checking, so the same burst was bounded or
    // unbounded purely by whether it arrived before or after the reply. A ceiling that depends on
    // arrival order is not a ceiling.
    //
    // 300 > MAX_UNSOLICITED_BACKLOG (256), coalesced into the same write as the reply.
    //
    // Deliberately minimal documents. The ceiling counts what one command *sees*, and the client reads
    // in 8 KiB chunks, so a burst of full-size documents spills past the first read and is carried into
    // later commands a few hundred bytes at a time — never reaching the ceiling within one command. The
    // first draft of this test used a 55-byte document and passed for exactly that reason: it proved the
    // followers were drained, not that the bound was enforced.
    let one = "<Status/>\n";
    let follower = one.repeat(300);
    assert!(
        follower.len() < 8 * 1024,
        "premise: the whole burst must fit inside one 8 KiB read for the per-command ceiling to see \
         it; {} bytes",
        follower.len()
    );
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            coalesce_extra_for_element: Some(("State".to_string(), follower)),
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");

    let outcome = h.adapter.get_state().await;

    assert!(
        outcome.is_err(),
        "300 coalesced followers exceed the 256 ceiling and must be refused, not silently drained; \
         got {outcome:?}"
    );
    let message = outcome.unwrap_err().to_string();
    assert!(
        message.contains("Gave up after"),
        "the refusal must name the backlog ceiling as the reason, so the operator can tell it from a \
         timeout; got {message:?}"
    );
    h.stop();
}

// ===========================================================================================
// Evidence-quality remediations from the CodeRabbit review (threads 7, 12, 14, 15, 16)
// ===========================================================================================

#[test]
fn every_fixture_sourced_from_a_salvage_report_records_that_chain() {
    // Thread 7. Prose inside `source` is not a machine-checkable fact: one fixture said in words that
    // the report was the immediate source and still left `source_chain` unrecorded, so the auditable
    // field disagreed with the sentence beside it. The whole point of `source_chain` is that
    // second-hand evidence is visible without reading prose, so the field has to be present wherever
    // the claim is.
    let mut missing = Vec::new();
    for profile in corpus::profiles() {
        for fixture in corpus::all_in(&profile) {
            // Keyed on the report itself, not on the upstream URL. The existing citation test only
            // looks at fixtures naming a `github.com/ohshitgorillas` URL, which is precisely how the
            // device-specific `modes.xml` escaped it: it cites a salvage report and an upstream *path*
            // at a dev ref, with no URL to trip the other check.
            let names_a_report = fixture.provenance.source.contains("SALVAGE")
                || fixture.provenance.source.contains("salvage report");
            if names_a_report && !fixture.provenance.source_chain.contains("read-via-report") {
                missing.push(format!(
                    "{}/{} (source_chain = {:?})",
                    profile, fixture.name, fixture.provenance.source_chain
                ));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "these fixtures cite a salvage report but do not record source_chain: read-via-report: {missing:#?}"
    );
}

#[test]
#[should_panic(expected = "source_loads_chain requires configured [source] mode")]
fn loading_a_chain_outside_source_mode_is_rejected_as_harness_misuse() {
    // Thread 12. `[source]` is the only configured mode whose loaded chain the source decides; in PCM
    // or SDM the configured mode *is* the chain. Allowing the pair to disagree lets the model serve SDM
    // enumerations while reporting configured PCM — a state no daemon produces, so a test that relies
    // on it proves nothing about the client. `armed_upstream_claims` does not flag it, so the model has
    // to refuse it at the setter.
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    model.external_change(|s| s.mode_index = 1);
    model.source_loads_chain(LoadedChain::Sdm);
}

#[test]
fn a_hidden_field_is_dropped_whatever_the_case_of_its_type_attribute() {
    // Thread 14. Attribute *keys* are lowercased by the projection, values are not, so `type="Hidden"`
    // slipped past the hidden-field drop and the field name was recorded. `raw::is_hostile_tag` already
    // compares this value case-insensitively; two spellings of the same rule in one crate is how a
    // privacy guarantee ends up depending on the daemon's capitalisation.
    let page = concat!(
        "<html><body><form>",
        "<input type=\"Hidden\" name=\"advanced_layout\" value=\"x\"/>",
        "<input type=\"HIDDEN\" name=\"pipeline_hint\" value=\"y\"/>",
        "<input type=\"text\" name=\"profile_name\" value=\"z\"/>",
        "</form></body></html>"
    );

    // Premise. The first draft of this test used `csrf_token` and `session_key`, which the sensitive-name
    // filter drops on its own — so it passed while the case-sensitivity it names went untested. These
    // names must be non-sensitive for the assertion below to mean anything.
    assert!(
        !mock_servers::hqplayer::raw::is_sensitive("advanced_layout")
            && !mock_servers::hqplayer::raw::is_sensitive("pipeline_hint"),
        "these field names must not be independently droppable, or this test proves nothing"
    );

    let obs = tier1::project_config_form(page);

    assert!(
        !obs.field_names.contains("advanced_layout") && !obs.field_names.contains("pipeline_hint"),
        "hidden fields must be dropped regardless of the case of `type`; recorded {:?}",
        obs.field_names
    );
    assert!(
        obs.field_names.contains("profile_name"),
        "visible fields must still be recorded; got {:?}",
        obs.field_names
    );
}

#[test]
fn a_credentialed_run_that_never_read_the_config_form_is_unverified_not_accepted() {
    // Thread 15. Both `/config` reads only `warn!` on failure, so a credentialed run that observed
    // nothing left `config_form` and `config_profiles` both `None` — indistinguishable, to the differ,
    // from having no credentials at all. The first branch then marked the claim *checked*, which is the
    // one thing `Report::checked` exists to prevent: it must never be able to mean "never looked".
    let mut capture = tier1::Capture {
        has_web_credentials: true,
        ..Default::default()
    };
    capture.config_form = None;
    capture.config_profiles = None;

    let report = tier1::diff(&capture, VERIFIED_PROFILE);

    assert!(
        !report.checked.contains("config_form"),
        "a run that observed nothing must not record config_form as checked"
    );
    assert!(
        report.unverified.iter().any(|u| u.contains("config_form")),
        "a credentialed run that failed both reads must leave config_form unverified; unverified = {:?}",
        report.unverified
    );
}

#[test]
fn corpus_attribute_reads_accept_every_spelling_the_daemon_may_use() {
    // Thread 16. `raw.rs` records hand-rolled attribute scanning as the cause of a silent
    // under-observation and replaced it with quick-xml for exactly this reason; the differ then
    // reintroduced the same `find(" key=\"")` pattern. It fails *quietly* — the comparison vanishes
    // rather than erroring — which is the failure mode that makes a clean report meaningless.
    let single_quoted = "<GetInfo name='HQPlayer' version='6.0.4'/>";
    let spaced = "<GetInfo name = \"HQPlayer\" version=\"6.0.4\"/>";

    assert_eq!(
        tier1::attr_of(single_quoted, "name").as_deref(),
        Some("HQPlayer"),
        "single-quoted attribute values are legal XML and the daemon may emit them"
    );
    assert_eq!(
        tier1::attr_of(spaced, "name").as_deref(),
        Some("HQPlayer"),
        "whitespace around `=` is legal XML"
    );

    // The declaration must not shadow the root element. This is the same mistake the adapter's own
    // `parse_attr` made before this PR — the declaration's `version="1.0"` was returned for a request
    // for `version` — so the replacement is pinned against reintroducing it on the corpus side.
    let declared = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><GetInfo version=\"6.0.4\"/>";
    assert_eq!(
        tier1::attr_of(declared, "version").as_deref(),
        Some("6.0.4"),
        "the XML declaration must not supply the root element's attribute"
    );

    // Corpus documents are stored with a provenance comment; `corpus::document` strips it, but the
    // reader must not depend on that having happened.
    let commented = "<!-- provenance: x -->\n<GetInfo version=\"6.0.4\"/>";
    assert_eq!(
        tier1::attr_of(commented, "version").as_deref(),
        Some("6.0.4"),
        "a leading comment must not stop the root element being read"
    );
}

/// Resolve one optional port from the environment.
///
/// Extracted so the tier-1 gate's configuration rules are testable without a daemon. Silently
/// defaulting a *malformed explicit* value is the dangerous case: `PORT=4321x` becomes 4321, so the
/// gate verifies a different service than the operator named and reports a clean pass for it. Absent
/// means "use the default"; present-but-unparseable means the operator made a mistake and must hear
/// about it.
fn tier1_port(var: &str, default: u16) -> Result<u16, String> {
    match std::env::var(var) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(format!("read {var}: {e}")),
        Ok(raw) => raw
            .parse()
            .map_err(|_| format!("{var} must be a u16 port, got {raw:?}")),
    }
}

/// Require an operator-supplied hardware marker when any fixture in a tier-1 profile is
/// hardware-dependent.
///
/// The profile selects expected evidence; it cannot attest to what is physically attached. Keeping
/// the requirement in fixture provenance makes the refusal travel with the claim instead of relying
/// on a runbook warning that the gate cannot enforce.
fn tier1_hardware_marker(profile: &str, supplied: Option<&str>) -> Result<(), String> {
    let mut required: Vec<String> = corpus::all_in(profile)
        .into_iter()
        .map(|fixture| fixture.provenance.hardware)
        .filter(|marker| !marker.is_empty())
        .collect();
    required.sort();
    required.dedup();

    if required.is_empty() {
        return Ok(());
    }
    if required.len() > 1 {
        return Err(format!(
            "tier-1 profile `{profile}` carries conflicting hardware requirements: {required:?}"
        ));
    }

    let expected = &required[0];
    match supplied {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(format!(
            "tier-1 profile `{profile}` requires hardware `{expected}`, but \
             UHC_HQP_CONFORMANCE_HARDWARE was `{actual}`"
        )),
        None => Err(format!(
            "tier-1 profile `{profile}` requires hardware `{expected}`; set \
             UHC_HQP_CONFORMANCE_HARDWARE={expected} only after confirming the attached device"
        )),
    }
}

/// Both web credentials or neither.
///
/// One-sided credentials are accepted by `configure` and then fail at request time, which the capture
/// records as "the read side was not observed" — indistinguishable from the declared no-credentials
/// limit. A typo in the password variable name would therefore make the `/config` lane look
/// legitimately uncaptured instead of misconfigured.
fn tier1_credentials(
    user: Option<String>,
    pass: Option<String>,
) -> Result<(Option<String>, Option<String>), String> {
    match (user, pass) {
        (None, None) => Ok((None, None)),
        (Some(u), Some(p)) => Ok((Some(u), Some(p))),
        (Some(_), None) => Err("UHC_HQP_CONFORMANCE_WEB_USER is set without _PASS".to_string()),
        (None, Some(_)) => Err("UHC_HQP_CONFORMANCE_WEB_PASS is set without _USER".to_string()),
    }
}

#[test]
fn a_malformed_explicit_port_is_refused_rather_than_silently_defaulted() {
    // Thread 9. Uses a variable name no other test reads, so this cannot race the live gate.
    let var = "UHC_HQP_CONFORMANCE_PORT_MALFORMED_PROBE";
    std::env::set_var(var, "4321x");
    let outcome = tier1_port(var, 4321);
    std::env::remove_var(var);

    assert!(
        outcome.is_err(),
        "an explicit unparseable port must be refused, not turned into the default; got {outcome:?}"
    );
}

#[test]
fn an_absent_port_still_takes_the_documented_default() {
    // The other half of the rule: hardening must not break the hermetic default.
    let outcome = tier1_port("UHC_HQP_CONFORMANCE_PORT_ABSENT_PROBE", 4321);
    assert_eq!(outcome, Ok(4321));
}

#[test]
fn one_sided_web_credentials_are_refused() {
    // Thread 9, second half.
    assert!(
        tier1_credentials(Some("hqp".into()), None).is_err(),
        "a user without a password must be refused"
    );
    assert!(
        tier1_credentials(None, Some("secret".into())).is_err(),
        "a password without a user must be refused"
    );
    assert!(
        tier1_credentials(None, None).is_ok(),
        "neither is the documented no-credentials lane"
    );
    assert!(
        tier1_credentials(Some("hqp".into()), Some("secret".into())).is_ok(),
        "both is the credentialed lane"
    );
}

#[tokio::test]
async fn the_fake_config_web_routes_a_request_head_split_across_reads() {
    // Thread 10. TCP does not promise that one `read` yields the whole request line. The fake server
    // routed on a single read, so a `/config/profile/load` request split mid-path was served the
    // `/config` page instead — a latent nondeterminism in the tests that read this lane rather than an
    // observed flake, because loopback usually delivers a small head in one segment. Written here as a
    // deliberate split so the guarantee is proven rather than assumed.
    let web = FakeConfigWeb::start("CONFIG-PAGE-BODY", "PROFILE-PAGE-BODY").await;

    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", web.port))
        .await
        .expect("connect to the fake config web");
    {
        use tokio::io::AsyncWriteExt;
        // Split *inside* the path, after a prefix that is itself a legal route.
        sock.write_all(b"GET /config/profile/lo")
            .await
            .expect("first half");
        sock.flush().await.expect("flush first half");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        sock.write_all(b"ad HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("second half");
        sock.flush().await.expect("flush second half");
    }

    let mut response = String::new();
    {
        use tokio::io::AsyncReadExt;
        // Tolerant on purpose. The pre-fix server answers the *truncated* head immediately and closes,
        // so the second half of the request meets a closed socket and the read can end in a reset. That
        // reset is a symptom, not the finding; accumulating whatever arrived keeps the assertion below —
        // which body was served — as the thing that reports the defect.
        let mut buf = [0u8; 4096];
        while let Ok(n) = sock.read(&mut buf).await {
            if n == 0 {
                break;
            }
            response.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }

    assert!(
        response.contains("PROFILE-PAGE-BODY"),
        "a split request head must still route to /config/profile/load; served {response:?}"
    );
}

// =============================================================================
// CodeRabbit review 4816484338 — harness and client defects it caught
//
// Every expectation below was RED against the unmodified code at head ea85fed. The client defect
// (`root_open_tag`'s single-quote blindness) is pinned in `src/adapters/hqplayer.rs`'s own
// `parse_attr_scope_tests`, where the mechanism is reachable; the rest are **harness** defects, so
// they carry the `model-fidelity` label — a harness that under-observes manufactures false absences,
// which is the failure class this whole boundary exists to prevent.
// =============================================================================

/// `Debug` for `Faults` must name every armed misbehaviour, `source_refuses_rate_pin` included.
///
/// Faults are printed in assertion messages precisely so a failure says which misbehaviour was armed.
/// A flag missing from that output makes the mode-conditional `SetRate` refusal invisible in exactly
/// the message a maintainer reads when the test it governs fails.
///
/// **Label: model-fidelity.**
#[test]
fn the_debug_output_for_faults_names_the_rate_pin_refusal() {
    use mock_servers::hqplayer::model::Faults;

    // Built by mutation rather than struct-update syntax: `Faults` keeps its `pending` queue private,
    // so `..Default::default()` is not available from outside the model module. Clippy's
    // `field_reassign_with_default` currently stays silent for exactly that reason (its suggested
    // rewrite would not compile), but the allow makes the intent explicit and survives a future
    // clippy that stops making the exception.
    #[allow(clippy::field_reassign_with_default)]
    let mut armed = Faults::default();
    armed.source_refuses_rate_pin = true;
    let shown = format!("{armed:?}");
    assert!(
        shown.contains("source_refuses_rate_pin"),
        "an armed fault that Debug does not name cannot be diagnosed from a failure message; got \
         {shown}"
    );
    assert!(
        shown.contains("true"),
        "the flag's value must be shown, not just its name; got {shown}"
    );

    // The negative side: an unarmed fault must still report itself as unarmed rather than vanish, so
    // "not printed" can never be mistaken for "not armed".
    let idle = format!("{:?}", Faults::default());
    assert!(
        idle.contains("source_refuses_rate_pin: false"),
        "an unarmed flag must read as false rather than be absent; got {idle}"
    );
}

/// The sanitiser must escape attribute values, not only element text.
///
/// `render_start` re-emitted the raw wire bytes of a value between double quotes. XML permits a `"`
/// inside a single-quoted value, so `song='Gloria "G" Step'` came back out as `song="Gloria "G"
/// Step"` and the artifact no longer reparsed — the same failure
/// `the_sanitiser_emits_a_document_that_still_reparses` already guards for text. A stored artifact
/// that cannot be reparsed is not evidence.
///
/// **Label: model-fidelity.**
#[test]
fn the_sanitiser_escapes_attribute_values_so_the_artifact_still_reparses() {
    use mock_servers::hqplayer::raw::sanitize;

    // A double quote inside a single-quoted value, plus markup characters that must survive as data.
    let doc = "<Status active_filter='say \"hi\" &amp; go' note='a &lt; b'/>";
    let clean = sanitize(doc);

    let mut reader = quick_xml::Reader::from_str(&clean);
    let mut values: Vec<(String, String)> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                for a in e.attributes() {
                    let a = a.unwrap_or_else(|e| {
                        panic!("sanitised attribute no longer parses: {e}\noutput was: {clean}")
                    });
                    values.push((
                        String::from_utf8_lossy(a.key.as_ref()).into_owned(),
                        a.unescape_value()
                            .unwrap_or_else(|e| {
                                panic!("sanitised value no longer unescapes: {e}\nfrom: {clean}")
                            })
                            .into_owned(),
                    ));
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(e) => panic!("sanitised output no longer reparses: {e}\noutput was: {clean}"),
        }
    }

    assert_eq!(
        values,
        vec![
            ("active_filter".to_string(), "say \"hi\" & go".to_string()),
            ("note".to_string(), "a < b".to_string()),
        ],
        "attribute values must survive the round trip with their meaning intact; got {clean:?}"
    );
}

/// Sanitised artifacts preserve XML meaning, not byte-for-byte entity spelling.
///
/// The sanitiser already normalizes quoting, whitespace, declarations, and element text while
/// removing secrets. Attribute values follow the same contract: a numeric character reference may
/// be emitted as a named entity, but reparsing must recover the exact semantic value. Recording this
/// explicitly prevents a normalized artifact from later being cited as a raw-wire byte capture.
///
/// **Label: model-fidelity.**
#[test]
fn the_sanitiser_normalizes_entity_spelling_but_preserves_attribute_meaning() {
    use mock_servers::hqplayer::raw::sanitize;

    let clean = sanitize("<Status song='Gloria&#39;s Step'/>");
    assert!(
        !clean.contains("&#39;"),
        "the stored artifact is deliberately normalized rather than byte-faithful: {clean}"
    );

    let mut reader = quick_xml::Reader::from_str(&clean);
    let value = loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Empty(e)) => {
                break e
                    .attributes()
                    .filter_map(Result::ok)
                    .find(|a| a.key.as_ref() == b"song")
                    .expect("song attribute")
                    .unescape_value()
                    .expect("normalized value reparses")
                    .into_owned();
            }
            Ok(quick_xml::events::Event::Eof) => panic!("no Status element in {clean}"),
            Ok(_) => {}
            Err(e) => panic!("normalized artifact does not reparse: {e}; {clean}"),
        }
    };
    assert_eq!(value, "Gloria's Step");
}

/// Redaction must not be weakened by the escaping fix: a sensitive *value* is still dropped.
///
/// A guard, not a red. The escape has to happen **after** the sensitivity check, or decoding a value
/// in order to escape it becomes a way for a credential to come back out re-encoded. A sensitive
/// attribute *key* is not the case to use here: `is_hostile_tag` drops that element whole before
/// `render_start` is ever reached, so it would prove nothing about this path. A harmless key with a
/// sensitive value is the shape that exercises it.
///
/// **Label: model-fidelity.**
#[test]
fn escaping_attribute_values_does_not_weaken_redaction() {
    use mock_servers::hqplayer::raw::sanitize;

    // `note` is not a sensitive name, so the element survives; its value contains `password`, so the
    // attribute must not. The `&quot;` is there so the value only *looks* clean before decoding.
    let clean = sanitize("<Status note='my &quot;password&quot; is hunter2' keep='fine'/>");
    assert!(
        !clean.contains("hunter2") && !clean.contains("password"),
        "an attribute whose value is sensitive must be dropped, decoded or not; got {clean}"
    );
    assert!(
        clean.contains("keep=\"fine\""),
        "a harmless attribute alongside it must still be kept; got {clean}"
    );
}

/// `has_child` must answer about a **direct child** of the root, not any descendant.
///
/// The `status_metadata_child` evidence claim rests on this being a structural child fact: ADR 003
/// asks whether `Status` carries a `metadata` child, and a `metadata` nested three levels down inside
/// something else is a different fact. Answering "yes" for a descendant is a false *presence*, the
/// mirror of the false absences this lane exists to avoid.
///
/// **Label: model-fidelity.**
#[test]
fn has_child_distinguishes_a_direct_child_from_a_deeper_descendant() {
    use mock_servers::hqplayer::raw::has_child;

    // The real shape: a self-closing `metadata` directly under the root.
    assert!(
        has_child(
            "<Status state=\"2\"><metadata artist=\"Bill Evans\"/></Status>",
            "metadata"
        ),
        "a self-closing direct child must be found"
    );
    // Non-self-closing direct child, for the same reason.
    assert!(
        has_child("<Status><metadata></metadata></Status>", "metadata"),
        "a direct child with an end tag must be found too"
    );

    // The defect: nested deeper, so it is not the structural fact the claim names.
    assert!(
        !has_child(
            "<Status><wrapper><metadata artist=\"Bill Evans\"/></wrapper></Status>",
            "metadata"
        ),
        "a grandchild must not satisfy a direct-child claim"
    );
    // The root itself is not its own child.
    assert!(
        !has_child("<metadata/>", "metadata"),
        "the root element must not count as its own child"
    );
    // Absent means absent.
    assert!(
        !has_child("<Status state=\"2\"/>", "metadata"),
        "an absent child must read as absent"
    );
}

/// An `<option>` with no text node must still be recorded as a named profile.
///
/// `Start` and `Empty` shared one arm, so `<option value="Speakers"/>` parked the value in
/// `pending_option` and nothing ever consumed it — the next option overwrote it and the profile
/// silently disappeared. The tier-1 differ then reports a corpus-only profile that the daemon
/// actually offered: a false absence attributed to the daemon.
///
/// **Label: model-fidelity.**
#[test]
fn a_self_closing_profile_option_is_still_a_named_profile() {
    let page = concat!(
        "<html><body><form>",
        "<select name=\"profile\">",
        "<option value=\"[default]\"/>",
        "<option value=\"Speakers\"/>",
        "<option value=\"Headphones\">Cans</option>",
        "<option value=\"Night\"></option>",
        "</select>",
        "</form></body></html>"
    );

    let obs = tier1::project_config_form(page);

    assert!(
        obs.offers_default,
        "the `[default]` base must still be recognised"
    );
    assert_eq!(
        obs.named_profiles,
        vec![
            ("Speakers".to_string(), "Speakers".to_string()),
            ("Headphones".to_string(), "Cans".to_string()),
            ("Night".to_string(), "Night".to_string()),
        ],
        "every named option must be recorded; a self-closing or text-less option falls back to its \
         value as its label"
    );
}

/// An option outside the profile select must still be ignored — the fix must not widen what is kept.
///
/// The projection is an allowlist: element text is kept *only* as an option label inside the profile
/// select. Splitting the `Start`/`Empty` arms must not turn that into "any option anywhere".
///
/// **Label: model-fidelity.**
#[test]
fn a_self_closing_option_outside_the_profile_select_is_still_ignored() {
    let page = concat!(
        "<html><body><form>",
        "<select name=\"filter\"><option value=\"poly-sinc\"/></select>",
        "<select name=\"profile\"><option value=\"Speakers\"/></select>",
        "</form></body></html>"
    );

    let obs = tier1::project_config_form(page);
    assert_eq!(
        obs.named_profiles,
        vec![("Speakers".to_string(), "Speakers".to_string())],
        "only options inside the `profile` select are profiles"
    );
}

/// A coalesced push frame must not bleed into the raw lane's returned document, and a reply behind one
/// must not be thrown away.
///
/// The lane accumulated by line and returned the whole buffer once anything in it parsed. A daemon
/// that puts an unsolicited `Status` push and the real reply on one line therefore produced two
/// failures at once: when the first root matched, the returned "document" carried a second document
/// glued to its tail; when it did not, `document.clear()` discarded the reply that had already
/// arrived and the lane waited out its whole deadline for bytes it was holding.
///
/// `framing::first_document_span` exists for exactly this and was already used by the client.
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn the_raw_lane_takes_one_frame_and_keeps_what_was_coalesced_behind_it() {
    use mock_servers::hqplayer::raw::{observe, Query};

    // A push frame first, then the reply the lane asked for, both on a single line — one `read_line`
    // yields both.
    let coalesced = concat!(
        "<Status state=\"2\" track=\"3\"/>",
        "<State state=\"1\" volume=\"-23.5\" mode=\"1\"/>\n"
    );
    let port = serve_one_canned_line(coalesced).await;

    let document = observe("127.0.0.1", port, Query::State, Duration::from_secs(2))
        .await
        .expect("the reply is present in the buffer, so the lane must find it");

    assert_eq!(
        framing::root_element(&document).as_deref(),
        Some("State"),
        "the lane must return the reply it asked for, not the push frame; got {document:?}"
    );
    assert!(
        !document.contains("<Status"),
        "the push frame must not be glued to the returned document; got {document:?}"
    );
    assert_eq!(
        framing::classify(&document),
        framing::Framing::Complete,
        "what is returned must be exactly one complete document; got {document:?}"
    );
    // The whole point: attribute reads scope to the root open tag, so a returned buffer holding two
    // documents is a buffer whose attributes cannot be trusted.
    assert_eq!(
        mock_servers::hqplayer::raw::root_attrs(&document)
            .into_iter()
            .find(|(k, _)| k == "volume")
            .map(|(_, v)| v)
            .as_deref(),
        Some("-23.5"),
        "the reply's own attributes must be readable off what is returned"
    );
}

/// The other half: the wanted frame arriving **first**, with a push frame behind it. What is returned
/// must still be one document, and the trailing push must not appear in it.
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn the_raw_lane_does_not_return_a_push_frame_glued_behind_the_reply() {
    use mock_servers::hqplayer::raw::{observe, Query};

    let coalesced = concat!(
        "<State state=\"1\" volume=\"-11.5\" mode=\"1\"/>",
        "<Status state=\"2\" track=\"3\"/>\n"
    );
    let port = serve_one_canned_line(coalesced).await;

    let document = observe("127.0.0.1", port, Query::State, Duration::from_secs(2))
        .await
        .expect("the reply is the first frame, so the lane must return it");

    assert!(
        !document.contains("<Status"),
        "a trailing push frame must be excluded from the returned document; got {document:?}"
    );
    assert_eq!(
        framing::classify(&document),
        framing::Framing::Complete,
        "exactly one document; got {document:?}"
    );
}

/// A push frame on its own line leaves only whitespace in the carry buffer. That tail is incomplete,
/// not malformed, so the raw lane must continue to the reply on the following line.
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn the_raw_lane_continues_after_a_push_frame_on_its_own_line() {
    use mock_servers::hqplayer::raw::{observe, Query};

    let port = serve_one_canned_line(concat!(
        "<Status state=\"2\" track=\"3\"/>\n",
        "<State state=\"1\" volume=\"-9.5\" mode=\"1\"/>\n"
    ))
    .await;

    let document = observe("127.0.0.1", port, Query::State, Duration::from_secs(2))
        .await
        .expect("a whitespace-only carry after a push frame must not block the next line");
    assert_eq!(framing::root_element(&document).as_deref(), Some("State"));
    assert!(document.contains("volume=\"-9.5\""));
}

/// A single well-formed reply must be unaffected — the ordinary path is the one that must not regress.
///
/// **Label: model-fidelity.**
#[tokio::test]
async fn the_raw_lane_still_reads_an_ordinary_single_reply() {
    use mock_servers::hqplayer::raw::{observe, Query};

    let port = serve_one_canned_line("<State state=\"1\" volume=\"-7.5\" mode=\"1\"/>\n").await;
    let document = observe("127.0.0.1", port, Query::State, Duration::from_secs(2))
        .await
        .expect("an ordinary reply must still be read");
    assert_eq!(framing::root_element(&document).as_deref(), Some("State"));
    assert!(document.contains("volume=\"-7.5\""));
}

/// Serve one canned line to the first connection, then hold the socket open.
///
/// Held open rather than closed so a lane that discards the reply reports a **timeout** — its actual
/// failure — instead of "connection closed mid-document", which would pass the test for the wrong
/// reason by reporting an error either way.
async fn serve_one_canned_line(line: &str) -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind canned responder");
    let port = listener.local_addr().expect("local addr").port();
    let line = line.to_string();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            // Read the request so the lane's write completes, then answer once.
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(line.as_bytes()).await;
            let _ = sock.flush().await;
            // Hold the connection open.
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    port
}

// =============================================================================
// Issue #347 — trustworthy live setters, and enumerations scoped to the LOADED chain
//
// #322 made these situations expressible; every client-side consequence is owned here. Each test
// below is labelled the way #322 labelled its own: **client-red** for an expectation that fails
// against the pre-#347 adapter, **client-pin** for a property that already holds and is pinned so it
// cannot regress. No test asserts elapsed wall-clock time and none contacts a real daemon.
// =============================================================================

// -----------------------------------------------------------------------------
// The `[source]` rate pin — suppressed with a stated reason, never sent
// -----------------------------------------------------------------------------

/// Under a configured `[source]` mode the daemon answers `SetRate` with `OK` and applies nothing
/// (HQP-C-018). Retrying cannot help — what governs the rate there is a persistent config limit with
/// no wire command — so the honest client behaviour is to **not send it** and say why.
///
/// Before #347 the client sent the pin and let readback arithmetic produce a failure whose message
/// named an index, not a reason.
///
/// **Label: client-red.** **Provenance of the daemon behaviour: derived-upstream, tier-2-only,
/// pending #332.**
#[tokio::test]
async fn a_rate_pin_under_source_is_suppressed_before_it_reaches_the_daemon() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.mode_index = 0; // configured `[source]`
        s.playback = 2;
    });
    h.model.arm(|f| f.source_refuses_rate_pin = true);

    let outcome = h.adapter.set_rate(705600).await;

    assert_eq!(
        h.model.request_count("SetRate"),
        0,
        "a write the daemon is known to ignore in this mode must not be put on the wire at all; \
         sending it and failing on readback is a true outcome reached for no stated reason"
    );
    let err = outcome
        .and_then(SettingOutcome::into_applied_result)
        .expect_err("an unsent write must never be reported as applied");
    let message = err.to_string();
    assert!(
        message.contains("[source]"),
        "the refusal must name the configured mode that causes it, so an operator can act on it \
         rather than reading an index comparison. Got: {message}"
    );
    h.stop();
}

/// The narrow case readback **cannot** catch: Auto (index 0) requested under `[source]`, where the
/// rate is already 0. Expected 0 against observed 0 is a match, so the pre-#347 client reported
/// success for a command that did nothing (HQP-C-019).
///
/// **Label: client-red.**
#[tokio::test]
async fn an_auto_rate_request_under_source_is_never_reported_as_applied() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.mode_index = 0;
        s.playback = 2;
    });
    h.model.arm(|f| f.source_refuses_rate_pin = true);
    assert_eq!(
        h.model.state().rate_index,
        0,
        "precondition: the rate must already be unpinned, which is what makes readback blind"
    );

    let outcome = h.adapter.set_rate(0).await;

    assert_eq!(
        h.model.request_count("SetRate"),
        0,
        "Auto is suppressed under `[source]` exactly like any other pin"
    );
    assert!(
        outcome
            .and_then(SettingOutcome::into_applied_result)
            .is_err(),
        "equality with pre-existing state is not proof that a setter applied; before #347 this \
         reported success because it compared 0 against 0"
    );
    h.stop();
}

/// A rate pin in a configured PCM or SDM mode is ordinary and must still work — the suppression is
/// **mode-conditional**, not a blanket refusal to pin rates.
///
/// **Label: client-pin.**
#[tokio::test]
async fn a_rate_pin_outside_source_is_still_sent_and_verified() {
    let h = Harness::verified().await;
    h.adapter
        .set_rate(44100)
        .await
        .applied()
        .expect("a PCM rate pin");
    assert_eq!(
        h.model.state().rate_index,
        1,
        "the PCM list gives 44100 index 1 and the daemon must actually have moved there"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// Mode writes — skipped when they are no-ops, honest about the pin when performed
// -----------------------------------------------------------------------------

/// `SetMode` clears the exact-rate pin even when the mode does not change (HQP-C-017), so writing
/// the mode a daemon is already in destroys user state for nothing.
///
/// **Label: client-red.** Before #347 the client sent it unconditionally.
#[tokio::test]
async fn a_no_op_mode_write_is_not_sent_so_the_rate_pin_survives() {
    let h = Harness::verified().await;
    h.adapter
        .set_rate(44100)
        .await
        .applied()
        .expect("pin a PCM rate");
    let pinned = h.model.state();
    assert_ne!(pinned.rate_index, 0, "precondition: the pin must be set");

    // The daemon is already in PCM.
    h.adapter
        .set_mode("PCM")
        .await
        .applied()
        .expect("a mode that is already running is trivially satisfied");

    assert_eq!(
        h.model.request_count("SetMode"),
        0,
        "no `SetMode` may reach a daemon already in that mode: the write is not a no-op on the \
         daemon, it clears the rate pin"
    );
    assert_eq!(
        h.model.state().rate_index,
        pinned.rate_index,
        "and the user's pin must survive"
    );
    h.stop();
}

/// A mode write that really changes the mode is sent, and the pin it clears is reported as cleared
/// rather than remembered.
///
/// **Label: client-pin** for the pipeline value — #322's `refresh_lists` already re-read it — and
/// **client-red** for the untouched-pin half, which only holds once the no-op skip exists.
#[tokio::test]
async fn a_performed_mode_write_reports_the_cleared_rate_pin_honestly() {
    let h = Harness::verified().await;
    h.adapter
        .set_rate(44100)
        .await
        .applied()
        .expect("pin a PCM rate");

    h.adapter
        .set_mode("SDM (DSD)")
        .await
        .applied()
        .expect("a real mode change");

    assert_eq!(
        h.model.request_count("SetMode"),
        1,
        "a mode change that is not a no-op must be sent exactly once"
    );
    assert_eq!(
        h.model.state().rate_index,
        0,
        "the daemon clears the pin on a mode change"
    );
    let pipeline = h.adapter.get_pipeline_status().await.expect("pipeline");
    assert_eq!(
        pipeline.settings.samplerate.selected.value.as_str(),
        "0",
        "the client must report the rate the daemon now holds — Auto — and never the rate the user \
         pinned before the chain reloaded"
    );
    assert!(
        pipeline
            .settings
            .samplerate
            .options
            .iter()
            .any(|o| o.value == "2822400"),
        "and the offered rates must be the newly loaded chain's, got {:?}",
        pipeline
            .settings
            .samplerate
            .options
            .iter()
            .map(|o| o.value.as_str())
            .collect::<Vec<_>>()
    );
    h.stop();
}

/// The daemon names the DSD mode `"SDM (DSD)"`, so a caller asking for `"DSD"` or `"SDM"` must reach
/// it by **semantic alias**, never by assuming a list position (HQP-C-013's device fixture is why
/// position is unsafe).
///
/// **Label: client-red.** Before #347 only an exact name matched, and a bare integer was accepted
/// instead.
#[tokio::test]
async fn a_mode_alias_resolves_through_the_daemons_own_name() {
    for requested in ["DSD", "SDM", "sdm (dsd)"] {
        let h = Harness::verified().await;
        h.adapter
            .set_mode(requested)
            .await
            .applied()
            .unwrap_or_else(|e| {
                panic!("`{requested}` must resolve to the daemon's SDM entry: {e}")
            });
        assert_eq!(
            h.model.state().mode_index,
            2,
            "`{requested}` must reach the list index the daemon gives `SDM (DSD)`, which is 2 here \
             and is not a position any client may assume"
        );
        h.stop();
    }
}

// -----------------------------------------------------------------------------
// Enumerations scoped to the LOADED chain
// -----------------------------------------------------------------------------

/// The headline hazard, end to end through the real client.
///
/// The cache is populated the ordinary way — `get_pipeline_status()`, which both
/// `GET /hqplayer/pipeline` and the MCP status tool already call. The source then moves the loaded
/// chain while configured `[source]` keeps `State.mode` at 0, so nothing prompts a mode-keyed cache
/// to re-read. A name from the **previous** chain must now fail to resolve rather than being sent as
/// a stale index that names a different filter in the chain now loaded.
///
/// The synthetic profile is what makes this a silent misselection instead of a loud rejection: every
/// index resolves in both chains and always to a different name.
///
/// **Label: client-red.** Observed pre-fix on #347 comment 5125934210: `ok=true` while the daemon
/// ended on a filter nobody asked for.
#[tokio::test]
async fn a_source_driven_chain_change_invalidates_the_cached_filter_list() {
    let h = Harness::start(
        SYNTHETIC_HAZARD_PROFILE,
        WirePolicy::default(),
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");
    h.model.external_change(|s| {
        s.mode_index = 0; // configured `[source]`
        s.playback = 2;
    });
    // The ordinary cache-populating read.
    h.adapter
        .get_pipeline_status()
        .await
        .expect("pipeline populates the list cache");

    h.model.source_loads_chain(LoadedChain::Sdm);
    let before = h.model.state();

    let outcome = h.adapter.set_filter_nx("SYN-pcm-7").await;

    assert!(
        outcome
            .and_then(SettingOutcome::into_applied_result)
            .is_err(),
        "a filter name that exists only in the chain that is no longer loaded must not resolve; \
         before #347 it resolved from the stale cache to index 7 and selected `SYN-sdm-7`"
    );
    assert_eq!(
        h.model.state().filter_nx_index,
        before.filter_nx_index,
        "and nothing may have been mutated on the way to that refusal"
    );
    h.stop();
}

/// The other half: after the same chain change, a name that **is** in the newly loaded chain
/// resolves to *that* chain's index.
///
/// **Label: client-red.** Before #347 the stale cache had no `SYN-sdm-*` entry, so the resolver fell
/// through to a fresh fetch and happened to be right — but only because the name was absent. The
/// assertion here is on the index actually sent, which is what the previous test shows the stale
/// path getting wrong.
#[tokio::test]
async fn after_a_chain_change_a_filter_resolves_against_the_newly_loaded_list() {
    let h = Harness::start(
        SYNTHETIC_HAZARD_PROFILE,
        WirePolicy::default(),
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");
    h.model.external_change(|s| {
        s.mode_index = 0;
        s.playback = 2;
    });
    h.adapter.get_pipeline_status().await.expect("pipeline");
    h.model.source_loads_chain(LoadedChain::Sdm);

    h.adapter
        .set_filter_nx("SYN-sdm-3")
        .await
        .applied()
        .expect("a name in the loaded chain must resolve");

    assert_eq!(
        request_attr(
            &h.model.last_request("SetFilter").expect("SetFilter sent"),
            "value"
        )
        .as_deref(),
        Some("3"),
        "the index sent must come from the chain the daemon has loaded now"
    );
    h.stop();
}

/// The same name occupies **different positions** in the two chains of the observed Opal corpus:
/// `poly-sinc-gauss-long` is index 7 under PCM and index 1 under SDM. That is the evidenced form of
/// the hazard, and it is the one a cache cannot survive.
///
/// **Label: client-red.**
#[tokio::test]
async fn a_name_present_in_both_chains_is_sent_with_the_loaded_chains_index() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.mode_index = 0;
        s.playback = 2;
    });
    h.adapter.get_pipeline_status().await.expect("pipeline");
    assert_eq!(
        corpus::index_of(
            &corpus::document(VERIFIED_PROFILE, "filters_pcm"),
            "FiltersItem",
            "poly-sinc-gauss-long",
        ),
        Some(7),
        "precondition: the PCM list gives this name index 7"
    );

    h.model.source_loads_chain(LoadedChain::Sdm);
    h.adapter
        .set_filter_nx("poly-sinc-gauss-long")
        .await
        .applied()
        .expect("the name is in both chains");

    assert_eq!(
        request_attr(
            &h.model.last_request("SetFilter").expect("SetFilter sent"),
            "value"
        )
        .as_deref(),
        Some("1"),
        "the SDM list gives the same name index 1; sending 7 would select a different filter, and \
         sending 7 is what the stale cache did"
    );
    h.stop();
}

/// A chain change invalidates the **shaper and rate** lists as well, not only the filter list — the
/// acceptance criterion names all three. Asserted through the read surface every client actually
/// polls, because that is where a stale list is published as truth.
///
/// **Label: client-red.** Before #347 `get_pipeline_status` refreshed only when a list was *empty*,
/// so it served the previous chain's options indefinitely.
#[tokio::test]
async fn a_chain_change_invalidates_the_shaper_and_rate_lists_the_pipeline_publishes() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.mode_index = 0;
        s.playback = 2;
    });
    let pcm = h.adapter.get_pipeline_status().await.expect("pipeline");
    assert!(
        pcm.settings.shaper.options.iter().any(|o| o.value == "NS9"),
        "precondition: the PCM chain's shapers are what is cached first"
    );

    h.model.source_loads_chain(LoadedChain::Sdm);
    let sdm = h.adapter.get_pipeline_status().await.expect("pipeline");

    let shapers: Vec<&str> = sdm
        .settings
        .shaper
        .options
        .iter()
        .map(|o| o.value.as_str())
        .collect();
    assert!(
        shapers.contains(&"ASDM7") && !shapers.contains(&"NS9"),
        "the modulators of the loaded SDM chain must replace the PCM shapers wholesale, got \
         {shapers:?}"
    );
    let rates: Vec<&str> = sdm
        .settings
        .samplerate
        .options
        .iter()
        .map(|o| o.value.as_str())
        .collect();
    assert!(
        rates.contains(&"2822400") && !rates.contains(&"44100"),
        "and so must the rates, got {rates:?}"
    );
    let filters: Vec<&str> = sdm
        .settings
        .filter1x
        .options
        .iter()
        .map(|o| o.value.as_str())
        .collect();
    assert!(
        filters.contains(&"sinc-Lh") && !filters.contains(&"IIR"),
        "and the filters, got {filters:?}"
    );
    h.stop();
}

/// A reconnect must not inherit the previous session's enumerations. The daemon may have been
/// reconfigured, restarted onto another profile, or simply moved chain while UHC was away.
///
/// **Label: client-red.** Before #347 `connect()` left the cache exactly as the dropped session had
/// left it.
#[tokio::test]
async fn a_reconnect_invalidates_the_cached_enumerations() {
    let h = Harness::start(
        SYNTHETIC_HAZARD_PROFILE,
        WirePolicy::default(),
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");
    h.model.external_change(|s| {
        s.mode_index = 0;
        s.playback = 2;
    });
    h.adapter.get_pipeline_status().await.expect("pipeline");

    h.adapter.disconnect().await;
    h.model.source_loads_chain(LoadedChain::Sdm);
    h.adapter.connect().await.expect("reconnect");

    let pipeline = h
        .adapter
        .get_pipeline_status()
        .await
        .expect("pipeline after reconnect");
    let filters: Vec<&str> = pipeline
        .settings
        .filter1x
        .options
        .iter()
        .map(|o| o.value.as_str())
        .collect();
    assert!(
        filters.iter().all(|f| f.starts_with("SYN-sdm-")),
        "a reconnected session must re-read the enumerations rather than resume the previous \
         session's, got {filters:?}"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// `SetFilter` carries both sides, or refuses without mutating
// -----------------------------------------------------------------------------

/// When `State` does not report the sibling filter, the pre-#347 client substituted the legacy
/// combined `filter` field and sent it as the sibling — a guess, and #322's audit recorded it as
/// defect C5. A guessed sibling silently overwrites the setting the user did not touch.
///
/// **Label: client-red.**
#[tokio::test]
async fn a_filter_write_is_refused_without_mutation_when_the_sibling_is_unknowable() {
    let h = Harness::verified().await;
    h.model
        .set_filter_field_reporting(FilterFieldReporting::CombinedOnly);
    let before = h.model.state();

    let outcome = h.adapter.set_filter_1x("poly-sinc-lp").await;

    assert!(
        outcome
            .and_then(SettingOutcome::into_applied_result)
            .is_err(),
        "with the Nx sibling unknowable the write must be refused, not guessed"
    );
    assert_eq!(
        h.model.request_count("SetFilter"),
        0,
        "and refused *before* anything reaches the daemon: `SetFilter` writes both sides, so a \
         guessed sibling is a silent overwrite of a setting nobody asked to change"
    );
    assert_eq!(
        (
            h.model.state().filter_1x_index,
            h.model.state().filter_nx_index
        ),
        (before.filter_1x_index, before.filter_nx_index),
        "nothing may have moved"
    );
    h.stop();
}

/// The ordinary path: a one-sided request still puts **both** arguments on the wire, taken from the
/// authoritative `State` rather than from the client's cache.
///
/// **Label: client-pin.**
#[tokio::test]
async fn a_one_sided_filter_write_carries_both_authoritative_arguments() {
    let h = Harness::verified().await;
    let before = h.model.state();
    h.adapter
        .set_filter_1x("poly-sinc-xtr")
        .await
        .applied()
        .expect("SetFilter");

    let sent = h.model.last_request("SetFilter").expect("SetFilter sent");
    assert_eq!(
        (
            request_attr(&sent, "value").as_deref(),
            request_attr(&sent, "value1x").as_deref()
        ),
        (
            Some(before.filter_nx_index.to_string().as_str()),
            Some("11")
        ),
        "both sides go on the wire together: the requested 1x index, and the Nx index the daemon \
         itself reports. Sent: {sent}"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// Semantic names only — no raw integer identity on a control path
// -----------------------------------------------------------------------------

/// A bare integer is not a setting name. The pre-#347 resolvers parsed one and used it as a **direct
/// list index**, so a client that had a stale number — or simply guessed — silently selected
/// whatever now sat at that position (HQP-C-063).
///
/// **Label: client-red.**
#[tokio::test]
async fn a_numeric_string_is_never_accepted_as_a_setting_name() {
    let h = Harness::verified().await;

    assert!(
        h.adapter
            .set_mode("1")
            .await
            .and_then(SettingOutcome::into_applied_result)
            .is_err(),
        "`1` is not a mode name"
    );
    assert!(
        h.adapter
            .set_filter_nx("7")
            .await
            .and_then(SettingOutcome::into_applied_result)
            .is_err(),
        "`7` is not a filter name"
    );
    assert!(
        h.adapter
            .set_shaper("4")
            .await
            .and_then(SettingOutcome::into_applied_result)
            .is_err(),
        "`4` is not a shaper name"
    );
    assert_eq!(
        (
            h.model.request_count("SetMode"),
            h.model.request_count("SetFilter"),
            h.model.request_count("SetShaping")
        ),
        (0, 0, 0),
        "and none of them may reach the daemon as a raw index"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// Matrix profiles — semantic identity, State as the authority
// -----------------------------------------------------------------------------

/// `MatrixGetProfile` is **not** the current-profile authority. #347 says so explicitly, and #341
/// records no versioned evidence that the supported daemon implements it consistently. The observed
/// `State.matrix_profile` field is the authority.
///
/// **Label: client-red.** Before #347 the client asked `MatrixGetProfile` and published its answer.
#[tokio::test]
async fn the_current_matrix_profile_is_read_from_state_not_matrix_get_profile() {
    let h = Harness::verified().await;
    h.model
        .arm(|f| f.matrix_current_override = Some("Speakers".to_string()));
    h.model
        .external_change(|s| s.matrix_profile = "Rock &amp; Roll".to_string());

    let current = h
        .adapter
        .get_matrix_profile()
        .await
        .expect("current profile")
        .expect("a profile is selected");

    assert_eq!(
        (current.name.as_str(), current.index),
        ("Rock & Roll", 2),
        "the authority is `State.matrix_profile`, and its index comes from resolving that name \
         against the fresh list — not from whatever `MatrixGetProfile` reports"
    );
    h.stop();
}

/// A matrix profile the daemon acknowledges but never applies is not success.
///
/// **Label: client-red.** Before #347 `set_matrix_profile` sent the name and returned `Ok` on the
/// `result="OK"` alone, with no readback at all.
#[tokio::test]
async fn a_matrix_profile_accepted_but_unchanged_is_not_reported_as_applied() {
    let h = Harness::verified().await;
    h.model
        .arm(|f| f.accept_but_ignore.push("MatrixSetProfile".to_string()));

    let outcome = h.adapter.set_matrix_profile(1).await;

    assert!(
        h.model.request_count("MatrixSetProfile") > 0,
        "the write must actually have been attempted: this is a daemon-side no-op"
    );
    assert_eq!(
        h.model.state().matrix_profile.as_str(),
        "Default",
        "precondition: the daemon really did not move"
    );
    assert!(
        outcome
            .and_then(SettingOutcome::into_applied_result)
            .is_err(),
        "`result=\"OK\"` is not proof of application for the matrix family either"
    );
    h.stop();
}

/// A current profile the daemon's own list does not contain must not be published carrying some
/// **other** profile's index. The pre-#347 read fell back to index 0, which is `Default` — so a UI
/// showed `Default` selected while the daemon was on something else entirely.
///
/// **Label: client-red.**
#[tokio::test]
async fn a_current_matrix_profile_absent_from_the_fresh_list_is_not_given_another_profiles_index() {
    let h = Harness::verified().await;
    h.model
        .arm(|f| f.matrix_current_override = Some("Ghost".to_string()));
    h.model
        .external_change(|s| s.matrix_profile = "Ghost".to_string());

    let current = h.adapter.get_matrix_profile().await.expect("current");

    assert!(
        current.is_none(),
        "a name absent from the fresh list is not a selectable identity, and index 0 belongs to \
         `Default`; got {current:?}"
    );
    h.stop();
}

/// The legacy numeric request stays a **compatibility input**: it is resolved against the fresh list
/// into a semantic name, and the name is what goes on the wire.
///
/// **Label: client-pin** for the wire form, **client-red** for the freshness — the pre-#347 code
/// fetched the list but never verified the outcome.
#[tokio::test]
async fn a_matrix_profile_write_sends_the_semantic_name_and_verifies_the_readback() {
    let h = Harness::verified().await;
    h.adapter
        .set_matrix_profile(2)
        .await
        .applied()
        .expect("a profile in the fresh list");

    assert_eq!(
        request_attr(
            &h.model
                .last_request("MatrixSetProfile")
                .expect("MatrixSetProfile sent"),
            "value"
        )
        .as_deref(),
        Some("Rock &amp; Roll"),
        "the semantic name goes on the wire, escaped as the daemon escapes it — never the number"
    );
    assert_eq!(
        h.model.state().matrix_profile.as_str(),
        "Rock &amp; Roll",
        "and the authoritative field must actually carry it"
    );
    h.stop();
}

/// An externally-made profile switch is visible on the next read, and an empty
/// `State.matrix_profile` is the default identity rather than a list position.
///
/// **Label: client-pin.**
#[tokio::test]
async fn an_external_matrix_switch_is_visible_and_an_empty_profile_is_no_selection() {
    let h = Harness::verified().await;
    h.model
        .external_change(|s| s.matrix_profile = "Speakers".to_string());
    let current = h
        .adapter
        .get_matrix_profile()
        .await
        .expect("current")
        .expect("selected");
    assert_eq!((current.index, current.name.as_str()), (1, "Speakers"));

    h.model
        .external_change(|s| s.matrix_profile = String::new());
    assert!(
        h.adapter
            .get_matrix_profile()
            .await
            .expect("current")
            .is_none(),
        "an empty profile field is the default/no-selection identity, not index 0"
    );
    h.stop();
}

/// An explicit daemon rejection of a matrix write is reported as the error it is.
///
/// **Label: client-pin.**
#[tokio::test]
async fn a_rejected_matrix_profile_write_reports_the_daemon_reason() {
    let h = Harness::verified().await;
    h.model.arm(|f| {
        f.reject_next
            .push(("MatrixSetProfile".to_string(), "profile in use".to_string()))
    });

    let err = h
        .adapter
        .set_matrix_profile(1)
        .await
        .and_then(SettingOutcome::into_applied_result)
        .expect_err("an explicit rejection is a failure");
    assert!(
        err.to_string().contains("profile in use"),
        "the daemon's own reason must survive to the caller, got: {err}"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// Bounded, non-wedging failure
// -----------------------------------------------------------------------------

/// A malformed reply fails that command and leaves the connection usable, so later polling is not
/// wedged. The oversized case is pinned by
/// `an_unbounded_reply_is_rejected_by_an_explicit_byte_cap_and_the_next_command_still_works`; this is
/// its malformed sibling.
///
/// **Label: client-pin.**
#[tokio::test]
async fn a_malformed_reply_does_not_wedge_later_polling() {
    let h = Harness::with_policy(WirePolicy {
        malformed_for_element: Some(("State".to_string(), "</Status>".to_string())),
        ..WirePolicy::default()
    })
    .await;

    assert!(
        h.adapter.get_state().await.is_err(),
        "precondition: the malformed reply must fail its own command"
    );
    let info = h
        .adapter
        .get_info()
        .await
        .expect("a later command must still be answered");
    assert_eq!(
        info.product, "Signalyst HQPlayer Embedded",
        "and it must read the daemon's real reply rather than leftovers"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// Ambiguous delivery — a lost reply is not proof the daemon did nothing
// -----------------------------------------------------------------------------

/// On HQPlayer Embedded 6.0.4 a `SetMode` was **accepted, logged and acted on** while the daemon sent
/// no response and later dropped the connection (HQP-C-029). Reporting that as a failure is as wrong
/// as reporting `OK` as success: the setting is there, and a client that says otherwise sends a user
/// chasing a change that already happened.
///
/// The drop is element-scoped because a setter is several commands now — it reads the enumeration and
/// `State` before it writes — so an unscoped "drop the next reply" would vanish during the read and
/// the write would never happen.
///
/// **Label: client-red.** Before #347 the send's error propagated and the state was never read back.
#[tokio::test]
async fn a_write_whose_reply_is_lost_after_the_daemon_applied_it_is_reported_as_applied() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            apply_then_drop_reply_for_element: Some("SetShaping".to_string()),
            ..WirePolicy::default()
        },
        HqpTimeouts {
            // One attempt, so the recovery under test is the readback rather than a resend.
            max_attempts: 1,
            ..fast_timeouts()
        },
    )
    .await;
    h.adapter.connect().await.expect("connect");

    let outcome = h
        .adapter
        .set_shaper("NS5")
        .await
        .expect("a lost reply is not a protocol error");

    assert!(
        h.server.element_drop_fired(),
        "precondition: the reply must actually have been dropped after the daemon applied it"
    );
    assert_eq!(
        h.model.state().shaper_index,
        3,
        "precondition: the daemon did apply it — NS5 is index 3 in the observed PCM list"
    );
    assert_eq!(
        outcome,
        SettingOutcome::Applied,
        "the readback finds the setting in place, so the write landed however its reply was lost"
    );
    h.stop();
}

/// The other side of the same coin. When the write draws no usable reply **and** the state does not
/// show it, the honest answer is neither success nor "it failed" — it is that delivery could not be
/// established. A caller that treats that as a clean failure will retry, and a retry of a write the
/// daemon may already hold is a different risk from a retry of one it certainly does not.
///
/// **Label: client-red.**
#[tokio::test]
async fn a_write_whose_delivery_cannot_be_established_is_reported_as_ambiguous() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            silent_for_element: Some("SetShaping".to_string()),
            ..WirePolicy::default()
        },
        HqpTimeouts {
            max_attempts: 1,
            ..fast_timeouts()
        },
    )
    .await;
    h.adapter.connect().await.expect("connect");
    let before = h.model.state().shaper_index;

    let outcome = h
        .adapter
        .set_shaper("NS5")
        .await
        .expect("ambiguity is an outcome, not a protocol error");

    assert_eq!(
        h.model.state().shaper_index,
        before,
        "precondition: the daemon never applied it"
    );
    match &outcome {
        SettingOutcome::Ambiguous { what, reason } => {
            assert_eq!(what, "shaper");
            assert!(
                reason.contains("may or may not"),
                "the reason must say what is unknown rather than assert a failure, got: {reason}"
            );
        }
        other => panic!(
            "a write whose delivery cannot be established is neither applied nor a plain failure; \
             got {other:?}"
        ),
    }
    assert!(
        outcome.into_applied_result().is_err(),
        "and no advertised surface may report it as success"
    );
    h.stop();
}

/// An explicit rejection is **not** ambiguous delivery, and the two must not be conflated: the daemon
/// saw the request and refused it, so there is nothing to read back and nothing to retry.
///
/// **Label: client-pin**, and the control for the type that separates them.
#[tokio::test]
async fn an_explicit_rejection_is_not_treated_as_ambiguous_delivery() {
    let h = Harness::verified().await;
    h.model.arm(|f| {
        f.reject_next
            .push(("SetShaping".to_string(), "invalid shaper".to_string()))
    });

    let err = h
        .adapter
        .set_shaper("NS5")
        .await
        .expect_err("an explicit result=Error stays an error, never an outcome");

    assert!(
        err.downcast_ref::<HqpRejected>().is_some(),
        "the rejection must keep its type, or telling it apart from a lost reply goes back to \
         matching on message text; got: {err}"
    );
    assert!(
        err.to_string().contains("invalid shaper"),
        "and the daemon's own reason must still reach the caller, got: {err}"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// A published enumeration set is coherent, or it is not published
// -----------------------------------------------------------------------------

/// Which chain-scoped families the pipeline view actually offers options for.
///
/// The unit of publication is the **set**: a view carrying one family and not the others has still
/// been handed to a caller as this daemon's settings, so "some but not all" is the shape both checks
/// below forbid.
fn published_families(
    pipeline: &unified_hifi_control::adapters::hqplayer::PipelineStatus,
) -> Vec<&'static str> {
    [
        ("mode", !pipeline.settings.mode.options.is_empty()),
        ("filter1x", !pipeline.settings.filter1x.options.is_empty()),
        ("shaper", !pipeline.settings.shaper.options.is_empty()),
        (
            "samplerate",
            !pipeline.settings.samplerate.options.is_empty(),
        ),
    ]
    .into_iter()
    .filter(|(_, full)| *full)
    .map(|(name, _)| name)
    .collect()
}

/// Four lists read one after another are four moments, not one. A source change landing between two
/// of them yields a set that mixes the chains — SDM filters offered next to PCM shapers — and
/// publishing that under one lock is no better than publishing it in pieces: it has still been
/// handed over as a single view of a single daemon.
///
/// The change is triggered by a request rather than by the test's own timeline, because that is the
/// only way to land it *inside* the window.
///
/// **Label: client-red.** Before the bracket, the refresh published modes and SDM filters from before
/// the change and PCM shapers and rates from after it.
#[tokio::test]
async fn a_chain_change_during_a_list_refresh_publishes_nothing_rather_than_a_mixed_set() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.mode_index = 0; // configured `[source]`
        s.playback = 2;
    });
    // Fill the cache from the PCM chain the ordinary way.
    h.adapter.get_pipeline_status().await.expect("pipeline");

    // The chain moves to SDM, which the next poll's probe will notice and act on...
    h.model.source_loads_chain(LoadedChain::Sdm);
    // ...and then moves back, mid-refresh, right after the filter list has been served. So the
    // refresh sees SDM filters and PCM shapers.
    h.model
        .switch_chain_after_request("GetFilters", LoadedChain::Pcm);

    match h.adapter.get_pipeline_status().await {
        // Refusing the straddling set means publishing none of it — and since this read needed that
        // set, it has nothing left to answer with. A view carrying one family and not the other
        // three, or all four empty, is still handed to a caller as this daemon's settings.
        Err(e) => assert!(
            e.to_string().contains("lists") || e.to_string().contains("chain"),
            "the failure must name what could not be settled. Got: {e}"
        ),
        Ok(straddled) => {
            let filters: Vec<&str> = straddled
                .settings
                .filter1x
                .options
                .iter()
                .map(|o| o.value.as_str())
                .collect();
            let shapers: Vec<&str> = straddled
                .settings
                .shaper
                .options
                .iter()
                .map(|o| o.value.as_str())
                .collect();
            panic!(
                "a read whose only enumeration set straddled a chain change must not be reported as \
                 a successful one. filters={filters:?} shapers={shapers:?} families={:?}",
                published_families(&straddled)
            );
        }
    }

    // And the client recovers: the daemon is settled on PCM now, so the next poll publishes a
    // coherent PCM set rather than staying blank.
    let settled = h.adapter.get_pipeline_status().await.expect("pipeline");
    let filters: Vec<&str> = settled
        .settings
        .filter1x
        .options
        .iter()
        .map(|o| o.value.as_str())
        .collect();
    let shapers: Vec<&str> = settled
        .settings
        .shaper
        .options
        .iter()
        .map(|o| o.value.as_str())
        .collect();
    assert!(
        filters.contains(&"IIR") && shapers.contains(&"NS9"),
        "refusing to publish a straddling set must not wedge the view: once the chain settles, the \
         next read publishes it. filters={filters:?} shapers={shapers:?}"
    );
    h.stop();
}

/// One family failing must not publish the other three, **and must not be reported as a successful
/// read either**. A refresh is a set or it is nothing: leaving a caller with fresh filters beside an
/// empty shaper list tells it the daemon offers no modulators, and answering `Ok` with *all four*
/// empty tells it the daemon offers nothing at all. Both are falsehoods a caller acts on; "not read
/// yet" is not something a `PipelineStatus` can express.
///
/// This test previously asserted `Ok` with nothing published, which pinned the second falsehood as
/// expected behaviour — the same ambiguity the bounded-retry fix rejects, reached through the lazy
/// fill instead. CodeRabbit found it.
///
/// **Label: client-red.**
#[tokio::test]
async fn a_single_failed_family_fails_the_read_rather_than_publishing_an_empty_one() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            silent_for_element: Some("GetShapers".to_string()),
            ..WirePolicy::default()
        },
        HqpTimeouts {
            max_attempts: 1,
            ..fast_timeouts()
        },
    )
    .await;
    h.adapter.connect().await.expect("connect");

    match h.adapter.get_pipeline_status().await {
        Err(e) => assert!(
            e.to_string().contains("lists"),
            "the failure must say the setting lists could not be read, rather than surfacing as \
             some unrelated error. Got: {e}"
        ),
        Ok(published) => {
            let families = published_families(&published);
            panic!(
                "a fill that never settled must not be reported as a successful read. It published \
                 {families:?}, with {} filter options and {} shaper options — a caller cannot tell \
                 that from a daemon that genuinely offers neither",
                published.settings.filter1x.options.len(),
                published.settings.shaper.options.len()
            );
        }
    }
    h.stop();
}

/// The same hole reached through the **first** read after connect, where there is no cache at all.
///
/// With nothing cached there is no previous fingerprint, so the chain check has nothing to
/// contradict and answers "unchanged" — correctly, since it is a check about *movement*. That makes
/// the required lazy fill the only thing standing between a caller and an empty answer, so its
/// failure has to be the read's failure.
///
/// **Label: client-red.**
#[tokio::test]
async fn a_first_read_whose_required_fill_never_settles_is_not_reported_as_success() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            silent_for_element: Some("GetFilters".to_string()),
            ..WirePolicy::default()
        },
        HqpTimeouts {
            max_attempts: 1,
            ..fast_timeouts()
        },
    )
    .await;
    h.adapter.connect().await.expect("connect");

    let outcome = h.adapter.get_pipeline_status().await;

    assert!(
        outcome.is_err(),
        "the very first read has no cache to fall back on, so a fill that did not settle leaves \
         nothing to publish; it published {:?} instead",
        outcome.map(|p| published_families(&p))
    );
    h.stop();
}

/// The legacy `filter` setting means *both sides*, and it must reach the daemon as **one**
/// `SetFilter`. Two one-sided writes can half-apply — the first lands, the second is rejected,
/// ignored or lost — leaving the daemon on a pair nobody asked for.
///
/// **Label: client-red.** The first form of this route wrote 1x and then Nx.
#[tokio::test]
async fn the_legacy_both_sides_filter_write_is_a_single_command() {
    let h = Harness::verified().await;
    let before = h.model.state();
    assert_eq!(
        (before.filter_1x_index, before.filter_nx_index),
        (6, 6),
        "precondition: the observed corpus starts both sides on index 6"
    );

    h.adapter
        .set_filter_pair("poly-sinc-xtr")
        .await
        .applied()
        .expect("both sides to one filter");

    assert_eq!(
        h.model.request_count("SetFilter"),
        1,
        "both sides go in one request, so the pair cannot half-apply on the wire"
    );
    let sent = h.model.last_request("SetFilter").expect("SetFilter sent");
    assert_eq!(
        (
            request_attr(&sent, "value").as_deref(),
            request_attr(&sent, "value1x").as_deref()
        ),
        (Some("11"), Some("11")),
        "and it carries the same index on both sides. Sent: {sent}"
    );
    let after = h.model.state();
    assert_eq!(
        (after.filter_1x_index, after.filter_nx_index),
        (11, 11),
        "the daemon must end with both sides on the requested filter"
    );
    h.stop();
}

/// A both-sides write the daemon acknowledges and drops is not success, and the failure names both
/// sides rather than half of them.
///
/// **Label: client-pin** for the refusal, **client-red** for the pair being one verified unit.
#[tokio::test]
async fn a_both_sides_filter_write_that_is_ignored_reports_both_sides() {
    let h = Harness::verified().await;
    h.model
        .arm(|f| f.accept_but_ignore.push("SetFilter".to_string()));

    let err = h
        .adapter
        .set_filter_pair("poly-sinc-xtr")
        .await
        .applied()
        .expect_err("an acknowledged-but-dropped pair is not applied");

    let message = err.to_string();
    assert!(
        message.contains("1x=11,Nx=11") && message.contains("1x=6,Nx=6"),
        "the error must name the pair asked for and the pair observed, so a half-applied write and \
         an ignored one are distinguishable. Got: {message}"
    );
    h.stop();
}

// -----------------------------------------------------------------------------
// The indices and the lists they are resolved against must describe the same chain
// (CodeRabbit, review of 2eb699e)
// -----------------------------------------------------------------------------

/// `State` reports settings as **list indices**, and an index is only meaningful beside the list it
/// came from. Reading `State` *before* settling the enumerations means a pre-transition index is
/// resolved against post-transition lists — the same misselection this issue exists to end, arriving
/// through the read path instead of the write path, and needing no write at all.
///
/// The Opal corpus is what makes it observable: its SDM excerpt is shorter than its PCM one, so an
/// index the daemon clamps across the transition resolves to nothing in the list that is then used.
///
/// **Label: client-red.** Before this, the published 1x filter was empty while the daemon held
/// `poly-sinc-xtr`.
#[tokio::test]
async fn a_chain_change_between_the_state_read_and_the_list_read_is_not_published() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.mode_index = 0; // configured `[source]`
        s.playback = 2;
        // Index 7 exists in the 12-entry PCM excerpt and not in the 5-entry SDM one, so the daemon
        // clamps it across the transition and the two chains disagree about what 7 means.
        s.filter_1x_index = 7;
        s.filter_nx_index = 7;
    });
    let before = h.adapter.get_pipeline_status().await.expect("pipeline");
    assert_eq!(
        before.settings.filter1x.selected.value.as_str(),
        "poly-sinc-gauss-long",
        "precondition: the PCM chain's list gives index 7 this name"
    );

    // The source moves the chain in the gap the client cannot see: after `State` has answered and
    // before the enumerations are read.
    h.model
        .switch_chain_after_request("State", LoadedChain::Sdm);

    let after = h.adapter.get_pipeline_status().await.expect("pipeline");

    let held = h.model.state().filter_1x_index;
    let truth = corpus::enum_entries(
        &corpus::document(VERIFIED_PROFILE, "filters_sdm"),
        "FiltersItem",
    )
    .into_iter()
    .find(|e| e.index == held)
    .map(|e| e.name)
    .expect("the daemon's own index resolves in the chain it now has loaded");

    assert_eq!(
        after.settings.filter1x.selected.value, truth,
        "the published selection must be what the daemon's current index names in the chain it \
         currently has loaded; an index read before the lists were settled belongs to neither"
    );
    h.stop();
}

/// The raw index form of `SetFilter` is an escape hatch, not a control path — but an escape hatch
/// that answers `Ok(())` on `result="OK"` alone is a false success with a public name on it. It goes
/// through the same verification as every other write.
///
/// **Label: client-red.** Before this it reported success for a write the daemon acknowledged and
/// dropped.
#[tokio::test]
async fn a_raw_filter_write_the_daemon_acknowledges_and_drops_is_not_reported_as_success() {
    let h = Harness::verified().await;
    h.model
        .arm(|f| f.accept_but_ignore.push("SetFilter".to_string()));
    let before = h.model.state();
    assert_ne!(
        before.filter_nx_index, 2,
        "precondition: the requested index must differ from the one held, or a readback proves \
         nothing either way"
    );

    let outcome = h.adapter.set_filter(2, 2).await;

    assert!(
        h.model.request_count("SetFilter") > 0,
        "the write must actually have been attempted: this is a daemon-side no-op"
    );
    assert_eq!(
        (
            h.model.state().filter_1x_index,
            h.model.state().filter_nx_index
        ),
        (before.filter_1x_index, before.filter_nx_index),
        "precondition: the daemon really did not move"
    );
    // Strengthened from `outcome.is_err()` after the fix landed: the RED was captured against the
    // pre-fix `Result<()>`, where an ignored write was `Ok(())`. The collapse is what every
    // advertised surface applies, so this asserts what a client would see — and additionally that
    // the outcome is `Ignored` rather than some other non-success, which the bare `is_err()` could
    // not distinguish.
    assert!(
        matches!(outcome, Ok(SettingOutcome::Ignored { .. })),
        "an acknowledged-but-dropped write is `Ignored` — not applied, and not a transport failure"
    );
    assert!(
        outcome.applied().is_err(),
        "`result=\"OK\"` is not proof of application on the raw path either — and a public method \
         that says otherwise is the false success this issue exists to remove"
    );
    h.stop();
}

/// A bounded retry has to be allowed to **fail**. When the chain moves during the state read on the
/// retry as well, the client has nothing coherent to publish: the lists it holds are from the chain
/// before that second move, and noticing the move drops them. Answering `Ok` there hands back a
/// `PipelineStatus` whose option lists are empty and whose selections resolve to nothing, presented
/// as this daemon's settings.
///
/// "I could not read this coherently" is a truthful answer and the route already renders it — the
/// pipeline handler has always had an error arm. "Here are no filters at all" is not.
///
/// **Label: client-red.** Before this, the second mismatch set the state and published anyway.
#[tokio::test]
async fn a_chain_that_moves_on_both_the_read_and_the_retry_is_reported_rather_than_published() {
    let h = Harness::verified().await;
    h.model.external_change(|s| {
        s.mode_index = 0; // configured `[source]`
        s.playback = 2;
        s.filter_1x_index = 7;
        s.filter_nx_index = 7;
    });
    h.adapter
        .get_pipeline_status()
        .await
        .expect("precondition: a settled daemon reads fine");

    // Two transitions, each landing on a `State` read: the first catches the initial read, the
    // second catches the retry. A daemon whose source is flapping does exactly this.
    h.model
        .switch_chain_after_request("State", LoadedChain::Sdm);
    h.model
        .switch_chain_after_request("State", LoadedChain::Pcm);

    let outcome = h.adapter.get_pipeline_status().await;

    match outcome {
        Err(e) => {
            let message = e.to_string();
            assert!(
                message.contains("chain"),
                "the failure must say what could not be established — that the loaded chain kept \
                 moving — rather than surfacing as some unrelated read error. Got: {message}"
            );
        }
        Ok(published) => {
            let families = published_families(&published);
            panic!(
                "a read that could not settle must not be reported as a successful one. It \
                 published {families:?}, with filter1x selected as {:?} out of {} options — a \
                 caller cannot tell that from a daemon that genuinely offers nothing",
                published.settings.filter1x.selected.value,
                published.settings.filter1x.options.len()
            );
        }
    }
    h.stop();
}

/// A profile load replaces the daemon's settings wholesale — filters, shapers and rates can all
/// change — so the enumerations cached from before it are describing a configuration that no longer
/// exists. `refresh_lists` deliberately leaves an existing cache alone when it cannot publish a
/// coherent replacement, which is right for a *transient* read and wrong here: what it preserves is
/// the pre-load lists, and the next read then finds them non-empty, skips its required fill, and
/// resolves the new profile's `State` indices through the old profile's options.
///
/// The chain check does not save it. That check compares **rates**, and a profile that changes
/// filters or shapers while leaving the rate list alone passes it — so the stale answer is not just
/// published, it is published confidently.
///
/// **Label: client-red.** Found by CodeRabbit at `d4acddf`, and it falsified a comment I had just
/// written claiming a failed post-load refresh "leaves the cache empty rather than stale".
#[tokio::test]
async fn a_profile_load_whose_refresh_fails_does_not_leave_the_previous_profiles_lists_in_place() {
    // This test is what made the shared-config coupling bite: supplying credentials leaked them to
    // every adapter built afterwards, and `tier1_checks_every_family_adr_003_requires` then attempted
    // a config read against a web server that had already stopped, recording `config_form` as neither
    // compared nor an accepted limit. See `configure_without_persisting`.
    let web = FakeConfigWeb::start(CONFIG_PAGE_RAW_LANE, PROFILE_PAGE_SEMANTIC_LANE).await;
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy::default(),
        HqpTimeouts {
            max_attempts: 1,
            ..fast_timeouts()
        },
    )
    .await;
    h.adapter.connect().await.expect("connect");
    configure_without_persisting(
        &h.adapter,
        "127.0.0.1",
        h.server.port(),
        web.port,
        "conformance",
        "conformance",
    )
    .await;
    h.adapter
        .connect()
        .await
        .expect("reconnect after configure");

    let before = h
        .adapter
        .get_pipeline_status()
        .await
        .expect("the cache fills while the daemon is healthy");
    let stale: Vec<&str> = before
        .settings
        .shaper
        .options
        .iter()
        .map(|o| o.value.as_str())
        .collect();
    assert!(
        stale.contains(&"NS9"),
        "precondition: the pre-load shapers are cached, got {stale:?}"
    );

    // The profile load succeeds; the refresh that follows it cannot complete. Armed only now, after
    // the cache above filled successfully — otherwise the test would be about a daemon that never
    // worked rather than one that stopped.
    h.model.arm(|f| {
        // One rejection breaks the eager post-load refresh; two more cover the read path's bounded
        // retry. On the broken implementation only the first is consumed because the stale cache
        // makes the read skip both fills. On the fixed implementation the cache is empty, so the
        // read must either establish fresh lists or fail explicitly rather than publish the old ones.
        for _ in 0..3 {
            f.reject_next.push((
                "GetShapers".to_string(),
                "post-profile refresh refused".to_string(),
            ));
        }
    });
    h.adapter
        .load_profile("raw-a")
        .await
        .expect("the POST succeeded, so the profile load did");

    match h.adapter.get_pipeline_status().await {
        Err(e) => assert!(
            e.to_string().contains("lists"),
            "the read must say the setting lists could not be established. Got: {e}"
        ),
        Ok(published) => {
            let offered: Vec<&str> = published
                .settings
                .shaper
                .options
                .iter()
                .map(|o| o.value.as_str())
                .collect();
            panic!(
                "after a profile load, the previous profile's options must never be published as \
                 the current ones — and the rate-based chain check cannot catch this, because a \
                 profile can change filters and shapers while leaving rates alone. Offered \
                 {offered:?}"
            );
        }
    }
    h.stop();
    web.stop();
}

/// Run `configure` with the persisted-config path pointed somewhere nothing else reads.
///
/// **`configure` persists web credentials to one file per process, and every `HqpAdapter::new` reads
/// it.** So a test that supplies credentials hands them to every adapter built afterwards — pointing
/// at a web port that dies with that test — and because `configure` never *clears* credentials, the
/// next test's own `configure` re-saves the inherited ones and the leak outlives its source. A
/// tier-1 capture that inherits them then attempts a `/config` read that cannot succeed, and records
/// a required claim as unverified: `tier1_checks_every_family_adr_003_requires` fails.
///
/// Two narrower attempts failed before this one, and the reason is worth keeping. Deleting the file
/// after `configure`, and snapshot-and-restore around it, both only *narrow* the window — it stays as
/// wide as the gap between two syscalls, and with hundreds of tests building adapters concurrently
/// that gap is hit. Measured: the suite failed roughly four runs in eight.
///
/// This instead makes the race **benign**. While the credentialed write happens, the shared path
/// points at a scratch directory: a concurrent `HqpAdapter::new` reads an absent file and gets no
/// credentials, which is exactly what it should have; a concurrent `configure` writes a config
/// nothing reads back, and nothing in this suite asserts that the file survives —
/// `isolate_config_dir` exists to protect a real user's config, not to test persistence.
///
/// The adapter keeps its credentials in memory throughout, which is what the caller actually needs.
///
/// Two callers of this helper running at once would defeat it: `UHC_CONFIG_DIR` is one process-wide
/// variable, so the second one's restore writes back whichever value it happened to read — the
/// *other* caller's scratch path, already deleted — and every later `HqpAdapter::new` reads from a
/// directory that no longer exists. The lock below makes the redirect-and-restore one indivisible
/// step. It is async because the `configure` it wraps is, and holding a `std::sync::Mutex` across an
/// await would block the runtime worker rather than yield it.
async fn configure_without_persisting(
    adapter: &HqpAdapter,
    host: &str,
    port: u16,
    web_port: u16,
    user: &str,
    password: &str,
) {
    static CONFIG_DIR_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    let _guard = CONFIG_DIR_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;

    let previous = std::env::var("UHC_CONFIG_DIR").expect("the suite isolates the config dir");
    let scratch = std::path::Path::new(&previous).join(format!(
        "no-persist-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&scratch).expect("scratch config dir");
    std::env::set_var("UHC_CONFIG_DIR", &scratch);
    adapter
        .configure(
            host.to_string(),
            Some(port),
            Some(web_port),
            Some(user.to_string()),
            Some(password.to_string()),
        )
        .await;
    std::env::set_var("UHC_CONFIG_DIR", &previous);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The lazy fill is decided **before** the reads, and a reconnect during them empties the cache
/// behind that decision: both `mark_disconnected` and `connect` invalidate, because a session's list
/// indices must not outlive it. So a read that found the cache complete can reach the publish step
/// holding nothing — and the chain check waves it through, correctly, because with no previous
/// fingerprint there is no *movement* to report. It is a check about change, not about presence.
///
/// Found while probing why the profile-load case would not go red: the mechanism that saved it there
/// is this same invalidation, and following it the other way turns up a live hole rather than a
/// theoretical one.
///
/// **Label: client-red.**
#[tokio::test]
async fn a_reconnect_during_the_reads_does_not_publish_the_cache_it_emptied() {
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            disruption: Disruption::DropNextReplyOnce,
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");
    let before = h
        .adapter
        .get_pipeline_status()
        .await
        .expect("the cache fills while the daemon is healthy");
    assert_eq!(
        published_families(&before).len(),
        4,
        "precondition: a complete cache, so the fill below is skipped"
    );

    // The next request draws no reply. The client reconnects and retries inside `send_command` — and
    // both halves of that invalidate, so by the time the read reaches the publish step the cache the
    // fill decision was made against is gone.
    h.server.arm_disruption();

    match h.adapter.get_pipeline_status().await {
        Err(e) => assert!(
            e.to_string().contains("lists") || e.to_string().contains("chain"),
            "the failure must name what could not be established. Got: {e}"
        ),
        Ok(published) => {
            let families = published_families(&published);
            assert_eq!(
                families.len(),
                4,
                "a read may recover and publish a complete set, but it must never publish the \
                 remains of a cache a reconnect emptied under it. Published {families:?}, with \
                 filter1x selected as {:?}",
                published.settings.filter1x.selected.value
            );
        }
    }
    h.stop();
}

/// A chain check that **could not run** is not a chain check that passed.
///
/// The final `GetRates` is the whole basis for joining `State`'s indices to the cached enumerations:
/// it is what establishes that the list a selection is resolved through belongs to the chain the
/// daemon had loaded when it reported that selection. An explicit `result="Error"` on it is terminal
/// in `send_command` — it does not retry, does not disconnect and does not invalidate — so the cache
/// survives intact and looks entirely usable. That is precisely what makes swallowing it dangerous:
/// the published view is indistinguishable from a verified one, and a caller has no way to see that
/// the join was never checked.
///
/// The earlier reading was "the cache was not invalidated, so the lists are still the ones the state
/// was read against". That is true about the *cache* and says nothing about the *daemon*: the lists
/// are unchanged locally, while whether the daemon still has that chain loaded is exactly the
/// question the failed request was asked. A `warn!` is not a substitute for an answer.
///
/// So it takes the same shape as every other unsettled read here — one bounded retry, then an error.
///
/// **Label: client-red.** Found by CodeRabbit at `619dee4`. Before this, a rejected final `GetRates`
/// was logged and the state was projected through the cached lists anyway.
#[tokio::test]
async fn a_final_chain_check_that_cannot_run_twice_is_reported_rather_than_published() {
    let h = Harness::verified().await;
    let before = h
        .adapter
        .get_pipeline_status()
        .await
        .expect("the cache fills while the daemon is healthy");
    assert_eq!(
        published_families(&before).len(),
        4,
        "precondition: a complete cache, so the read below skips its fill and the only `GetRates` \
         left in it is the chain check"
    );
    let rates_before = h.model.request_count("GetRates");

    // Two rejections: the first catches the read's chain check, the second catches the retry's. The
    // daemon stays up throughout — an explicit rejection is not a transport failure — so nothing
    // invalidates and the cache remains fully populated and inviting.
    h.model.arm(|f| {
        for _ in 0..2 {
            f.reject_next
                .push(("GetRates".to_string(), "chain check refused".to_string()));
        }
    });

    let outcome = h.adapter.get_pipeline_status().await;

    match outcome {
        Err(e) => {
            let message = e.to_string();
            assert!(
                message.contains("chain"),
                "the failure must say what could not be established — that the loaded chain could \
                 not be verified — rather than surfacing as some unrelated read error. Got: \
                 {message}"
            );
            assert_eq!(
                h.model.request_count("GetRates") - rates_before,
                2,
                "the bound is one retry, not zero and not a loop: a first unverifiable check is \
                 re-read once before the read gives up"
            );
        }
        Ok(published) => {
            let families = published_families(&published);
            panic!(
                "a read whose chain check never ran must not be published as a verified one. It \
                 published {families:?}, with filter1x selected as {:?} out of {} options — \
                 indices joined to lists nothing confirmed belong to the same chain",
                published.settings.filter1x.selected.value,
                published.settings.filter1x.options.len()
            );
        }
    }
    h.stop();
}

/// The bound is a **retry**, not a second way to fail. A single unverifiable chain check followed by
/// one that runs and passes leaves the read with exactly what it needs — a state and a set of lists
/// confirmed to describe the same loaded chain — and that read is published.
///
/// This is the other half of the check above, and it is what keeps the fix from degrading into
/// "any failed request fails the read": the request count is asserted, so a read that published
/// without a *successful* check cannot pass by publishing the same four families the swallowing
/// version did.
///
/// **Label: client-red.** Before this, the retry did not exist: the first rejection was logged and
/// the cached lists were published unverified, so coherence was never established at all.
#[tokio::test]
async fn a_final_chain_check_that_runs_on_the_retry_publishes_the_verified_read() {
    let h = Harness::verified().await;
    let before = h
        .adapter
        .get_pipeline_status()
        .await
        .expect("the cache fills while the daemon is healthy");
    assert_eq!(
        published_families(&before).len(),
        4,
        "precondition: a complete cache, so the read below skips its fill"
    );
    let rates_before = h.model.request_count("GetRates");

    h.model.arm(|f| {
        f.reject_next
            .push(("GetRates".to_string(), "chain check refused".to_string()))
    });

    let published = h
        .adapter
        .get_pipeline_status()
        .await
        .expect("a check that runs on the retry and passes is a verified read");

    assert_eq!(
        h.model.request_count("GetRates") - rates_before,
        2,
        "the publication must rest on a check that actually ran: one rejected, one answered. A \
         single request would mean the rejection was swallowed and nothing was verified"
    );
    assert_eq!(
        published_families(&published).len(),
        4,
        "a verified read publishes the complete set"
    );
    assert_eq!(
        published.settings.filter1x.selected.value, before.settings.filter1x.selected.value,
        "and the daemon did not move, so the verified selection is the one it held all along"
    );
    h.stop();
}

/// **The proof is not the end of the read.** Everything the read does *after* proving coherence is
/// still able to destroy it, and the lazy `VolumeRange` fill is exactly that: a network request,
/// issued once the chain check has already passed, on the way to the projection.
///
/// A lost reply to it is not exotic. `send_command` calls `mark_disconnected`, which invalidates
/// every chain-scoped list because a session's indices must not outlive it, then reconnects — and
/// `connect` invalidates again for the same reason. The read then clones four empty vectors and
/// hands them back as `Ok`, having proved a coherence that no longer describes anything it is
/// publishing.
///
/// This is reachable on the **first** read of a connection, which is the only read that has to fetch
/// the volume range at all: nothing is cached, so the request is always made. No chain movement, no
/// profile load, no concurrency — one dropped reply.
///
/// The assertion is deliberately two-sided. Recovering and publishing a complete set is a fine
/// answer, and so is refusing; what is forbidden is the third thing, an `Ok` carrying none of the
/// daemon's settings, which a caller cannot tell from a daemon that offers none.
///
/// **Label: client-red.** Found by CodeRabbit at `b7a7a1a`. Before this, the read published four
/// empty families.
#[tokio::test]
async fn a_lost_volume_range_reply_does_not_publish_the_cache_its_reconnect_emptied() {
    // Element-scoped and one-shot: the daemon applies the request, then vanishes without replying,
    // once. Scoping it to `VolumeRange` keeps it clear of connection setup and of the list fill, so
    // the only thing it can disturb is the lazy volume-range fetch itself.
    let h = Harness::start(
        VERIFIED_PROFILE,
        WirePolicy {
            apply_then_drop_reply_for_element: Some("VolumeRange".to_string()),
            ..WirePolicy::default()
        },
        fast_timeouts(),
    )
    .await;
    h.adapter.connect().await.expect("connect");

    // The first read after connect: nothing is cached, so this read both fills the lists and fetches
    // the volume range.
    let outcome = h.adapter.get_pipeline_status().await;

    assert!(
        h.server.element_drop_fired(),
        "precondition: the read must actually have lost a `VolumeRange` reply, otherwise this test \
         proves nothing about what follows one"
    );

    match outcome {
        Err(e) => assert!(
            e.to_string().contains("lists") || e.to_string().contains("chain"),
            "refusing is a legitimate answer, but it must name what could not be established. Got: \
             {e}"
        ),
        Ok(published) => {
            let families = published_families(&published);
            assert_eq!(
                families.len(),
                4,
                "a read may recover and publish a complete set, but a proof of coherence taken \
                 before a reconnect says nothing about the cache that reconnect emptied. Published \
                 {families:?}, with filter1x selected as {:?} out of {} options",
                published.settings.filter1x.selected.value,
                published.settings.filter1x.options.len()
            );
        }
    }
    h.stop();
}
