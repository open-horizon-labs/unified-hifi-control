//! Explicitly opted-in HQPlayer lifecycle qualification.
//!
//! This probe is read-only. It establishes one coherent native session, waits for an operator to
//! restart the daemon, observes the first failed poll, and measures until a complete replacement
//! handshake succeeds. It never invokes a restart itself and never runs in ordinary CI.

use std::io::Write;
use std::time::Duration;

use unified_hifi_control::adapters::hqplayer::{HqpAdapter, HqpTimeouts};
use unified_hifi_control::bus::create_bus;

const OPT_IN_ENV: &str = "UHC_HQP_LIFECYCLE_LIVE";
const HOST_ENV: &str = "UHC_HQP_HOST";
const PORT_ENV: &str = "UHC_HQP_PORT";
const PROBE_INTERVAL: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(750);

#[tokio::test]
#[ignore = "requires explicit opt-in plus an operator-triggered HQPlayer restart"]
async fn supported_daemon_restart_window_is_measured_to_a_coherent_snapshot() {
    assert_eq!(
        std::env::var(OPT_IN_ENV).as_deref(),
        Ok("1"),
        "set {OPT_IN_ENV}=1 explicitly; this lane waits for a live daemon restart"
    );
    let host = std::env::var(HOST_ENV).expect("set UHC_HQP_HOST");
    let port = std::env::var(PORT_ENV)
        .ok()
        .map(|value| value.parse::<u16>().expect("UHC_HQP_PORT is a u16"))
        .unwrap_or(4321);

    let adapter = HqpAdapter::new(create_bus());
    adapter
        .configure(host.clone(), Some(port), None, None, None)
        .await;
    adapter
        .set_timeouts(HqpTimeouts {
            connect: CONNECT_TIMEOUT,
            response: RESPONSE_TIMEOUT,
            reconnect_delay: PROBE_INTERVAL,
            max_attempts: 1,
        })
        .await;
    adapter
        .connect()
        .await
        .expect("baseline coherent HQPlayer handshake");
    let baseline = adapter
        .get_status()
        .await
        .info
        .expect("baseline handshake carries daemon identity");

    println!(
        "HQP_LIFECYCLE_BASELINE_READY={}",
        serde_json::json!({
            "schema": "uhc-hqp-lifecycle/v1",
            "product": baseline.product,
            "version": baseline.version,
            "engine": baseline.engine,
            "host": host,
            "port": port,
            "probe_interval_ms": PROBE_INTERVAL.as_millis(),
            "connect_timeout_ms": CONNECT_TIMEOUT.as_millis(),
            "response_timeout_ms": RESPONSE_TIMEOUT.as_millis(),
        })
    );
    std::io::stdout()
        .flush()
        .expect("flush baseline marker for the operator");

    let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut last_coherent_at = tokio::time::Instant::now();
    let first_failed_observation_at = loop {
        assert!(
            tokio::time::Instant::now() < wait_deadline,
            "no daemon outage was observed within 60 seconds"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
        match adapter.get_state().await {
            Ok(_) => last_coherent_at = tokio::time::Instant::now(),
            Err(_) => break tokio::time::Instant::now(),
        }
    };

    let mut failed_attempt_offsets_ms = Vec::new();
    let recovery_deadline = first_failed_observation_at + Duration::from_secs(60);
    loop {
        assert!(
            tokio::time::Instant::now() < recovery_deadline,
            "no coherent daemon recovery was observed within 60 seconds"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
        match adapter.connect().await {
            Ok(()) => break,
            Err(_) => failed_attempt_offsets_ms.push(
                tokio::time::Instant::now()
                    .saturating_duration_since(first_failed_observation_at)
                    .as_millis(),
            ),
        }
    }

    let coherent_recovery_at = tokio::time::Instant::now();
    let outage_onset_uncertainty =
        first_failed_observation_at.saturating_duration_since(last_coherent_at);
    let recovery_lower_bound =
        coherent_recovery_at.saturating_duration_since(first_failed_observation_at);
    let recovery_upper_bound = coherent_recovery_at.saturating_duration_since(last_coherent_at);
    let recovered = adapter
        .get_status()
        .await
        .info
        .expect("replacement coherent handshake carries daemon identity");
    println!(
        "HQP_LIFECYCLE_MEASUREMENT={}",
        serde_json::json!({
            "schema": "uhc-hqp-lifecycle/v1",
            "product": recovered.product,
            "version": recovered.version,
            "engine": recovered.engine,
            // The actual outage began between these two completed observations.
            "last_coherent_to_first_failed_observation_ms":
                outage_onset_uncertainty.as_millis(),
            "first_failed_observation_to_coherent_recovery_ms":
                recovery_lower_bound.as_millis(),
            // Policy qualification uses this conservative upper bound, not the lower bound above.
            "last_coherent_to_coherent_recovery_ms":
                recovery_upper_bound.as_millis(),
            "recovery_window_lower_bound_ms": recovery_lower_bound.as_millis(),
            "recovery_window_upper_bound_ms": recovery_upper_bound.as_millis(),
            "probe_interval_ms": PROBE_INTERVAL.as_millis(),
            "connect_timeout_ms": CONNECT_TIMEOUT.as_millis(),
            "response_timeout_ms": RESPONSE_TIMEOUT.as_millis(),
            "failed_attempt_offsets_ms": failed_attempt_offsets_ms,
        })
    );
}
