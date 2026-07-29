//! HQPlayer native-protocol conformance suite (issue #322).
//!
//! These tests drive the **real** [`HqpAdapter`] over a **real** TCP socket against a fake daemon
//! that speaks the documented wire protocol. That is the whole point: `tests/adapter_integration.rs`
//! currently tests the HQPlayer mock with a hand-rolled TCP client instead of the adapter, saying so
//! in a comment ("Use reqwest to test the mock directly (not through adapter) because HQP adapter
//! uses a complex TCP protocol"), so no existing test can observe a client protocol defect.
//!
//! Every assertion goes through the adapter's public surface. None asserts elapsed wall-clock time.
//!
//! Protocol truth is the corpus under `tests/fixtures/hqplayer/`, cross-checked against:
//! <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/protocol.md>

mod mock_servers;

use std::sync::Arc;
use std::sync::Once;

use unified_hifi_control::adapters::hqplayer::HqpAdapter;
use unified_hifi_control::bus::create_bus;

use mock_servers::hqplayer::corpus::{self, VERIFIED_PROFILE};
use mock_servers::hqplayer::wire::{Chunking, Responder, WirePolicy, WireServer};

/// `HqpAdapter::configure` persists to disk. Redirect the config directory at process start so a
/// conformance run can never overwrite a real user's `hqp-config.json`.
fn isolate_config_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!(
            "uhc-hqp-conformance-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create isolated config dir");
        std::env::set_var("UHC_CONFIG_DIR", &dir);
    });
}

/// The verified `GetInfo` reply, sent as a single-line document so connection setup is never the
/// thing under test.
const GET_INFO: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
    "<GetInfo engine=\"6.0.4\" name=\"Opal\" platform=\"Linux\" ",
    "product=\"Signalyst HQPlayer Embedded\" version=\"6\"/>"
);

/// A `State` reply matching the daemon state the corpus `Status` fixture describes: playing, PCM
/// mode (list index 1), rate auto, decimal dB volume.
const STATE_PLAYING: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
    "<State state=\"2\" mode=\"1\" filter=\"6\" filter1x=\"6\" filterNx=\"6\" shaper=\"4\" ",
    "rate=\"0\" volume=\"-23.5\" active_mode=\"1\" active_rate=\"0\" convolution=\"0\" ",
    "invert=\"0\" repeat=\"0\" random=\"0\" adaptive=\"0\" filter_junk=\"0\" ",
    "matrix_profile=\"Default\"/>"
);

/// Minimal responder for the framing checkpoint: identity, one `Status` document from the corpus,
/// and one `State` document. Deliberately not a daemon model — only what this expectation needs.
struct FramingResponder {
    status: String,
}

impl Responder for FramingResponder {
    fn respond(&self, request: &str) -> Option<String> {
        if request.contains("<GetInfo") {
            Some(GET_INFO.to_string())
        } else if request.contains("<Status") {
            Some(self.status.clone())
        } else if request.contains("<State") {
            Some(STATE_PLAYING.to_string())
        } else {
            None
        }
    }
}

/// Verified framing rule: a `Status` document carrying a track has a **self-closing** `<metadata/>`
/// child, so the document ends at `</Status>` and not at the child's `/>`. A client that stops at
/// the first `/>` leaves `</Status>` unread in the socket, and the *next* command reads that
/// leftover as its own reply — so the failure is visible as a desynchronised stream, not as a bad
/// `Status`.
///
/// Client expectation: after reading a `Status` document that contains a `metadata` child, the next
/// `State` read still reports the daemon's actual state.
#[tokio::test]
async fn state_read_after_status_with_metadata_child_reports_the_daemon_state() {
    isolate_config_dir();

    let fixture = corpus::load(VERIFIED_PROFILE, "status_playing_with_metadata");
    assert!(
        fixture.provenance.source.contains("hqptuner"),
        "corpus fixture must carry protocol provenance, got {:?}",
        fixture.provenance
    );

    // Declaration on the same line as the opening tag, so document-shape is the only variable:
    // the only newlines are the ones the daemon puts around the metadata child.
    let status = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>{}",
        fixture.document
    );

    let server = WireServer::start(
        Arc::new(FramingResponder { status }),
        WirePolicy {
            // Force `</Status>` into a separate TCP segment from the child's `/>`, so the client
            // cannot pass by accident of buffering.
            chunking: Chunking::AfterMarker("/>".to_string()),
            ..WirePolicy::default()
        },
    )
    .await;

    let bus = create_bus();
    let adapter = HqpAdapter::new(bus);
    adapter
        .configure("127.0.0.1".to_string(), Some(server.port()), None, None, None)
        .await;

    // connect() issues GetInfo and then Status, so the metadata child is read here.
    adapter.connect().await.expect("connect to fake daemon");

    let state = adapter.get_state().await.expect("read State");

    assert_eq!(
        state.state, 2,
        "State read after a Status document with a self-closing metadata child must report the \
         daemon's playback state (2 = playing). Got {}, which means the Status read stopped at the \
         metadata child's `/>` and left `</Status>` in the socket for this command to consume. \
         Full state: {:?}",
        state.state, state
    );

    server.stop();
}
