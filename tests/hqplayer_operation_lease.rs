//! Endpoint-stable HQPlayer semantic-operation regressions (issue #368).
//!
//! Every server in this file is a loopback stateful fake. No live HQPlayer is contacted.

#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{io::Cursor, io::Write};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use mock_servers::hqplayer::corpus::{self, VERIFIED_PROFILE};
use mock_servers::hqplayer::model::DaemonModel;
use mock_servers::hqplayer::wire::{ReplyGate, WirePolicy, WireServer};
use unified_hifi_control::adapters::hqplayer::{
    HqpAdapter, HqpTimeouts, LegacySettingTarget, SettingOutcome,
};
use unified_hifi_control::bus::create_bus;

fn fast_timeouts() -> HqpTimeouts {
    HqpTimeouts {
        connect: Duration::from_millis(100),
        response: Duration::from_millis(750),
        reconnect_delay: Duration::from_millis(10),
        max_attempts: 1,
    }
}

async fn wait_for_request(server: &WireServer, element: &str, after: u32) {
    for _ in 0..100 {
        if server.stats().element_count(element) > after {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("fake daemon never received {element}");
}

async fn wait_for_gate(gate: &ReplyGate) {
    tokio::time::timeout(Duration::from_secs(1), gate.wait_until_reached())
        .await
        .expect("fake daemon reaches the manually gated request");
}

fn queue_native_reconfigure(
    adapter: Arc<HqpAdapter>,
    port: u16,
) -> (Arc<Notify>, tokio::task::JoinHandle<()>) {
    let started = Arc::new(Notify::new());
    let task = {
        let started = started.clone();
        tokio::spawn(async move {
            started.notify_one();
            adapter
                .configure("127.0.0.1".to_string(), Some(port), None, None, None)
                .await;
        })
    };
    (started, task)
}

async fn assert_reconfigure_is_waiting(
    started: &Notify,
    reconfigure: &tokio::task::JoinHandle<()>,
) {
    started.notified().await;
    // The spawned task does not yield between notifying and polling `configure`, so reaching this
    // point means it has tried the operation lease and suspended behind the gated operation.
    assert!(
        !reconfigure.is_finished(),
        "configure must remain pending until the gated operation releases its endpoint lease"
    );
}

async fn connected_pair() -> (WireServer, WireServer, Arc<HqpAdapter>) {
    let (_, first, _, second, adapter) = connected_pair_with_models().await;
    (first, second, adapter)
}

async fn connected_pair_with_models() -> (
    DaemonModel,
    WireServer,
    DaemonModel,
    WireServer,
    Arc<HqpAdapter>,
) {
    let first_model = DaemonModel::with_profile(VERIFIED_PROFILE);
    let first = WireServer::start(Arc::new(first_model.clone()), WirePolicy::default()).await;
    let second_model = DaemonModel::with_profile(VERIFIED_PROFILE);
    let second = WireServer::start(Arc::new(second_model.clone()), WirePolicy::default()).await;
    let adapter = Arc::new(HqpAdapter::new(create_bus()));
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(first.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter
        .connect()
        .await
        .expect("coherent endpoint A session");
    (first_model, first, second_model, second, adapter)
}

struct ProfileWeb {
    port: u16,
    post_seen: Arc<Notify>,
    release_post: Arc<Notify>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

fn profile_backup(applied: bool) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer.start_file("hqplayerd.xml", options).unwrap();
    let working = if applied {
        br#"<hqplayerd><engine profile="raw-a"/></hqplayerd>"#.as_slice()
    } else {
        br#"<hqplayerd><engine profile="default"/></hqplayerd>"#.as_slice()
    };
    writer.write_all(working).unwrap();
    writer.start_file("data/cfgs/raw-a.xml", options).unwrap();
    writer
        .write_all(br#"<hqplayerd><engine profile="raw-a"/></hqplayerd>"#)
        .unwrap();
    writer.finish().unwrap().into_inner()
}

impl ProfileWeb {
    async fn start(pause_post: bool, drop_post_response: bool) -> Self {
        Self::start_with_post_status(pause_post, drop_post_response, 200).await
    }

    async fn start_with_post_status(
        pause_post: bool,
        drop_post_response: bool,
        post_status: u16,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind profile web fake");
        let port = listener.local_addr().expect("profile web addr").port();
        let post_seen = Arc::new(Notify::new());
        let release_post = Arc::new(Notify::new());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let applied = Arc::new(AtomicBool::new(false));
        let before_backup = Arc::new(profile_backup(false));
        let after_backup = Arc::new(profile_backup(true));
        let task = {
            let post_seen = post_seen.clone();
            let release_post = release_post.clone();
            let requests = requests.clone();
            let applied = applied.clone();
            let before_backup = before_backup.clone();
            let after_backup = after_backup.clone();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let post_seen = post_seen.clone();
                    let release_post = release_post.clone();
                    let requests = requests.clone();
                    let applied = applied.clone();
                    let before_backup = before_backup.clone();
                    let after_backup = after_backup.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        loop {
                            let mut chunk = [0u8; 1024];
                            let read = socket.read(&mut chunk).await.unwrap_or(0);
                            if read == 0 || head.len() + read > 16 * 1024 {
                                return;
                            }
                            head.extend_from_slice(&chunk[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let request = String::from_utf8_lossy(&head);
                        let first_line = request.lines().next().unwrap_or_default().to_string();
                        requests
                            .lock()
                            .expect("profile request log lock")
                            .push(first_line.clone());
                        if first_line.starts_with("POST /restore ") {
                            post_seen.notify_one();
                            if pause_post {
                                release_post.notified().await;
                            }
                            // A response failure is deliberately independent of application. The
                            // adapter must reconcile it through the fresh backup readback.
                            applied.store(true, Ordering::Release);
                            if drop_post_response {
                                return;
                            }
                        }
                        let (body, content_type): (Vec<u8>, &str) =
                            if first_line.starts_with("GET /backup/settings.zip ") {
                                let archive = if applied.load(Ordering::Acquire) {
                                    after_backup.as_ref()
                                } else {
                                    before_backup.as_ref()
                                };
                                (archive.clone(), "application/zip")
                            } else if first_line.starts_with("GET /config/profile/load ") {
                                (
                                    r#"<html><body><form>
                            <input type="hidden" name="_xsrf" value="token"/>
                            <select name="profile"><option value="raw-a">Raw A</option></select>
                            </form></body></html>"#
                                        .as_bytes()
                                        .to_vec(),
                                    "text/html",
                                )
                            } else {
                                (b"ok".to_vec(), "text/html")
                            };
                        let status = if first_line.starts_with("POST /restore ") {
                            post_status
                        } else {
                            200
                        };
                        let response = format!(
                            "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: \
                             {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                        let _ = socket.flush().await;
                    });
                }
            })
        };
        Self {
            port,
            post_seen,
            release_post,
            requests,
            task,
        }
    }

    fn count(&self, method: &str) -> usize {
        self.requests
            .lock()
            .expect("profile request log lock")
            .iter()
            .filter(|line| line.starts_with(method))
            .count()
    }

    fn stop(self) {
        self.task.abort();
    }
}

/// A semantic setter is one operation even though the protocol exposes it as several requests.
/// Once its opening enumeration came from endpoint A, runtime reconfiguration must wait until the
/// setter has written and verified against A; it must never continue the bracket on endpoint B.
///
/// **Label: client-red.** Per-request conversation serialization lets queued `configure()` win the
/// mutex immediately after `GetModes`, so the existing setter reconnects and writes endpoint B.
#[tokio::test]
async fn reconfiguration_waits_for_the_whole_semantic_setter() {
    let (first, second, adapter) = connected_pair().await;

    // Hold the opening enumeration until configure has actually tried and failed to acquire the
    // operation lease. No scheduler timing window is involved.
    let gate = ReplyGate::new("GetModes");
    first.set_policy(WirePolicy {
        reply_gate: Some(gate.clone()),
        ..WirePolicy::default()
    });
    let setting = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.set_mode("SDM").await })
    };
    wait_for_gate(&gate).await;

    let (configure_started, reconfigure) = queue_native_reconfigure(adapter.clone(), second.port());
    assert_reconfigure_is_waiting(&configure_started, &reconfigure).await;
    gate.release();

    let _outcome = setting
        .await
        .expect("setter task joins")
        .expect("setter succeeds on endpoint A");
    reconfigure.await.expect("configure completes after setter");
    assert_eq!(first.stats().element_count("SetMode"), 1);
    assert_eq!(
        second.stats().element_count("SetMode"),
        0,
        "a value resolved on endpoint A must never be written to endpoint B"
    );

    first.stop();
    second.stop();
}

