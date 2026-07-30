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
//! * Every assertion goes through the adapter's public surface, so a later sans-io extraction
//!   (#162) cannot invalidate the suite.
//! * **No test asserts elapsed wall-clock time.** Timeout and reconnect behaviour is exercised
//!   through the injectable [`HqpTimeouts`] seam and asserted on outcomes and attempt counts.
//! * The suite is hermetic: it needs no HQPlayer. The opt-in real-daemon mode is
//!   [`real_daemon_smoke_check_when_opted_in`], and it is a connectivity/identity smoke check only —
//!   diffing the corpus against live hardware is stage-3 work.
//! * Protocol truth is the corpus under `tests/fixtures/hqplayer/`, cross-checked against
//!   <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/protocol.md>.
//!   Current Rust behaviour is never treated as the specification.

mod mock_servers;

use std::sync::Arc;
use std::sync::Once;
use std::time::Duration;

use unified_hifi_control::adapters::hqplayer::{framing, HqpAdapter, HqpTimeouts};
use unified_hifi_control::bus::create_bus;

use mock_servers::hqplayer::corpus::{self, LEGACY_PROFILE, VERIFIED_PROFILE};
use mock_servers::hqplayer::model::{request_attr, DaemonModel, DocumentStyle, Metadata};
use mock_servers::hqplayer::tier1;
use mock_servers::hqplayer::wire::{Chunking, Disruption, WirePolicy, WireServer};

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

    let result = h.adapter.set_filter_1x("IIR").await;

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

#[tokio::test]
async fn an_unknown_command_is_reported_as_an_error_without_dropping_the_connection() {
    let h = Harness::verified().await;
    // `control` rejects unknown actions locally, so drive an unknown element through a setter the
    // daemon does not recognise by asking for a mode index the daemon will refuse.
    let rejected = h.adapter.set_mode("99").await;
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
async fn mute_is_toggled_on_the_daemon() {
    let h = Harness::verified().await;
    h.adapter.volume_mute().await.expect("VolumeMute");
    let muted = h.model.state().muted;
    h.adapter.volume_mute().await.expect("VolumeMute");
    assert_eq!(
        (muted, h.model.state().muted),
        (true, false),
        "VolumeMute is a toggle on the daemon, not an absolute set"
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
    let index = corpus::index_of(&filters, "FiltersItem", "poly-sinc-lp").expect("observed index");

    h.adapter
        .set_filter_1x("poly-sinc-lp")
        .await
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

    let result = h.adapter.set_filter(stored, None).await;
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
// AC8 - hermetic in CI, with a documented opt-in real-daemon mode (a smoke check, by design)
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
    let port: u16 = std::env::var("UHC_HQP_CONFORMANCE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4321);
    let web_port: u16 = std::env::var("UHC_HQP_CONFORMANCE_WEB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8088);
    let profile = std::env::var("UHC_HQP_CONFORMANCE_PROFILE")
        .unwrap_or_else(|_| VERIFIED_PROFILE.to_string());
    let user = std::env::var("UHC_HQP_CONFORMANCE_WEB_USER").ok();
    let pass = std::env::var("UHC_HQP_CONFORMANCE_WEB_PASS").ok();

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

    assert!(
        report.is_clean(),
        "tier-1 found {} divergence(s) between the daemon and corpus profile `{profile}`. Each one \
         must be resolved by re-provenancing the fixture from this capture or by shipping it as a \
         stated gap - not by loosening the differ.",
        report.divergences.len()
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
    // 12 frames ~ six seconds of 2 Hz push, i.e. twice a response window. Newline-separated, because
    // several documents sharing ONE line cost a single skip - `response.clear()` drops the rest of
    // the line with them - so `repeat` would not exercise the bound at all.
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
