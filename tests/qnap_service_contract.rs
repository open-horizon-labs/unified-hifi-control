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