/// Reconfiguration queued after the write must not make endpoint B satisfy endpoint A's readback.
#[tokio::test]
async fn setter_readback_stays_on_the_endpoint_that_received_the_write() {
    let (first, second, adapter) = connected_pair().await;
    let gate = ReplyGate::new("SetMode");
    first.set_policy(WirePolicy {
        reply_gate: Some(gate.clone()),
        ..WirePolicy::default()
    });
    let setting = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.set_mode("SDM").await })
    };
    wait_for_gate(&gate).await;

    let (configure_started, reconfigure) = queue_native_reconfigure(adapter.clone(), second.port());
    assert_reconfigure_is_waiting(&configure_started, &reconfigure).await;
    gate.release();

    let _outcome = setting
        .await
        .expect("setter task joins")
        .expect("endpoint A readback proves its own write");
    reconfigure
        .await
        .expect("configure completes after readback");
    assert_eq!(second.stats().element_count("State"), 0);
    assert_eq!(second.stats().element_count("GetModes"), 0);

    first.stop();
    second.stop();
}

/// A lost setter reply must not transparently reconnect and replay a list position on a replacement
/// native session. Delivery on the original session is ambiguous, and that is the whole answer.
#[tokio::test]
async fn post_write_session_loss_is_ambiguous_without_cross_session_retry() {
    let (first, second, adapter) = connected_pair().await;
    first.set_policy(WirePolicy {
        apply_then_drop_reply_for_element: Some("SetMode".to_string()),
        ..WirePolicy::default()
    });
    let connections_before = first.stats().connections();

    let outcome = adapter
        .set_mode("SDM")
        .await
        .expect("session loss is represented as an explicit outcome");
    assert!(
        matches!(
            outcome,
            unified_hifi_control::adapters::hqplayer::SettingOutcome::Ambiguous { .. }
        ),
        "a write whose session vanished cannot be reported as applied: {outcome:?}"
    );
    assert_eq!(first.stats().element_count("SetMode"), 1);
    assert_eq!(
        first.stats().connections(),
        connections_before,
        "the semantic write must not reconnect and replay its index"
    );
    assert_eq!(second.stats().element_count("SetMode"), 0);

    first.stop();
    second.stop();
}

