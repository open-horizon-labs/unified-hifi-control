//! Unified Hi-Fi Control - Rust Implementation
//!
//! A source-agnostic hi-fi control bridge for hardware surfaces and Home Assistant.

// Server-only: full server implementation
#[cfg(feature = "server")]
mod server {
    use unified_hifi_control::{
        adapters, aggregator, api, app, bus, config, coordinator, embedded, firmware, knobs, mcp,
        mdns,
    };

    // Import load_app_settings for checking adapter enabled state
    use api::load_app_settings;

    use anyhow::Result;
    use axum::middleware::from_fn_with_state;
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
    use tower_http::{
        compression::CompressionLayer,
        cors::{AllowOrigin, Any, CorsLayer},
        trace::TraceLayer,
    };
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    /// Same-origin browser requests do not need CORS. Cross-origin browser
    /// access is opt-in because this server also exposes playback, OAuth, and
    /// companion authority endpoints; a global permissive policy would let
    /// arbitrary sites issue those requests through a tunnel. Operators that
    /// intentionally host a separate UI must set `UHC_ALLOWED_ORIGINS` to a
    /// comma-separated list of exact origins.
    fn configured_cors_layer() -> CorsLayer {
        let origins =
            configured_origin_headers(std::env::var("UHC_ALLOWED_ORIGINS").ok().as_deref());

        if origins.is_empty() {
            tracing::info!("CORS disabled; same-origin access remains available");
            CorsLayer::new()
        } else {
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods(Any)
                .allow_headers(Any)
        }
    }

