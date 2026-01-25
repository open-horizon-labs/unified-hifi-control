//! Unified Hi-Fi Control - Rust Implementation
//!
//! A source-agnostic hi-fi control bridge for hardware surfaces and Home Assistant.

// Server-only: full server implementation
#[cfg(feature = "server")]
mod server {
    use unified_hifi_control::{
        adapters, aggregator, api, app, bus, config, coordinator, embedded, firmware, knobs, mdns,
    };

    // Import Startable trait for adapter lifecycle methods
    use adapters::Startable;

    // Import load_app_settings for checking adapter enabled state
    use api::load_app_settings;

    use anyhow::Result;
    use axum::{
        response::{Html, IntoResponse, Redirect},
        routing::{delete, get, post, put},
        Router,
    };
    use dioxus::prelude::DioxusRouterExt;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::signal;
    use tokio_util::sync::CancellationToken;
    use tower_http::{compression::CompressionLayer, cors::CorsLayer, trace::TraceLayer};
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    /// Flash page - redirects to external web flasher
    async fn flash_page() -> impl IntoResponse {
        Html(
            r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Flash Knob - Unified Hi-Fi Control</title>
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@picocss/pico@2/css/pico.min.css">
</head>
<body class="container">
    <h1>Flash Knob Firmware</h1>
    <article>
        <p><strong>HTTPS Required</strong></p>
        <p>Browser-based flashing requires HTTPS. Use the official web flasher:</p>
        <p><a href="https://roon-knob.muness.com/" target="_blank" rel="noopener" role="button">Open Web Flasher</a></p>
    </article>
</body>
</html>"#,
        )
    }

    /// Legacy redirect: /control -> /ui/zones
    async fn control_redirect() -> impl IntoResponse {
        Redirect::to("/ui/zones")
    }

    /// Legacy redirect: /admin -> /settings
    async fn settings_redirect() -> impl IntoResponse {
        Redirect::to("/settings")
    }

    pub async fn run() -> Result<()> {
        // Initialize logging
        // Priority: RUST_LOG > LOG_LEVEL (legacy) > default
        let log_filter = std::env::var("RUST_LOG")
            .or_else(|_| std::env::var("LOG_LEVEL"))
            .unwrap_or_else(|_| "unified_hifi_control=debug,tower_http=debug,roon_api=info".into());

        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(&log_filter))
            .with(tracing_subscriber::fmt::layer())
            .init();

        tracing::info!(
            "Starting Unified Hi-Fi Control (Rust) v{} ({})",
            env!("UHC_VERSION"),
            env!("UHC_GIT_SHA")
        );

        // Log embedded assets status (ADR 002)
        if embedded::has_embedded_assets() {
            let assets = embedded::list_embedded_assets();
            tracing::info!(
                "Embedded WASM assets: {} files (single-binary mode)",
                assets.len()
            );
            tracing::debug!("Embedded files: {:?}", assets);
        } else {
            tracing::info!("No embedded WASM assets (development mode, use dx serve)");
        }

        // Load configuration
        let config = config::load_config()?;
        tracing::info!("Configuration loaded, port: {}", config.port);

        // Issue #76: Migrate config files to unified-hifi/ subdirectory
        config::migrate_config_to_subdir();

        // Migrate Node.js config files if present (seamless Docker image swap)
        config::migrate_nodejs_configs();

        // Create event bus
        let bus = bus::create_bus();
        tracing::info!("Event bus initialized");

        // Load app settings and create adapter coordinator (single source of truth for lifecycle)
        let app_settings = load_app_settings();
        let coord = Arc::new(coordinator::AdapterCoordinator::new(bus.clone()));
        coord.register_from_settings(&app_settings.adapters).await;
        tracing::info!("Adapter coordinator initialized");

        // Construct base URL for display in Roon and mDNS
        let base_url = format!(
            "http://{}:{}",
            gethostname::gethostname().to_string_lossy(),
            config.port
        );

        // =========================================================================
        // Create all adapter instances (needed for API handlers regardless of state)
        // =========================================================================

        // Initialize Knob device store early (needed for Roon extension status)
        // Issue #76: Uses config subdirectory for knobs.json
        let knob_store = knobs::KnobStore::new();
        tracing::info!("Knob store initialized");

        // Roon adapter - coordinator handles starting based on enabled state
        // Issue #169: Pass knob_store for controller count in extension status
        let roon = Arc::new(adapters::roon::RoonAdapter::new_configured(
            bus.clone(),
            base_url.clone(),
            knob_store.clone(),
        ));

        // HQPlayer instance manager (multi-instance support, no settings toggle)
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

        // Auto-connect HQPlayer if configured (establishes TCP connection at startup)
        if hqplayer.is_configured().await {
            match hqplayer.get_pipeline_status().await {
                Ok(_) => tracing::info!("HQPlayer auto-connected at startup"),
                Err(e) => tracing::warn!(
                    "HQPlayer auto-connect failed (will retry on page access): {}",
                    e
                ),
            }
        }