/// The frozen numeric compatibility boundary is one operation: list position resolution cannot be
/// leased separately from the named semantic write it selects.
#[tokio::test]
async fn legacy_index_resolution_and_apply_share_one_endpoint() {
    let (first_model, first, second_model, second, adapter) = connected_pair_with_models().await;
    assert_eq!(
        corpus::enum_entries(&corpus::document(VERIFIED_PROFILE, "modes"), "ModesItem").len(),
        3,
        "this regression depends on verified list position 2"
    );
    let gate = ReplyGate::new("GetModes");
    first.set_policy(WirePolicy {
        reply_gate: Some(gate.clone()),
        ..WirePolicy::default()
    });
    let setting = {
        let adapter = adapter.clone();
        tokio::spawn(async move {
            adapter
                .apply_legacy_index(LegacySettingTarget::Mode, 2)
                .await
        })
    };
    wait_for_gate(&gate).await;

    let (configure_started, reconfigure) = queue_native_reconfigure(adapter.clone(), second.port());
    assert_reconfigure_is_waiting(&configure_started, &reconfigure).await;
    gate.release();

    let _outcome = setting
        .await
        .expect("legacy setting task joins")
        .expect("legacy mode index is applied on endpoint A");
    reconfigure.await.expect("configure completes after apply");
    assert_eq!(first.stats().element_count("SetMode"), 1);
    assert_eq!(second.stats().element_count("SetMode"), 0);
    assert_eq!(
        first_model.state().mode_index,
        2,
        "list position 2 resolves to SDM (DSD) and is applied on endpoint A"
    );
    assert_ne!(
        second_model.state().mode_index,
        2,
        "the resolved value must not leak to endpoint B"
    );

    first.stop();
    second.stop();
}

