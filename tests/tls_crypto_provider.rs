//! The server must be able to build a rustls client config. If two crypto providers are compiled
//! in (ring and aws-lc-rs) and none is installed, rustls panics on the first TLS connection —
//! which took the server down in production (#815). Reqwest 0.13 compiles its AWS-LC fallback,
//! so startup must explicitly select ring rather than relying on feature autodetection.
#![cfg(feature = "server")]

#[test]
fn tls_clients_build_after_startup_provider_selection() {
    unified_hifi_control::tls::install_crypto_provider();
    // Exercise both direct rustls consumers and the current reqwest backend.
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    drop(config);
    reqwest::Client::builder()
        .build()
        .expect("HTTP TLS client builds after startup selection");
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

#[test]
fn every_tls_entrypoint_selects_the_provider_before_application_setup() {
    for source in [
        include_str!("../src/main.rs"),
        include_str!("../src/bin/uhc_hiphi_pair.rs"),
        include_str!("../src/bin/uhc_music_details.rs"),
    ] {
        let file = syn::parse_file(source).expect("entrypoint parses");
        let main = file
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Fn(function)
                    if function.sig.ident == "main" && function.sig.asyncness.is_some() =>
                {
                    Some(function)
                }
                _ => None,
            })
            .expect("async entrypoint");
        let Some(syn::Stmt::Expr(syn::Expr::Call(call), _)) = main.block.stmts.first() else {
            panic!("TLS provider must be selected before application setup");
        };
        let syn::Expr::Path(function) = call.func.as_ref() else {
            panic!("provider setup call")
        };
        let path: Vec<_> = function
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        assert_eq!(
            path,
            ["unified_hifi_control", "tls", "install_crypto_provider"]
        );
    }
}
