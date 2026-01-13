//! Unified Hi-Fi Control - Rust Implementation
//!
//! A source-agnostic hi-fi control bridge for hardware surfaces and Home Assistant.

use unified_hifi_control::{adapters, api, bus, config, knobs, ui};

use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tower_http::{compression::CompressionLayer, cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "unified_hifi_control=debug,tower_http=debug,roon_api=info".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!(
        "Starting Unified Hi-Fi Control (Rust) v{}",
        env!("CARGO_PKG_VERSION")
    );

    // Load configuration
    let config = config::load_config()?;
    tracing::info!("Configuration loaded, port: {}", config.port);

    // Create event bus
    let bus = bus::create_bus();
    tracing::info!("Event bus initialized");

    // Initialize Roon adapter
    let roon = adapters::roon::RoonAdapter::new(bus.clone()).await?;
    tracing::info!("Roon adapter initialized");

    // Initialize HQPlayer adapter
    let hqplayer = Arc::new(adapters::hqplayer::HqpAdapter::new(bus.clone()));
    if let Some(ref hqp_config) = config.hqplayer {
        hqplayer
            .configure(
                hqp_config.host.clone(),
                Some(hqp_config.port),
                None,
                hqp_config.username.clone(),
                hqp_config.password.clone(),
            )
            .await;
        tracing::info!("HQPlayer adapter configured for {}", hqp_config.host);
    }

    // Initialize LMS adapter
    let lms = Arc::new(adapters::lms::LmsAdapter::new(bus.clone()));
    if let Some(ref lms_config) = config.lms {
        lms.configure(
            lms_config.host.clone(),
            Some(lms_config.port),
            lms_config.username.clone(),
            lms_config.password.clone(),
        )
        .await;

        // Start LMS polling
        if let Err(e) = lms.start().await {
            tracing::warn!("Failed to start LMS adapter: {}", e);
        } else {
            tracing::info!("LMS adapter started for {}", lms_config.host);
        }
    }

    // Initialize MQTT adapter
    let mqtt = Arc::new(adapters::mqtt::MqttAdapter::new(bus.clone()));
    if let Some(ref mqtt_config) = config.mqtt {
        mqtt.configure(
            mqtt_config.host.clone(),
            Some(mqtt_config.port),
            mqtt_config.username.clone(),
            mqtt_config.password.clone(),
            mqtt_config.topic_prefix.clone(),
        )
        .await;

        if let Err(e) = mqtt.start().await {
            tracing::warn!("Failed to start MQTT adapter: {}", e);
        } else {
            tracing::info!("MQTT adapter started for {}", mqtt_config.host);
        }
    }

    // Initialize OpenHome adapter (SSDP discovery)
    let openhome = Arc::new(adapters::openhome::OpenHomeAdapter::new(bus.clone()));
    if let Err(e) = openhome.start().await {
        tracing::warn!("Failed to start OpenHome adapter: {}", e);
    } else {
        tracing::info!("OpenHome adapter started (SSDP discovery active)");
    }

    // Initialize UPnP adapter (SSDP discovery)
    let upnp = Arc::new(adapters::upnp::UPnPAdapter::new(bus.clone()));
    if let Err(e) = upnp.start().await {
        tracing::warn!("Failed to start UPnP adapter: {}", e);
    } else {
        tracing::info!("UPnP adapter started (SSDP discovery active)");
    }

    // Initialize Knob device store
    let data_dir = config::get_data_dir();
    let knob_store = knobs::KnobStore::new(data_dir);
    tracing::info!("Knob store initialized");

    // Build application state (clone Arcs so we can access adapters for shutdown)
    let state = api::AppState::new(
        roon,
        hqplayer,
        lms.clone(),
        mqtt.clone(),
        openhome.clone(),
        upnp.clone(),
        knob_store,
        bus.clone(),
    );

    // Build API routes
    let app = Router::new()
        // Health check
        .route("/status", get(api::status_handler))
        // Roon routes
        .route("/roon/status", get(api::roon_status_handler))
        .route("/roon/zones", get(api::roon_zones_handler))
        .route("/roon/zone/:zone_id", get(api::roon_zone_handler))
        .route("/roon/control", post(api::roon_control_handler))
        .route("/roon/volume", post(api::roon_volume_handler))
        // HQPlayer routes
        .route("/hqplayer/status", get(api::hqp_status_handler))
        .route("/hqplayer/pipeline", get(api::hqp_pipeline_handler))
        .route("/hqplayer/control", post(api::hqp_control_handler))
        .route("/hqplayer/volume", post(api::hqp_volume_handler))
        .route("/hqplayer/setting", post(api::hqp_setting_handler))
        .route("/hqplayer/profiles", get(api::hqp_profiles_handler))
        .route("/hqplayer/profile", post(api::hqp_load_profile_handler))
        // HQPlayer Matrix profile routes
        .route(
            "/hqplayer/matrix/profiles",
            get(api::hqp_matrix_profiles_handler),
        )
        .route(
            "/hqplayer/matrix/profile",
            post(api::hqp_set_matrix_profile_handler),
        )
        // LMS routes
        .route("/lms/status", get(api::lms_status_handler))
        .route("/lms/players", get(api::lms_players_handler))
        .route("/lms/player/:player_id", get(api::lms_player_handler))
        .route("/lms/control", post(api::lms_control_handler))
        .route("/lms/volume", post(api::lms_volume_handler))
        // OpenHome routes
        .route("/openhome/status", get(api::openhome_status_handler))
        .route("/openhome/zones", get(api::openhome_zones_handler))
        .route(
            "/openhome/zone/:zone_id/now_playing",
            get(api::openhome_now_playing_handler),
        )
        .route("/openhome/control", post(api::openhome_control_handler))
        // UPnP routes
        .route("/upnp/status", get(api::upnp_status_handler))
        .route("/upnp/zones", get(api::upnp_zones_handler))
        .route(
            "/upnp/zone/:zone_id/now_playing",
            get(api::upnp_now_playing_handler),
        )
        .route("/upnp/control", post(api::upnp_control_handler))
        // Event stream (SSE)
        .route("/events", get(api::events_handler))
        // Knob hardware API routes
        .route("/knob/zones", get(knobs::knob_zones_handler))
        .route("/knob/now_playing", get(knobs::knob_now_playing_handler))
        .route("/knob/now_playing/image", get(knobs::knob_image_handler))
        .route("/knob/control", post(knobs::knob_control_handler))
        .route("/knob/config", get(knobs::knob_config_handler))
        .route("/knob/config", post(knobs::knob_config_update_handler))
        .route("/knob/devices", get(knobs::knob_devices_handler))
        // Web UI routes
        .route("/", get(ui::dashboard_page))
        .route("/zones", get(ui::zones_page))
        .route("/hqplayer", get(ui::hqplayer_page))
        .route("/lms", get(ui::lms_page))
        // Legacy redirects
        .route("/control", get(ui::control_redirect))
        .route("/settings", get(ui::settings_redirect))
        // Middleware
        .layer(CorsLayer::permissive())
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Start server with graceful shutdown
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Cleanup: stop adapters
    tracing::info!("Shutting down adapters...");
    lms.stop().await;
    mqtt.stop().await;
    openhome.stop().await;
    upnp.stop().await;
    tracing::info!("Shutdown complete");

    Ok(())
}

/// Wait for shutdown signal (Ctrl+C or SIGTERM)
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("Received Ctrl+C, shutting down..."),
        _ = terminate => tracing::info!("Received SIGTERM, shutting down..."),
    }
}