/// Matrix positions are list-local too, so numeric resolution, name write, and State verification
/// must remain one operation.
#[tokio::test]
async fn matrix_index_resolution_and_apply_share_one_endpoint() {
    let (first_model, first, second_model, second, adapter) = connected_pair_with_models().await;
    assert_eq!(
        corpus::enum_entries(
            &corpus::document(VERIFIED_PROFILE, "matrix_profiles"),
            "MatrixProfile"
        )
        .len(),
        3,
        "this regression depends on the verified matrix-profile list"
    );
    let gate = ReplyGate::new("MatrixListProfiles");
    first.set_policy(WirePolicy {
        reply_gate: Some(gate.clone()),
        ..WirePolicy::default()
    });
    let setting = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.set_matrix_profile(1).await })
    };
    wait_for_gate(&gate).await;

    let (configure_started, reconfigure) = queue_native_reconfigure(adapter.clone(), second.port());
    assert_reconfigure_is_waiting(&configure_started, &reconfigure).await;
    gate.release();

    let _outcome = setting
        .await
        .expect("matrix setting task joins")
        .expect("matrix profile is applied on endpoint A");
    reconfigure.await.expect("configure completes after apply");
    assert_eq!(first.stats().element_count("MatrixSetProfile"), 1);
    assert_eq!(second.stats().element_count("MatrixSetProfile"), 0);
    assert_eq!(second.stats().element_count("State"), 0);
    assert_eq!(
        first_model.state().matrix_profile,
        "Speakers",
        "list position 1 resolves to Speakers and is applied on endpoint A"
    );
    assert_ne!(
        second_model.state().matrix_profile,
        "Speakers",
        "the resolved profile must not leak to endpoint B"
    );

    first.stop();
    second.stop();
}

/// Cancellation drops the RAII operation lease and lets queued reconfiguration make progress.
#[tokio::test]
async fn cancelling_an_operation_releases_queued_reconfiguration() {
    let (first, second, adapter) = connected_pair().await;
    let gate = ReplyGate::new("GetModes");
    first.set_policy(WirePolicy {
        reply_gate: Some(gate.clone()),
        ..WirePolicy::default()
    });
    let setting = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.set_mode("SDM").await })
    };
    wait_for_gate(&gate).await;

    let (configure_started, reconfigure) = queue_native_reconfigure(adapter.clone(), second.port());
    assert_reconfigure_is_waiting(&configure_started, &reconfigure).await;
    setting.abort();
    gate.release();

    tokio::time::timeout(Duration::from_millis(500), reconfigure)
        .await
        .expect("cancelled operation releases the queued configure lease")
        .expect("configure task joins");
    assert_eq!(first.stats().element_count("SetMode"), 0);
    assert_eq!(second.stats().element_count("SetMode"), 0);

    first.stop();
    second.stop();
}

