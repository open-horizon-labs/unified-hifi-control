//! The server must be able to build a rustls client config. If two crypto providers are compiled
//! in (ring and aws-lc-rs) and none is installed, rustls panics on the first TLS connection —
//! which took the server down in production after `rumqttc` 0.25's default features added
//! aws-lc-rs (beta.6 candidate, #815). These tests fail on that build.
#![cfg(feature = "server")]

#[test]
fn a_tls_client_config_builds_with_the_default_provider() {
    // Panics with "Could not automatically determine the process-level CryptoProvider" when the
    // provider is ambiguous and none was installed.
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    drop(config);
}

#[test]
fn startup_installs_ring_as_the_process_provider() {
    unified_hifi_control::tls::install_crypto_provider();
    let provider = rustls::crypto::CryptoProvider::get_default().expect("a provider is installed");
    let ring = rustls::crypto::ring::default_provider();
    assert_eq!(
        provider.cipher_suites.len(),
        ring.cipher_suites.len(),
        "the installed provider is ring's"
    );
    // Idempotent: a second call (another entry point) must not panic.
    unified_hifi_control::tls::install_crypto_provider();
}
