//! Safety contract for the privileged QNAP service wrapper.

const SERVICE: &str = include_str!("../build/qnap/shared/unified-hifi-control.sh");

#[test]
fn qnap_service_keeps_credentials_private_and_stops_only_its_recorded_process() {
    assert!(
        SERVICE.contains("umask 077"),
        "HQPlayer credentials and persistent backups must not inherit a permissive NAS umask"
    );
    assert!(
        !SERVICE.contains("PORT_PID="),
        "the package must never kill an unrelated process merely because it owns port 8088"
    );
    assert!(
        !SERVICE.contains("pkill -9"),
        "the package must not use a broad force-kill fallback"
    );
}

#[test]
fn qnap_service_refuses_a_stale_pid_reused_by_an_unrelated_process() {
    let stop = SERVICE
        .split("  stop)")
        .nth(1)
        .and_then(|tail| tail.split("  restart)").next())
        .expect("service wrapper has a stop arm");
    assert!(
        SERVICE.contains("readlink -f \"/proc/${PID_TO_CHECK}/exe\""),
        "a live numeric PID is insufficient; the wrapper must verify /proc identity"
    );

    let graceful = stop
        .find("kill \"$PID\"")
        .expect("stop arm sends a graceful signal");
    let first_identity = stop[..graceful]
        .rfind("is_our_pid \"$PID\"")
        .expect("identity is checked before the graceful signal");
    assert!(first_identity < graceful);

    let force = stop
        .find("kill -9 \"$PID\"")
        .expect("stop arm has a bounded force fallback");
    let second_identity = stop[..force]
        .rfind("is_our_pid \"$PID\"")
        .expect("identity is checked again before the force signal");
    assert!(
        second_identity > graceful,
        "PID identity must be rechecked after the grace period because the PID can be reused"
    );
}