        // HQP zone link service
        let hqp_zone_links = Arc::new(adapters::hqplayer::HqpZoneLinkService::new(
            hqp_instances.clone(),
        ));
        hqp_zone_links.auto_correct_links().await;
        let link_count = hqp_zone_links.get_links().await.len();
        if link_count > 0 {
            tracing::info!("HQPlayer: {} zone link(s) active", link_count);
        }

        // LMS adapters (polling + CLI subscription with shared state)
        // Issue #165: Split into two adapters with independent retry
        let (lms, lms_cli) = adapters::lms::create_lms_adapters(bus.clone());
        if let Some(ref lms_config) = config.lms {
            lms.configure(
                lms_config.host.clone(),
                Some(lms_config.port),
                lms_config.username.clone(),
                lms_config.password.clone(),
            )
            .await;
        }

        // OpenHome adapter
        let openhome = Arc::new(adapters::openhome::OpenHomeAdapter::new(bus.clone()));

        // UPnP adapter
        let upnp = Arc::new(adapters::upnp::UPnPAdapter::new(bus.clone()));

        // =========================================================================
        // Start enabled adapters (single codepath using coordinator)
        // =========================================================================

        // Build list of startable adapters
        // Note: lms_cli shares config with lms - both start when LMS is configured
        let startable_adapters: Vec<Arc<dyn adapters::Startable>> = vec![
            roon.clone(),
            lms.clone(),
            lms_cli.clone(),
            openhome.clone(),
            upnp.clone(),
        ];

        // Single loop to start all enabled adapters
        coord.start_all_enabled(&startable_adapters).await;

        // Initialize ZoneAggregator for unified zone state
        let zone_aggregator = Arc::new(aggregator::ZoneAggregator::new(bus.clone()));
        let aggregator_for_spawn = zone_aggregator.clone();
        tokio::spawn(async move {
            aggregator_for_spawn.run().await;
        });
        tracing::info!("ZoneAggregator started");

        // Clone Roon adapter for shutdown access (cheap - just Arc clones)
        let roon_for_shutdown = roon.clone();

        // Create shutdown token for graceful SSE termination (fixes #73)
        let shutdown_token = CancellationToken::new();

        // Build application state (clone Arcs so we can access adapters for shutdown)
        let state = api::AppState::new(
            roon,
            hqplayer,
            hqp_instances,
            hqp_zone_links,
            lms.clone(),
            openhome.clone(),
            upnp.clone(),
            knob_store,
            bus.clone(),
            zone_aggregator,
            coord.clone(),
            startable_adapters.clone(),
            Instant::now(),
            shutdown_token.clone(),
        );

        // Clone state for shutdown diagnostics
        let state_for_shutdown = state.clone();

        // Build API routes
        let router = Router::new()
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
            // HQPlayer pipeline POST route (iOS compatible)
            .route("/hqp/pipeline", get(api::hqp_pipeline_handler))
            .route("/hqp/pipeline", post(api::hqp_pipeline_update_handler))
            // HQPlayer status route (iOS uses /hqp/status)
            .route("/hqp/status", get(api::hqp_status_handler))
            // HQPlayer profiles route (iOS uses /hqp/profiles)
            .route("/hqp/profiles", get(api::hqp_profiles_handler))
            .route("/hqp/profiles/load", post(api::hqp_load_profile_handler))
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
            .route("/lms/discover", get(api::lms_discover_handler))
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
            // Legacy SSR routes (flash page not yet migrated)
            .route("/knobs/flash", get(flash_page))
            // Legacy redirects
            .route("/control", get(control_redirect))
            .route("/admin", get(settings_redirect))
            // Embedded WASM/JS assets (ADR 002: serve from memory, no disk extraction)
            .route("/assets/{*path}", get(embedded::serve_embedded_asset))
            // Embedded static files (favicon, CSS, images)
            .route(
                "/favicon.ico",
                get(|| embedded::serve_static_file(axum::extract::Path("favicon.ico".to_string()))),
            )
            .route(
                "/apple-touch-icon.png",
                get(|| {
                    embedded::serve_static_file(axum::extract::Path(
                        "apple-touch-icon.png".to_string(),
                    ))
                }),
            )
            .route(
                "/tailwind.css",
                get(|| {
                    embedded::serve_static_file(axum::extract::Path("tailwind.css".to_string()))
                }),
            )
            .route(
                "/dx-components-theme.css",
                get(|| {
                    embedded::serve_static_file(axum::extract::Path(
                        "dx-components-theme.css".to_string(),
                    ))
                }),
            )
            // Middleware
            .layer(CorsLayer::permissive())
            .layer(CompressionLayer::new())
            .layer(TraceLayer::new_for_http())
            .with_state(state);