    fn configured_origin_headers(raw: Option<&str>) -> Vec<axum::http::HeaderValue> {
        raw.into_iter()
            .flat_map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|origin| !origin.is_empty())
            .filter(|origin| {
                (origin.starts_with("http://") || origin.starts_with("https://"))
                    && !origin.chars().any(char::is_whitespace)
            })
            .filter_map(|origin| match axum::http::HeaderValue::from_str(&origin) {
                Ok(value) => Some(value),
                Err(error) => {
                    tracing::warn!(
                        origin,
                        "Ignoring invalid UHC_ALLOWED_ORIGINS entry: {error}"
                    );
                    None
                }
            })
            .collect::<Vec<_>>()
    }

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

    /// Resolve the primary address other devices use to reach this host.
    /// Connecting a UDP socket selects the default route without sending data.
    fn is_trusted_lan_ip(ip: &std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(ip) => ip.is_private(),
            std::net::IpAddr::V6(ip) => ip.is_unique_local(),
        }
    }

    fn primary_non_loopback_ip() -> Option<std::net::IpAddr> {
        let routed_ip = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|socket| {
                socket.connect("192.0.2.1:80")?;
                socket.local_addr()
            })
            .ok()
            .map(|addr| addr.ip())
            .filter(is_trusted_lan_ip);

        routed_ip.or_else(|| {
            if_addrs::get_if_addrs()
                .ok()?
                .into_iter()
                .filter(|interface| !interface.is_loopback() && !interface.is_link_local())
                .map(|interface| interface.ip())
                .filter(is_trusted_lan_ip)
                .min_by_key(|ip| match ip {
                    std::net::IpAddr::V4(_) => 0,
                    std::net::IpAddr::V6(_) => 1,
                })
        })
    }

    #[cfg(test)]
    mod address_tests {
        use super::is_trusted_lan_ip;
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        #[test]
        fn trusted_lan_addresses_are_private_or_unique_local() {
            assert!(is_trusted_lan_ip(&IpAddr::V4(Ipv4Addr::new(
                192, 168, 1, 2
            ))));
            assert!(is_trusted_lan_ip(&IpAddr::V4(Ipv4Addr::new(
                10, 20, 30, 40
            ))));
            assert!(is_trusted_lan_ip(&IpAddr::V6(Ipv6Addr::new(
                0xfd12, 0x3456, 0x789a, 0, 0, 0, 0, 1
            ))));
        }

        #[test]
        fn public_and_local_only_addresses_are_not_advertised() {
            assert!(!is_trusted_lan_ip(&IpAddr::V4(Ipv4Addr::new(
                203, 0, 113, 10
            ))));
            assert!(!is_trusted_lan_ip(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
            assert!(!is_trusted_lan_ip(&IpAddr::V4(Ipv4Addr::new(
                169, 254, 1, 2
            ))));
            assert!(!is_trusted_lan_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
            assert!(!is_trusted_lan_ip(&IpAddr::V6(Ipv6Addr::new(
                0x2001, 0x0db8, 0, 0, 0, 0, 0, 1
            ))));
        }
    }

    #[cfg(test)]
    mod cors_tests {
        use super::configured_origin_headers;

        #[test]
        fn cors_is_empty_by_default_and_accepts_only_explicit_origins() {
            assert!(configured_origin_headers(None).is_empty());
            let origins = configured_origin_headers(Some(
                "https://uhc.example.test, https://admin.example.test,not an origin",
            ));
            assert_eq!(origins.len(), 2);
            assert_eq!(origins[0], "https://uhc.example.test");
            assert_eq!(origins[1], "https://admin.example.test");
        }
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
        let mcp_host = primary_non_loopback_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| {
                format!(
                    "{}.local",
                    gethostname::gethostname()
                        .to_string_lossy()
                        .trim_end_matches(".local")
                )
            });
        let mcp_endpoint = app::McpEndpoint::new(&mcp_host, config.port);
        tracing::info!("MCP agent endpoint: {}", mcp_endpoint.url);

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
        // Both LMS observers publish the same `lms:` projection.  Register the
        // companion with the coordinator so reconfiguration and shutdown stop
        // both workers before retiring that shared projection.
        coord.register_companion("lms", lms_cli.clone()).await;
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

        // Direct streaming adapters. Credentials are supplied through the
        // provider OAuth/bridge contract (#463) or environment bootstrap.
        // Spotify is controller-only: it discovers and controls existing Connect
        // devices, never acting as a receiver.
        let spotify = Arc::new(adapters::spotify::SpotifyAdapter::new(bus.clone()));
        if let Ok(access_token) = std::env::var("SPOTIFY_ACCESS_TOKEN") {
            if !access_token.trim().is_empty() {
                spotify
                    .set_token(adapters::spotify::SpotifyToken {
                        access_token,
                        refresh_token: std::env::var("SPOTIFY_REFRESH_TOKEN").ok(),
                        expires_at: std::env::var("SPOTIFY_TOKEN_EXPIRES_AT")
                            .ok()
                            .and_then(|value| value.parse().ok()),
                    })
                    .await;
            }
        }

        let music_assistant_config = match (
            std::env::var("MUSIC_ASSISTANT_HOST"),
            std::env::var("MUSIC_ASSISTANT_TOKEN"),
        ) {
            (Ok(host), Ok(token)) if !host.trim().is_empty() && !token.trim().is_empty() => {
                let port = std::env::var("MUSIC_ASSISTANT_PORT")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(8095);
                let tls = std::env::var("MUSIC_ASSISTANT_TLS")
                    .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
                    .unwrap_or(true);
                let allow_insecure_http = std::env::var("MUSIC_ASSISTANT_INSECURE_HTTP")
                    .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
                    .unwrap_or(false);
                Some(adapters::musicassistant::MusicAssistantConfig {
                    host,
                    port,
                    token,
                    tls,
                    allow_insecure_http,
                })
            }
            _ => None,
        };

        // =========================================================================
        // Start enabled adapters (single codepath using coordinator)
        // =========================================================================

        // Build the complete lifecycle list. Provider adapters are registered
        // with the API registry below, but the coordinator owns their start
        // and stop decisions just like local adapters.
        // Note: lms_cli shares config with lms - both start when LMS is configured
        let legacy_startables: Vec<Arc<dyn adapters::Startable>> = vec![
            roon.clone(),
            lms.clone(),
            lms_cli.clone(),
            openhome.clone(),
            upnp.clone(),
        ];

        let mut provider_startables: Vec<Arc<dyn adapters::Startable>> = vec![spotify.clone()];
        let mut startable_adapters = legacy_startables.clone();
        startable_adapters.extend(provider_startables.iter().cloned());

        // Initialize ZoneAggregator for unified zone state
        let zone_aggregator = Arc::new(aggregator::ZoneAggregator::new(bus.clone()));
        let aggregator_for_spawn = zone_aggregator.clone();
        let (aggregator_ready_tx, aggregator_ready_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            aggregator_for_spawn
                .run_with_ready(aggregator_ready_tx)
                .await;
        });
        // The event bus does not replay.  Do not let any adapter publish its
        // initial snapshot until the aggregator has subscribed.
        aggregator_ready_rx
            .await
            .map_err(|_| anyhow::anyhow!("zone aggregator failed to initialize"))?;
        tracing::info!("ZoneAggregator started");

        // Credential-backed providers historically started whenever their
        // credentials were present, even before the settings toggle existed.
        // Keep that compatibility while recording the decision in the same
        // coordinator registry used by every other adapter.  Dynamic settings
        // changes can subsequently disable them through the coordinator.
        coord
            .set_enabled(
                "spotify",
                app_settings.adapters.spotify || spotify.is_configured().await,
            )
            .await;
        let music_assistant_configured = music_assistant_config.is_some();
        coord
            .set_enabled(
                "musicassistant",
                app_settings.adapters.musicassistant || music_assistant_configured,
            )
            .await;

        // Start local adapters only after the aggregator readiness barrier.
        // Provider adapters start after the registry is ready, through the
        // same coordinator-owned lifecycle path below.
        coord.start_all_enabled(&legacy_startables).await;

        // Create shutdown token for graceful SSE termination (fixes #73)
        let shutdown_token = CancellationToken::new();

        // Build application state (clone Arcs so we can access adapters for shutdown)
        let mut state = api::AppState::new(
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
        if let Some(bootstrap_token) = state.controller_auth.take_bootstrap_secret().await {
            tracing::info!(
                "UHC controller bootstrap token (display once; do not put it in a tunnel URL): {}",
                bootstrap_token
            );
        }

        // #531: registered for `hifi_collections`' content operation only --
        // `search`/`play_uri` refuse through this trait (see
        // `LibraryAdapter for LmsAdapter`/`for RoonAdapter`'s docs);
        // `hifi_search`/`hifi_play` keep calling these adapters directly.
        state
            .adapter_registry
            .register_library("lms", state.lms.clone())
            .await;
        state
            .adapter_registry
            .register_library("roon", state.roon.clone())
            .await;
        state
            .adapter_registry
            .register_with_lifecycle(spotify.clone(), spotify.clone())
            .await;
        state
            .adapter_registry
            .register_library("spotify", spotify.clone())
            .await;
        state.provider_auth.attach_spotify(spotify.clone()).await;
        state
            .provider_auth
            .attach_musicassistant(state.musicassistant.clone())
            .await;
        let persisted_music_assistant_config = state
            .provider_auth
            .musicassistant_bootstrap_config()
            .unwrap_or_else(|error| {
                tracing::warn!("Music Assistant encrypted configuration unavailable: {error}");
                None
            });
        if let Some(config) = music_assistant_config.or(persisted_music_assistant_config) {
            match adapters::musicassistant::MusicAssistantAdapter::new(bus.clone(), config.clone())
            {
                Ok(adapter) => {
                    if let Err(error) = state
                        .musicassistant
                        .install(Arc::new(adapter), config)
                        .await
                    {
                        tracing::warn!("Music Assistant adapter disabled: {error}");
                    }
                }
                Err(error) => tracing::warn!("Music Assistant adapter disabled: {error}"),
            }
        }
        // An encrypted credential-only restart discovers its usable client
        // here, after the initial settings registry was constructed. Preserve
        // the established credential-backed provider behavior: a configured
        // MA peer starts even when its optional feature switch was previously
        // false, while later settings writes remain authoritative.
        coord
            .set_enabled(
                "musicassistant",
                app_settings.adapters.musicassistant || state.musicassistant.is_configured().await,
            )
            .await;
        state
            .adapter_registry
            .register_with_lifecycle(state.musicassistant.clone(), state.musicassistant.clone())
            .await;
        state
            .adapter_registry
            .register_library("musicassistant", state.musicassistant.clone())
            .await;
        provider_startables.push(state.musicassistant.clone());
        let mut startable_adapters = (*state.startable_adapters).clone();
        startable_adapters.push(state.musicassistant.clone());
        state.startable_adapters = Arc::new(startable_adapters);

        // Apple Music playback is owned by a native MusicKit companion. The
        // paired bridge implementation shares the registry with the HTTP
        // pairing endpoints and starts on the first successful claim.
        let apple_music = Arc::new(adapters::apple_music::AppleMusicAdapter::with_companion(
            bus.clone(),
            Arc::new(api::apple_bridge::PairedMusicKitCompanion::new(
                state.apple_bridges.clone(),
            )),
            std::time::Duration::from_secs(5),
        ));
        state
            .adapter_registry
            .register_with_lifecycle(apple_music.clone(), apple_music.clone())
            .await;
        state
            .adapter_registry
            .register_library("applemusic", apple_music.clone())
            .await;

        // Provider adapters participate in the same feature-toggle lifecycle as
        // local adapters. Their zones still arrive through the bus and the
        // aggregator; the registry only dispatches commands and controls their
        // lifecycle after the coordinator has approved them.
        provider_startables.push(apple_music.clone());
        let mut startable_adapters = (*state.startable_adapters).clone();
        startable_adapters.push(apple_music.clone());
        state.startable_adapters = Arc::new(startable_adapters);
        coord.start_all_enabled(&provider_startables).await;

        // Clone state for shutdown diagnostics
        let state_for_shutdown = state.clone();

        // Create MCP extension (state for MCP handlers)
        let mcp_extension = mcp::create_mcp_extension(state.clone());

        // Build API routes
        let api_oauth_start = api::provider_auth::oauth_start;
        let api_oauth_callback = api::provider_auth::oauth_callback;
        let api_oauth_revoke = api::provider_auth::oauth_revoke;
        let api_provider_configure = api::provider_auth::configure_provider;
        let api_spotify_account = api::spotify_account_handler;
        let api_musicassistant_status = api::provider_auth::musicassistant_status;
        let api_bridge_pair = api::apple_bridge::pair;
        let api_bridge_discover_pairing = api::apple_bridge::discover_pairing;
        let api_bridge_claim = api::apple_bridge::claim;
        let api_bridge_revoke = api::apple_bridge::revoke;
        let api_bridge_rename = api::apple_bridge::rename;
        let api_bridge_revoke_by_bridge_id = api::apple_bridge::revoke_by_bridge_id;
        let api_bridge_status = api::apple_bridge::status;
        let api_bridge_state = api::apple_bridge::state;
        let api_bridge_commands = api::apple_bridge::commands;
        let api_bridge_ack = api::apple_bridge::acknowledge;
        let api_bridge_content = api::apple_bridge::content;
        let api_bridge_content_ack = api::apple_bridge::acknowledge_content;
        let controller_bootstrap = api::controller_auth::bootstrap;
        let controller_status = api::controller_auth::status;
        #[rustfmt::skip]
        let router = Router::new()
            // Health check
            .route("/status", get(api::status_handler))
            // Installation-bound controller bootstrap/session boundary
            .route("/api/controller/bootstrap", post(controller_bootstrap))
            .route("/api/controller/status", get(controller_status))
            // Provider authorization and native companion pairing
            .route("/api/providers/{provider}/oauth/start", get(api_oauth_start))
            .route("/api/providers/{provider}/oauth/callback", get(api_oauth_callback))
            .route("/api/providers/{provider}/oauth/revoke", post(api_oauth_revoke))
            .route("/api/providers/{provider}/configure", post(api_provider_configure))
            .route("/api/providers/spotify/account", get(api_spotify_account))
            .route("/api/providers/musicassistant/status", get(api_musicassistant_status))
            .route("/api/bridges/applemusic/pair", post(api_bridge_pair))
            .route("/api/bridges/applemusic/discover", post(api_bridge_discover_pairing))
            .route("/api/bridges/applemusic/claim", post(api_bridge_claim))
            .route("/api/bridges/applemusic/revoke", post(api_bridge_revoke))
            .route("/api/bridges/applemusic/rename", post(api_bridge_rename))
            .route("/api/bridges/applemusic/remove", post(api_bridge_revoke_by_bridge_id))
            .route("/api/bridges/applemusic/status", get(api_bridge_status))
            .route("/api/bridges/applemusic/state", post(api_bridge_state))
            .route("/api/bridges/applemusic/commands", get(api_bridge_commands))
            .route("/api/bridges/applemusic/commands/{command_id}", post(api_bridge_ack))
            .route("/api/bridges/applemusic/content", get(api_bridge_content))
            .route("/api/bridges/applemusic/content/{request_id}", post(api_bridge_content_ack))
            // Roon routes
            .route("/roon/status", get(api::roon_status_handler))
            .route("/roon/zones", get(api::roon_zones_handler))
            .route("/roon/zone/{zone_id}", get(api::roon_zone_handler))
            .route("/roon/control", post(api::roon_control_handler))
            .route("/roon/volume", post(api::roon_volume_handler))
            .route("/roon/image", get(api::roon_image_handler))
            // Roon Browse routes
            .route("/roon/search", get(api::roon_search_handler))
            .route("/roon/play", post(api::roon_play_handler))
            .route("/roon/play_item", post(api::roon_play_item_handler))
            .route("/roon/browse", post(api::roon_browse_handler))
            .route("/roon/browse/status", get(api::roon_browse_status_handler))
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
            // Library browse, queue and play-ref for the web UI (#507). Same
            // verb vocabulary as the hifi_collections/hifi_queue/hifi_play_ref
            // MCP tools -- see src/api/browse.rs.
            .route("/api/collections", post(api::browse::collections_handler))
            .route("/api/queue", post(api::browse::queue_handler))
            .route("/api/play_ref", post(api::browse::play_ref_handler))
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
            .route("/assets/{*path}", get(embedded::serve_embedded_asset));

        // Static file routes: only needed for non-web builds (cargo run).
        // In web/fullstack mode, Dioxus automatically serves public/ directory.
        #[cfg(not(feature = "web"))]
        let router = router
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
            );

        // Keep router in scope for web builds (static files served by Dioxus)
        #[cfg(feature = "web")]
        let router = router;

        let router = router
            // MCP routes (same port as main app)
            .route("/mcp", get(mcp::handle_mcp_get))
            .route("/mcp", post(mcp::handle_mcp_post))
            .route("/mcp", delete(mcp::handle_mcp_delete))
            // Middleware
            .layer(mcp_extension)
            .layer(from_fn_with_state(
                state.controller_auth.clone(),
                api::controller_auth::middleware,
            ))
            .layer(configured_cors_layer())
            .layer(CompressionLayer::new())
            .layer(TraceLayer::new_for_http())
            .with_state(state);

        // ADR 002: Embedded assets mode - SSR with injected bootstrap scripts
        // serve_api_application() provides SSR + server functions, but no static assets
        // Our middleware injects the bootstrap scripts (from embedded index.html) into SSR HTML
        // This enables WASM hydration without requiring a public/ directory at runtime
        let serve_config = || dioxus::server::ServeConfig::new().context(mcp_endpoint.clone());
        let router = if embedded::has_embedded_assets() {
            if let Some(bootstrap) = embedded::extract_bootstrap_snippet() {
                tracing::info!("Using embedded SSR mode (bootstrap scripts will be injected)");
                tracing::debug!("Bootstrap snippet:\n{}", bootstrap);
                router
                    .serve_api_application(serve_config(), app::App)
                    .layer(embedded::InjectDioxusBootstrapLayer::new(bootstrap))
            } else {
                tracing::warn!(
                    "Embedded assets found but no bootstrap scripts - falling back to SPA"
                );
                router
                    .serve_api_application(serve_config(), app::App)
                    .fallback(embedded::serve_index_html)
            }
        } else {
            tracing::info!("Using SSR mode (no embedded assets, use dx serve for development)");
            // Standard SSR mode for development
            router.serve_dioxus_application(serve_config(), app::App)
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

        // Stop every adapter through the coordinator-owned lifecycle list.
        coord.stop_all(&state_for_shutdown.startable_adapters).await;
        if let Some(ref fw) = firmware_service {
            fw.stop();
        }
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
