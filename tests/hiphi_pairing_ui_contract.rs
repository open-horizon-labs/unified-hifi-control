//! Browser onboarding contract for the packaged HiPhi connector.
//!
//! A QPKG that merely contains `uhc-hiphi-pair` still strands an owner who
//! installed it through App Center. These checks keep the three-step local
//! ceremony reachable from Settings while preserving the private-key boundary.

const MAIN: &str = include_str!("../src/main.rs");
const API: &str = include_str!("../src/api/hiphi_pairing.rs");
const SETTINGS: &str = include_str!("../src/app/pages/settings.rs");

#[test]
fn settings_exposes_the_complete_local_pairing_ceremony() {
    for route in [
        "/api/hiphi/pairing/prepare",
        "/api/hiphi/pairing/initiate",
        "/api/hiphi/pairing/complete",
    ] {
        assert!(MAIN.contains(route), "server must mount {route}");
        assert!(SETTINGS.contains(route), "Settings must call {route}");
    }

    for copy in [
        "Connect HiPhi Cloud",
        "Installation public key",
        "Choose enrollment file",
        "Pairing fingerprint",
        "Complete pairing",
        "https://hiphi.audio/#cloud",
        "Why sign up?",
    ] {
        assert!(SETTINGS.contains(copy), "missing onboarding copy: {copy}");
    }
}

#[test]
fn browser_contract_never_serializes_the_private_installation_key() {
    assert!(!API.contains("installation_private_key"));
    assert!(!API.contains("private_key"));
    assert!(API.contains("installation_public_key"));
    assert!(API.contains("installation_fingerprint"));
}

#[test]
fn pairing_routes_are_owner_gated() {
    let auth = include_str!("../src/api/controller_auth.rs");
    assert!(auth.contains("path.starts_with(\"/api/hiphi/pairing/\")"));
}

#[test]
fn pairing_origin_is_server_pinned() {
    assert!(API.contains("https://relay.hiphi.audio"));
    assert!(!API.contains("request.cloud_origin"));
}

#[test]
fn browser_handoff_explicitly_clears_inherited_group_and_other_permissions() {
    assert!(
        API.contains("file.set_permissions"),
        "the QNAP config volume may inherit ACL mode bits despite create mode 0600"
    );
}
