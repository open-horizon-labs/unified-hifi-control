//! Software-fixture evidence only: real HQPlayer Embedded reconnect is a separate gate.
#[test]
fn adversarial_loopback_router_suite() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let suite = manifest.join("../../tools/naa_router_lab.py");
    let output = std::process::Command::new("python3")
        .arg(suite)
        .arg("--binary")
        .arg(env!("CARGO_BIN_EXE_naa-router"))
        .output()
        .expect("python3 is required for the loopback router lab");
    assert!(
        output.status.success(),
        "loopback router lab failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