/// Cancellation after a request was written must poison that native socket. Otherwise its late
/// reply can satisfy the next same-named request and make stale enumeration bytes look fresh.
#[tokio::test]
async fn cancelling_an_inflight_request_forces_a_clean_same_endpoint_session() {
    let native = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let adapter = Arc::new(HqpAdapter::new(create_bus()));
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter.connect().await.expect("coherent native session");
    let connections_before = native.stats().connections();
    let volume_reads_before = native.stats().element_count("VolumeRange");

    let gate = ReplyGate::new("GetModes");
    native.set_policy(WirePolicy {
        reply_gate: Some(gate.clone()),
        ..WirePolicy::default()
    });
    let cancelled = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.set_mode("SDM").await })
    };
    wait_for_gate(&gate).await;
    cancelled.abort();
    cancelled
        .await
        .expect_err("the in-flight operation is cancelled");
    gate.release();

    let cancelled_status = adapter.get_status().await;
    assert!(
        !cancelled_status.connected && cancelled_status.info.is_none(),
        "a cancelled in-flight conversation cannot remain a reachable producer"
    );
    adapter
        .get_pipeline_status()
        .await
        .expect("the next complete read reconnects and rebuilds session-scoped state");
    assert_eq!(
        native.stats().connections(),
        connections_before + 1,
        "the cancelled request's socket must not be reused"
    );
    assert!(
        native.stats().element_count("VolumeRange") > volume_reads_before,
        "session-A volume capability must be discarded and reread on the replacement session"
    );

    native.stop();
}

/// Timeout is cancellation by the protocol deadline rather than by task abort; it must release the
/// same RAII operation lease and allow queued reconfiguration to finish.
#[tokio::test]
async fn timing_out_an_operation_releases_queued_reconfiguration() {
    let (first, second, adapter) = connected_pair().await;
    adapter
        .set_timeouts(HqpTimeouts {
            // `wait_for_request` has a 200 ms observation budget; leave enough time after it sees
            // the request to prove configure is queued behind the still-held operation lease.
            response: Duration::from_millis(400),
            ..fast_timeouts()
        })
        .await;
    first.set_policy(WirePolicy {
        silent_for_element: Some("GetModes".to_string()),
        ..WirePolicy::default()
    });
    let modes_before = first.stats().element_count("GetModes");
    let setting = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.set_mode("SDM").await })
    };
    wait_for_request(&first, "GetModes", modes_before).await;

    let (configure_started, reconfigure) = queue_native_reconfigure(adapter.clone(), second.port());
    assert_reconfigure_is_waiting(&configure_started, &reconfigure).await;

    assert!(setting.await.expect("setter task joins").is_err());
    tokio::time::timeout(Duration::from_millis(500), reconfigure)
        .await
        .expect("timed-out operation releases the queued configure lease")
        .expect("configure task joins");
    assert_eq!(first.stats().element_count("SetMode"), 0);
    assert_eq!(second.stats().element_count("SetMode"), 0);

    first.stop();
    second.stop();
}

/// A persistent profile load changes the native setting universe. Its backup, restore, readback,
/// cache invalidation, and native enumeration refresh therefore share one endpoint lease.
#[tokio::test]
async fn profile_restore_and_native_refresh_stay_on_one_endpoint() {
    let first = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let second = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let first_web = ProfileWeb::start(true, false).await;
    let second_web = ProfileWeb::start(false, false).await;
    let adapter = Arc::new(HqpAdapter::new(create_bus()));
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(first.port()),
            Some(first_web.port),
            Some("test".to_string()),
            Some("test".to_string()),
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter
        .connect()
        .await
        .expect("coherent endpoint A session");
    let endpoint_before = adapter.endpoint_generation().await;

    let load = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.load_profile("raw-a").await })
    };
    first_web.post_seen.notified().await;

    let second_port = second.port();
    let second_web_port = second_web.port;
    let configure_started = Arc::new(Notify::new());
    let reconfigure = {
        let adapter = adapter.clone();
        let configure_started = configure_started.clone();
        tokio::spawn(async move {
            configure_started.notify_one();
            adapter
                .configure(
                    "127.0.0.1".to_string(),
                    Some(second_port),
                    Some(second_web_port),
                    Some("other".to_string()),
                    Some("other".to_string()),
                )
                .await;
        })
    };

    configure_started.notified().await;
    assert!(
        !reconfigure.is_finished(),
        "configure must remain queued while endpoint A's profile restore is unresolved"
    );
    assert_eq!(adapter.endpoint_generation().await, endpoint_before);
    let filters_before = first.stats().element_count("GetFilters");
    first_web.release_post.notify_one();

    load.await
        .expect("profile task joins")
        .expect("fresh backup readback verifies the applied profile");
    reconfigure
        .await
        .expect("configure follows profile refresh");
    assert_eq!(
        first_web.count("GET "),
        3,
        "backup capture, applied readback, and fresh profile-list recovery stay on endpoint A"
    );
    assert_eq!(first_web.count("POST "), 1);
    assert_eq!(second_web.count("GET "), 0);
    assert_eq!(second_web.count("POST "), 0);
    assert!(
        first.stats().element_count("GetFilters") > filters_before,
        "the verified profile load must force a native enumeration refresh on endpoint A"
    );
    assert_eq!(second.stats().element_count("GetFilters"), 0);
    assert_eq!(
        adapter.endpoint_generation().await,
        endpoint_before.wrapping_add(1),
        "native/web/credential replacement advances endpoint provenance exactly once"
    );

    first.stop();
    second.stop();
    first_web.stop();
    second_web.stop();
}

