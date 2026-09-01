use std::{sync::Arc, time::Instant};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use unified_hifi_control::{
    adapters::{
        hqplayer::{HqpInstanceManager, HqpZoneLinkService},
        lms::LmsAdapter,
        musicassistant::MusicAssistantAdapter,
        openhome::OpenHomeAdapter,
        roon::RoonAdapter,
        upnp::UPnPAdapter,
        Startable,
    },
    aggregator::ZoneAggregator,
    api::{
        self,
        credentials::{MusicAssistantCredentialRecord, MusicAssistantCredentialStore},
        provider_auth::ProviderAuthState,
        AppState,
    },
    bus::create_bus,
    coordinator::AdapterCoordinator,
    knobs::KnobStore,
};

async fn state_with_store(store: MusicAssistantCredentialStore) -> AppState {
    let bus = create_bus();
    let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));
    let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
    let instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = instances.get_default().await;
    let links = Arc::new(HqpZoneLinkService::new(instances.clone()));
    let lms = Arc::new(LmsAdapter::new(bus.clone()));
    let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
    let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
    let startables: Vec<Arc<dyn Startable>> =
        vec![roon.clone(), lms.clone(), openhome.clone(), upnp.clone()];
    let mut state = AppState::new(
        roon,
        hqplayer,
        instances,
        links,
        lms,
        openhome,
        upnp,
        KnobStore::new(),
        bus,
        Arc::new(ZoneAggregator::new(create_bus())),
        coordinator,
        startables,
        Instant::now(),
        CancellationToken::new(),
    );
    state.provider_auth = Arc::new(ProviderAuthState::with_musicassistant_credential_store(
        store,
    ));
    state
        .provider_auth
        .attach_musicassistant(state.musicassistant.clone())
        .await;
    state
}

async fn good_api() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/api", post(|| async { Json(json!([])) })),
        )
        .await
        .unwrap();
    });
    (port, handle)
}

async fn failing_api() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/api",
                post(|| async { (StatusCode::UNAUTHORIZED, "token-reflection-secret") }),
            ),
        )
        .await
        .unwrap();
    });
    (port, handle)
}

#[tokio::test]
async fn rejected_configure_preserves_existing_runtime_and_redacts_secrets_from_status() {
    let directory = tempdir().unwrap();
    let store = MusicAssistantCredentialStore::new(directory.path().join("ma.enc"), [7; 32]);
    let (old_port, old_server) = good_api().await;
    let old = MusicAssistantCredentialRecord {
        host: "127.0.0.1".into(),
        port: old_port,
        token: "old-private-token".into(),
        tls: false,
        allow_insecure_http: true,
    };
    store.save(&old).unwrap();
    let state = state_with_store(store.clone()).await;
    let old_adapter = Arc::new(
        MusicAssistantAdapter::new(
            state.bus.clone(),
            unified_hifi_control::adapters::musicassistant::MusicAssistantConfig {
                host: old.host.clone(),
                port: old.port,
                token: old.token.clone(),
                tls: false,
                allow_insecure_http: true,
            },
        )
        .unwrap(),
    );
    state
        .musicassistant
        .install(
            old_adapter,
            unified_hifi_control::adapters::musicassistant::MusicAssistantConfig {
                host: old.host.clone(),
                port: old.port,
                token: old.token.clone(),
                tls: false,
                allow_insecure_http: true,
            },
        )
        .await
        .unwrap();
    let (bad_port, bad_server) = failing_api().await;
    let router = Router::new()
        .route(
            "/api/providers/{provider}/configure",
            post(api::provider_auth::configure_provider),
        )
        .route(
            "/api/providers/musicassistant/status",
            get(api::provider_auth::musicassistant_status),
        )
        .with_state(state.clone());
    let response = router.clone().oneshot(Request::post("/api/providers/musicassistant/configure").header("content-type", "application/json").body(Body::from(json!({"host":"127.0.0.1","port":bad_port,"token":"new-private-token","tls":false,"allow_insecure_http":true}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let saved = store.load().unwrap().unwrap();
    assert_eq!(saved.host, "127.0.0.1");
    assert_eq!(saved.port, old_port);
    assert_eq!(saved.token, "old-private-token");
    let status = router
        .oneshot(
            Request::get("/api/providers/musicassistant/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(status.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["endpoint"]["port"], old_port);
    let rendered = body.to_string();
    assert!(!rendered.contains("old-private-token"));
    assert!(!rendered.contains("new-private-token"));
    assert!(!rendered.contains("token-reflection-secret"));
    old_server.abort();
    bad_server.abort();
}
