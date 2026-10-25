//! Process-wide TLS setup.

/// Install ring as the process-wide rustls crypto provider.
///
/// Every TLS client in UHC (reqwest, tokio-tungstenite, MQTT via rumqttc) uses rustls 0.23. If a
/// dependency ever compiles in a second provider (aws-lc-rs) and none is installed, rustls cannot
/// choose and panics on the first TLS connection — which took the server down in production. Call
/// this once at startup in each binary, before any TLS client is built. Reqwest 0.13 also
/// compiles an AWS-LC fallback; explicit selection keeps every runtime client on ring.
/// Safe to call more than once.
pub fn install_crypto_provider() {
    // `Err` means a provider is already installed (an earlier call, or a test harness); keep it.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