/// Once a restore has been dispatched, losing its response is reconciled against a fresh backup.
#[tokio::test]
async fn lost_restore_response_is_reconciled_by_persistent_readback() {
    let native = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let web = ProfileWeb::start(false, true).await;
    let adapter = Arc::new(HqpAdapter::new(create_bus()));
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            Some(web.port),
            Some("test".to_string()),
            Some("test".to_string()),
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;

    adapter
        .load_profile("raw-a")
        .await
        .expect("a dropped response is success only after matching backup readback");
    assert_eq!(web.count("GET "), 3);
    assert_eq!(web.count("POST "), 1);

    native.stop();
    web.stop();
}

/// A server-side restore response is not proof of rejection. Matching persistent readback is the
/// authoritative outcome.
#[tokio::test]
async fn restore_server_error_is_reconciled_by_persistent_readback() {
    let native = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let web = ProfileWeb::start_with_post_status(false, false, 500).await;
    let adapter = HqpAdapter::new(create_bus());
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            Some(web.port),
            Some("test".to_string()),
            Some("test".to_string()),
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter.connect().await.expect("coherent native session");
    adapter
        .get_pipeline_status()
        .await
        .expect("precondition: native setting caches are populated");
    let filters_before = native.stats().element_count("GetFilters");

    adapter
        .load_profile("raw-a")
        .await
        .expect("HTTP 500 does not override matching persistent readback");
    assert_eq!(web.count("GET "), 3);
    assert_eq!(web.count("POST "), 1);
    assert!(
        !adapter.get_cached_profiles().await.is_empty(),
        "verified recovery refreshes the persistent profile cache"
    );
    adapter
        .get_pipeline_status()
        .await
        .expect("native settings remain readable after verified profile recovery");
    assert!(
        native.stats().element_count("GetFilters") > filters_before,
        "a verified restore invalidates and refills the native setting cache"
    );

    native.stop();
    web.stop();
}

/// Once a native setter receives its acknowledgement, failure of the same-session State proof is
/// still ambiguous. It must not reconnect and use a replacement session as evidence.
#[tokio::test]
async fn acknowledged_native_write_with_lost_readback_is_ambiguous_without_reconnect() {
    let native = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let adapter = Arc::new(HqpAdapter::new(create_bus()));
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter.connect().await.expect("coherent native session");

    let gate = ReplyGate::new("SetMode");
    native.set_policy(WirePolicy {
        reply_gate: Some(gate.clone()),
        ..WirePolicy::default()
    });
    let setting = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.set_mode("SDM").await })
    };
    wait_for_gate(&gate).await;
    native.set_policy(WirePolicy {
        silent_for_element: Some("State".to_string()),
        ..WirePolicy::default()
    });
    gate.release();

    let outcome = setting
        .await
        .expect("setter task joins")
        .expect("an unverifiable write has an explicit outcome");
    assert!(
        matches!(outcome, SettingOutcome::Ambiguous { .. }),
        "same-session readback loss must be ambiguous, got {outcome:?}"
    );
    assert_eq!(native.stats().element_count("SetMode"), 1);
    assert_eq!(
        native.stats().connections(),
        1,
        "verification must not reconnect and treat another session as proof"
    );

    native.stop();
}