        // ADR 002: Embedded assets mode - SSR with injected bootstrap scripts
        // serve_api_application() provides SSR + server functions, but no static assets
        // Our middleware injects the bootstrap scripts (from embedded index.html) into SSR HTML
        // This enables WASM hydration without requiring a public/ directory at runtime
        let router = if embedded::has_embedded_assets() {
            if let Some(bootstrap) = embedded::extract_bootstrap_snippet() {
                tracing::info!("Using embedded SSR mode (bootstrap scripts will be injected)");
                tracing::debug!("Bootstrap snippet:\n{}", bootstrap);
                router
                    .serve_api_application(dioxus::server::ServeConfig::new(), app::App)
                    .layer(embedded::InjectDioxusBootstrapLayer::new(bootstrap))
            } else {
                tracing::warn!(
                    "Embedded assets found but no bootstrap scripts - falling back to SPA"
                );
                router
                    .serve_api_application(dioxus::server::ServeConfig::new(), app::App)
                    .fallback(embedded::serve_index_html)
            }
        } else {
            tracing::info!("Using SSR mode (no embedded assets, use dx serve for development)");
            // Standard SSR mode for development
            router.serve_dioxus_application(dioxus::server::ServeConfig::new(), app::App)
        };

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
        let firmware_service = if firmware_auto_update {
            let poll_interval = std::env::var("FIRMWARE_POLL_INTERVAL_MINUTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);
            let service = Arc::new(firmware::FirmwareService::new());
            service.clone().start_polling(poll_interval);
            tracing::info!(
                "Firmware auto-update enabled (poll interval: {} min)",
                poll_interval
            );
            Some(service)
        } else {
            tracing::info!("Firmware auto-update disabled");
            None
        };

        let listener = tokio::net::TcpListener::bind(addr).await?;

        // Create shutdown future that cancels token before graceful shutdown (fixes #73)
        let graceful_shutdown = {
            let token = shutdown_token.clone();
            let state = state_for_shutdown.clone();
            async move {
                shutdown_signal().await;

                // Cancel SSE streams BEFORE Axum starts waiting for connections
                token.cancel();

                // Log active SSE connections for diagnostics
                let active = state.active_sse_connections();
                if active > 0 {
                    tracing::info!(
                        "Cancelling {} active SSE connection(s) for graceful shutdown",
                        active
                    );
                }
            }
        };

        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(graceful_shutdown)
        .await?;

        // Cleanup: publish ShuttingDown event and stop adapters
        tracing::info!("Shutting down adapters...");

        // Publish ShuttingDown event for any bus listeners
        bus.publish(bus::BusEvent::ShuttingDown {
            reason: Some("User requested shutdown".to_string()),
        });

        // Give listeners a moment to react to ShuttingDown
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Stop adapters
        roon_for_shutdown.stop().await;
        if let Some(ref fw) = firmware_service {
            fw.stop();
        }
        lms.stop().await;
        openhome.stop().await;
        upnp.stop().await;
        tracing::info!("Shutdown complete");

        Ok(())
    }

    /// Wait for shutdown signal (Ctrl+C or SIGTERM)
    #[allow(clippy::expect_used)] // Signal handlers must succeed for graceful shutdown
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
}

// Server entry point
#[cfg(feature = "server")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Handle --version and --help before starting server
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!(
            "unified-hifi-control {} ({})",
            env!("UHC_VERSION"),
            env!("UHC_GIT_SHA")
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "unified-hifi-control {} ({})",
            env!("UHC_VERSION"),
            env!("UHC_GIT_SHA")
        );
        println!();
        println!(
            "Source-agnostic hi-fi control bridge for Roon, LMS, HQPlayer, and hardware knobs."
        );
        println!();
        println!("USAGE:");
        println!("    unified-hifi-control [OPTIONS]");
        println!();
        println!("OPTIONS:");
        println!("    -h, --help       Print help information");
        println!("    -V, --version    Print version information");
        println!();
        println!("ENVIRONMENT VARIABLES:");
        println!("    PORT             HTTP server port (default: 8088)");
        println!("    CONFIG_DIR       Configuration directory");
        println!("    LOG_LEVEL        Log level (debug, info, warn, error)");
        println!("    LMS_HOST         LMS server host (auto-enables LMS backend)");
        println!("    LMS_PORT         LMS server port (default: 9000)");
        return Ok(());
    }

    server::run().await
}

// WASM entry point (client-side only)
#[cfg(all(not(feature = "server"), target_arch = "wasm32"))]
fn main() {
    use unified_hifi_control::app;
    dioxus::launch(app::App);
}

// Fallback for other configurations
#[cfg(all(not(feature = "server"), not(target_arch = "wasm32")))]
fn main() {
    eprintln!("This binary requires either the 'server' feature or wasm32 target.");
    std::process::exit(1);
}
