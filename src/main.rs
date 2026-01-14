//! Unified Hi-Fi Control - Rust Implementation
//!
//! A source-agnostic hi-fi control bridge for hardware surfaces and Home Assistant.

use unified_hifi_control::{adapters, api, bus, config, firmware, knobs, mdns, ui};

use anyhow::Result;
use axum::{
    routing::{delete, get, post, put},
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

    // Migrate Node.js config files if present (seamless Docker image swap)
    config::migrate_nodejs_configs();

    // Create event bus
    let bus = bus::create_bus();
    tracing::info!("Event bus initialized");

    // Construct base URL for display in Roon and mDNS
    let base_url = format!(
        "http://{}:{}",
        gethostname::gethostname().to_string_lossy(),
        config.port
    );

    // Initialize Roon adapter (optional - service continues without Roon)
    let roon = match adapters::roon::RoonAdapter::new(bus.clone(), base_url.clone()).await {
        Ok(adapter) => {
            tracing::info!("Roon adapter initialized");
            adapter
        }
        Err(e) => {
            tracing::warn!(
                "Failed to initialize Roon adapter: {}. Continuing without Roon.",
                e
            );
            adapters::roon::RoonAdapter::new_disconnected(bus.clone())
        }
    };

    // Initialize HQPlayer instance manager (multi-instance support)
    let hqp_instances = Arc::new(adapters::hqplayer::HqpInstanceManager::new(bus.clone()));
    hqp_instances.load_from_config().await;
    let instance_count = hqp_instances.instance_count().await;
    if instance_count > 0 {
        tracing::info!(
            "HQPlayer: {} instance(s) loaded from config",
            instance_count
        );
    }

    // Create default HQPlayer adapter for backward compatibility
    let hqplayer = hqp_instances.get_default().await;
    // Config file takes precedence over persisted disk config
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
        hqp_instances.save_to_config().await;
        tracing::info!(
            "HQPlayer default instance configured for {}",
            hqp_config.host
        );
    } else if hqplayer.is_configured().await {
        let status = hqplayer.get_status().await;
        if let Some(host) = status.host {
            tracing::info!("HQPlayer default instance: {}:{}", host, status.port);
        }
    }

    // Initialize HQP zone link service
    let hqp_zone_links = Arc::new(adapters::hqplayer::HqpZoneLinkService::new(
        hqp_instances.clone(),
    ));
    hqp_zone_links.auto_correct_links().await;
    let link_count = hqp_zone_links.get_links().await.len();
    if link_count > 0 {
        tracing::info!("HQPlayer: {} zone link(s) active", link_count);
    }

    // Initialize LMS adapter (may have config loaded from disk)
    let lms = Arc::new(adapters::lms::LmsAdapter::new(bus.clone()));
    // Config file takes precedence over persisted disk config
    if let Some(ref lms_config) = config.lms {
        lms.configure(
            lms_config.host.clone(),
            Some(lms_config.port),
            lms_config.username.clone(),
            lms_config.password.clone(),
        )
        .await;
    }
    // Start LMS if configured (from config file or persisted disk config)
    if lms.is_configured().await {
        if let Err(e) = lms.start().await {
            tracing::warn!("Failed to start LMS adapter: {}", e);
        } else {
            let status = lms.get_status().await;
            if let Some(host) = status.host {
                tracing::info!("LMS adapter started for {}:{}", host, status.port);
            }
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
        hqp_instances,
        hqp_zone_links,
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
        .route("/roon/zone/{zone_id}", get(api::roon_zone_handler))
        .route("/roon/control", post(api::roon_control_handler))
        .route("/roon/volume", post(api::roon_volume_handler))
        .route("/roon/image", get(api::roon_image_handler))
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
        // HQPlayer config routes
        .route("/hqplayer/config", get(api::hqp_config_handler))
        .route("/hqplayer/configure", post(api::hqp_configure_handler))
        .route("/hqp/detect", post(api::hqp_detect_handler))
        // HQPlayer multi-instance routes
        .route("/hqp/instances", get(api::hqp_instances_handler))
        .route("/hqp/instances", post(api::hqp_add_instance_handler))
        .route(
            "/hqp/instances/{name}",
            delete(api::hqp_remove_instance_handler),
        )
        // HQPlayer instance-specific profile routes (web UI profiles via HTTP)
        .route(
            "/hqp/instances/{name}/profiles",
            get(api::hqp_instance_profiles_handler),
        )
        .route(
            "/hqp/instances/{name}/profile",
            post(api::hqp_instance_load_profile_handler),
        )
        // HQPlayer instance-specific matrix profile routes (native TCP protocol)
        .route(
            "/hqp/instances/{name}/matrix/profiles",
            get(api::hqp_instance_matrix_profiles_handler),
        )
        .route(
            "/hqp/instances/{name}/matrix/profile",
            post(api::hqp_instance_set_matrix_profile_handler),
        )
        // HQPlayer zone linking routes
        .route("/hqp/zones/links", get(api::hqp_zone_links_handler))
        .route("/hqp/zones/link", post(api::hqp_zone_link_handler))
        .route("/hqp/zones/unlink", post(api::hqp_zone_unlink_handler))
        .route(
            "/hqp/zones/{zone_id}/pipeline",
            get(api::hqp_zone_pipeline_handler),
        )
        // HQPlayer network discovery
        .route("/hqp/discover", get(api::hqp_discover_handler))
        // LMS routes
        .route("/lms/status", get(api::lms_status_handler))
        .route("/lms/config", get(api::lms_config_handler))
        .route("/lms/configure", post(api::lms_configure_handler))
        .route("/lms/players", get(api::lms_players_handler))
        .route("/lms/player/{player_id}", get(api::lms_player_handler))
        .route("/lms/control", post(api::lms_control_handler))
        .route("/lms/volume", post(api::lms_volume_handler))
        // OpenHome routes
        .route("/openhome/status", get(api::openhome_status_handler))
        .route("/openhome/zones", get(api::openhome_zones_handler))
        .route(
            "/openhome/zone/{zone_id}/now_playing",
            get(api::openhome_now_playing_handler),
        )
        .route("/openhome/control", post(api::openhome_control_handler))
        // UPnP routes
        .route("/upnp/status", get(api::upnp_status_handler))
        .route("/upnp/zones", get(api::upnp_zones_handler))
        .route(
            "/upnp/zone/{zone_id}/now_playing",
            get(api::upnp_now_playing_handler),
        )
        .route("/upnp/control", post(api::upnp_control_handler))
        // App settings API
        .route("/api/settings", get(api::api_settings_get_handler))
        .route("/api/settings", post(api::api_settings_post_handler))
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
        // Knob protocol routes (firmware uses these paths directly)
        .route("/now_playing", get(knobs::knob_now_playing_handler))
        .route("/now_playing/image", get(knobs::knob_image_handler))
        .route("/control", post(knobs::knob_control_handler))
        .route("/config/{knob_id}", get(knobs::knob_config_by_path_handler))
        .route(
            "/config/{knob_id}",
            put(knobs::knob_config_update_by_path_handler),
        )
        // Firmware OTA routes
        .route("/firmware/version", get(knobs::firmware_version_handler))
        .route("/firmware/download", get(knobs::firmware_download_handler))
        .route("/manifest-s3.json", get(knobs::manifest_handler))
        .route(
            "/admin/fetch-firmware",
            post(knobs::admin_fetch_firmware_handler),
        )
        // Protocol route: /zones returns JSON (for knob, iOS, etc.)
        .route("/zones", get(knobs::knob_zones_handler))
        // Web UI routes
        .route("/", get(ui::dashboard_page))
        .route("/ui/zones", get(ui::zones_page))
        .route("/zone", get(ui::zone_page))
        .route("/critical", get(ui::zone_page))
        .route("/admin/critical", get(ui::zone_page))
        .route("/hqplayer", get(ui::hqplayer_page))
        .route("/lms", get(ui::lms_page))
        .route("/knobs", get(ui::knobs_page))
        .route("/knobs/flash", get(ui::flash_page))
        .route("/settings", get(ui::settings_page))
        // Legacy redirects
        .route("/control", get(ui::control_redirect))
        .route("/admin", get(ui::settings_redirect))
        // Middleware
        .layer(CorsLayer::permissive())
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Start server with graceful shutdown
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("Listening on http://{}", addr);

    // Advertise via mDNS for knob discovery
    let _mdns = match mdns::advertise(config.port, "Unified Hi-Fi Control", &base_url) {
        Ok(daemon) => {
            tracing::info!("mDNS advertising started");
            Some(daemon)
        }
        Err(e) => {
            tracing::warn!("Failed to start mDNS advertising: {}", e);
            None
        }
    };

    // Start firmware auto-update service
    let firmware_auto_update = std::env::var("FIRMWARE_AUTO_UPDATE")
        .map(|v| v != "false")
        .unwrap_or(true);
    if firmware_auto_update {
        let poll_interval = std::env::var("FIRMWARE_POLL_INTERVAL_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        let firmware_service = std::sync::Arc::new(firmware::FirmwareService::new());
        firmware_service.start_polling(poll_interval);
        tracing::info!(
            "Firmware auto-update enabled (poll interval: {} min)",
            poll_interval
        );
    } else {
        tracing::info!("Firmware auto-update disabled");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Cleanup: stop adapters (mDNS daemon will be dropped automatically)
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