/// The public raw numeric filter method is still a mutating operation. Losing its reply after the
/// fake applied the request must not trigger a positional replay on a replacement session.
#[tokio::test]
async fn raw_numeric_filter_lost_reply_is_ambiguous_without_replay() {
    let native = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let adapter = HqpAdapter::new(create_bus());
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter.connect().await.expect("coherent native session");
    native.set_policy(WirePolicy {
        apply_then_drop_reply_for_element: Some("SetFilter".to_string()),
        ..WirePolicy::default()
    });

    let outcome = adapter
        .set_filter(1, 1)
        .await
        .expect("a lost write reply has an explicit outcome");
    assert!(
        matches!(outcome, SettingOutcome::Ambiguous { .. }),
        "lost raw numeric write reply must be ambiguous, got {outcome:?}"
    );
    assert_eq!(native.stats().element_count("SetFilter"), 1);
    assert_eq!(
        native.stats().connections(),
        1,
        "a positional write must never be replayed on a replacement session"
    );

    native.stop();
}

/// A reconnect during a pipeline read must restart the whole read, including volume capability.
/// Otherwise the result can combine a range from session A with settings and State from session B.
#[tokio::test]
async fn pipeline_read_restarts_when_its_native_session_changes() {
    let native = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let adapter = HqpAdapter::new(create_bus());
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter.connect().await.expect("coherent native session");
    let connections_before = native.stats().connections();
    let volume_reads_before = native.stats().element_count("VolumeRange");
    native.set_policy(WirePolicy {
        apply_then_drop_reply_for_element: Some("GetFilters".to_string()),
        ..WirePolicy::default()
    });

    adapter
        .get_pipeline_status()
        .await
        .expect("the complete pipeline read restarts and settles on the replacement session");
    assert_eq!(native.stats().connections(), connections_before + 1);
    assert!(
        native.stats().element_count("VolumeRange") > volume_reads_before,
        "volume capability must be re-read as part of the replacement session's complete pipeline"
    );

    native.stop();
}

/// A verified restore invalidates every replaced cache, refills the native universe, and refreshes
/// the persistent profile list even when the restore response itself was lost.
#[tokio::test]
async fn verified_restore_refills_native_and_persistent_caches() {
    let native = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let web = ProfileWeb::start(false, true).await;
    let adapter = HqpAdapter::new(create_bus());
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            Some(web.port),
            Some("test".to_string()),
            Some("test".to_string()),
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter.connect().await.expect("coherent native session");
    adapter
        .get_pipeline_status()
        .await
        .expect("precondition: native caches are populated");
    adapter
        .fetch_profiles()
        .await
        .expect("precondition: persistent form cache is populated");
    let filters_before = native.stats().element_count("GetFilters");

    adapter
        .load_profile("raw-a")
        .await
        .expect("matching backup readback reconciles the dropped restore response");
    assert!(
        !adapter.get_cached_profiles().await.is_empty(),
        "successful recovery publishes a freshly fetched persistent profile list"
    );

    adapter
        .get_pipeline_status()
        .await
        .expect("the next pipeline read refills from the daemon");
    assert!(
        native.stats().element_count("GetFilters") > filters_before,
        "the previous profile's chain enumeration must not be reused"
    );

    native.stop();
    web.stop();
}

/// A web endpoint change does not remove the healthy native zone, but it does invalidate every
/// cached form value and advances the endpoint identity used by persistent operations.
#[tokio::test]
async fn changing_only_the_web_endpoint_clears_persistent_lane_caches() {
    let native = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let first_web = ProfileWeb::start(false, false).await;
    let second_web = ProfileWeb::start(false, false).await;
    let adapter = HqpAdapter::new(create_bus());
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            Some(first_web.port),
            Some("test".to_string()),
            Some("test".to_string()),
        )
        .await;
    let profiles = adapter
        .fetch_profiles()
        .await
        .expect("cache profile form A");
    assert_eq!(profiles.len(), 1);
    let before = adapter.endpoint_generation().await;

    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            Some(second_web.port),
            None,
            None,
        )
        .await;

    assert!(adapter.get_cached_profiles().await.is_empty());
    assert_eq!(adapter.endpoint_generation().await, before.wrapping_add(1));

    native.stop();
    first_web.stop();
    second_web.stop();
}

/// Credentials are part of persistent endpoint identity even when every address is unchanged.
#[tokio::test]
async fn changing_only_web_credentials_clears_persistent_lane_caches() {
    let native = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let web = ProfileWeb::start(false, false).await;
    let adapter = HqpAdapter::new(create_bus());
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            Some(web.port),
            Some("first".to_string()),
            Some("first".to_string()),
        )
        .await;
    assert_eq!(adapter.fetch_profiles().await.expect("cache form").len(), 1);
    let before = adapter.endpoint_generation().await;

    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(native.port()),
            Some(web.port),
            Some("second".to_string()),
            Some("second".to_string()),
        )
        .await;

    assert!(adapter.get_cached_profiles().await.is_empty());
    assert_eq!(adapter.endpoint_generation().await, before.wrapping_add(1));

    native.stop();
    web.stop();
}

/// Pin the actual frozen numeric HTTP boundary to the combined adapter operation. A future handler
/// must not regress to `legacy_index_to_name(...).await` followed by a separately leased setter.
#[test]
fn legacy_http_boundary_uses_combined_index_apply() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/api/mod.rs"))
        .expect("read API source");
    let start = source
        .find("async fn hqp_apply_legacy_setting(")
        .expect("legacy handler helper exists");
    let end = source[start..]
        .find("async fn hqp_apply_named_setting(")
        .map(|offset| start + offset)
        .expect("named helper follows legacy helper");
    let helper = &source[start..end];

    assert_eq!(
        helper.matches("apply_legacy_index").count(),
        5,
        "mode, filter pair, both filter sides, and shaper must use the combined operation"
    );
    assert!(!helper.contains("legacy_index_to_name"));
}

/// Keep the await-in-lock exemption honest: the allowlisted guard name may only ever denote the
/// adapter's root operation mutex, never an arbitrary lock hidden behind the same spelling.
#[test]
fn every_operation_guard_is_the_root_operation_lock() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/adapters/hqplayer.rs"
    ))
    .expect("read HQPlayer adapter");
    assert_eq!(
        source.matches("let _operation_guard").count(),
        source
            .matches("let _operation_guard = self.operation_lock.lock().await;")
            .count(),
        "the await-in-lock exemption is valid only for the root endpoint operation mutex"
    );
}

/// Cancellation poison is the synchronous reachability fence. It may only clear after both cached
/// producer state and the shared socket have been invalidated.
#[test]
fn cancelled_transport_poison_clears_after_session_state_and_socket() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/adapters/hqplayer.rs"
    ))
    .expect("read HQPlayer adapter");
    let start = source
        .find("async fn mark_disconnected(&self)")
        .expect("mark_disconnected exists");
    let end = source[start..]
        .find("/// Send command and get response")
        .map(|offset| start + offset)
        .expect("send-command section follows disconnect cleanup");
    let cleanup = &source[start..end];
    let connected_clear = cleanup
        .find("state.connected = false;")
        .expect("producer reachability is cleared");
    let socket_clear = cleanup
        .find("*conn = None;")
        .expect("shared native socket is cleared");
    let poison_clear = cleanup
        .find(".store(false, std::sync::atomic::Ordering::Release);")
        .expect("cancellation poison is cleared");

    assert!(
        poison_clear > connected_clear && poison_clear > socket_clear,
        "poison must remain true until stale producer state and the socket are both invalid"
    );
}
