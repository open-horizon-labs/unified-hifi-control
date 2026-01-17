//! MQTT Adapter
//!
//! Bridges the internal event bus to MQTT for Home Assistant integration.

use anyhow::{anyhow, Result};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::bus::{BusEvent, SharedBus};

const DEFAULT_PORT: u16 = 1883;
const DEFAULT_TOPIC_PREFIX: &str = "unified-hifi-control";

/// MQTT connection status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MqttStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub port: u16,
    pub topic_prefix: String,
}

/// Internal state
struct MqttState {
    host: Option<String>,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    topic_prefix: String,
    connected: bool,
}

impl Default for MqttState {
    fn default() -> Self {
        Self {
            host: None,
            port: DEFAULT_PORT,
            username: None,
            password: None,
            topic_prefix: DEFAULT_TOPIC_PREFIX.to_string(),
            connected: false,
        }
    }
}

/// MQTT Adapter
pub struct MqttAdapter {
    state: Arc<RwLock<MqttState>>,
    client: Arc<RwLock<Option<AsyncClient>>>,
    bus: SharedBus,
    shutdown: CancellationToken,
}

impl MqttAdapter {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(MqttState::default())),
            client: Arc::new(RwLock::new(None)),
            bus,
            shutdown: CancellationToken::new(),
        }
    }

    /// Configure the MQTT connection
    pub async fn configure(
        &self,
        host: String,
        port: Option<u16>,
        username: Option<String>,
        password: Option<String>,
        topic_prefix: Option<String>,
    ) {
        let mut state = self.state.write().await;
        state.host = Some(host);
        state.port = port.unwrap_or(DEFAULT_PORT);
        state.username = username;
        state.password = password;
        if let Some(prefix) = topic_prefix {
            state.topic_prefix = prefix;
        }
    }

    /// Check if configured
    pub async fn is_configured(&self) -> bool {
        self.state.read().await.host.is_some()
    }

    /// Get connection status
    pub async fn get_status(&self) -> MqttStatus {
        let state = self.state.read().await;
        MqttStatus {
            connected: state.connected,
            host: state.host.clone(),
            port: state.port,
            topic_prefix: state.topic_prefix.clone(),
        }
    }

    /// Start MQTT connection and bridge
    pub async fn start(&self) -> Result<()> {
        let (host, port, username, password, topic_prefix) = {
            let state = self.state.read().await;
            let host = state
                .host
                .clone()
                .ok_or_else(|| anyhow!("MQTT host not configured"))?;
            (
                host,
                state.port,
                state.username.clone(),
                state.password.clone(),
                state.topic_prefix.clone(),
            )
        };

        // Create MQTT options
        let mut options = MqttOptions::new("unified-hifi-control", &host, port);
        options.set_keep_alive(Duration::from_secs(30));

        if let (Some(user), Some(pass)) = (&username, &password) {
            options.set_credentials(user, pass);
        }

        // Create client
        let (client, mut eventloop) = AsyncClient::new(options, 100);

        // Store client
        {
            let mut client_guard = self.client.write().await;
            *client_guard = Some(client.clone());
        }

        // Subscribe to control topics
        let control_topic = format!("{}/+/control", topic_prefix);
        client.subscribe(&control_topic, QoS::AtMostOnce).await?;

        tracing::info!("MQTT connecting to {}:{}...", host, port);

        // Note: connected state will be set true when ConnAck is received

        // Spawn event loop handler
        let state = self.state.clone();
        let bus = self.bus.clone();
        let _prefix = topic_prefix.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!("MQTT event loop shutting down");
                        break;
                    }
                    result = eventloop.poll() => {
                        match result {
                            Ok(Event::Incoming(Incoming::Publish(publish))) => {
                                // Handle incoming control messages
                                let topic = publish.topic.clone();
                                let payload = String::from_utf8_lossy(&publish.payload).to_string();

                                if topic.ends_with("/control") {
                                    // Parse zone_id from topic: prefix/zone_id/control
                                    let parts: Vec<&str> = topic.split('/').collect();
                                    if parts.len() >= 2 {
                                        let zone_id = parts[parts.len() - 2].to_string();

                                        // Try to parse as JSON
                                        if let Ok(cmd) = serde_json::from_str::<Value>(&payload) {
                                            let action = cmd
                                                .get("action")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("play_pause")
                                                .to_string();
                                            let value = cmd.get("value").cloned();

                                            bus.publish(BusEvent::ControlCommand {
                                                zone_id,
                                                action,
                                                value,
                                            });
                                        }
                                    }
                                }
                            }
                            Ok(Event::Incoming(Incoming::ConnAck(ack))) => {
                                tracing::info!("MQTT connected (code: {:?})", ack.code);
                                let mut state = state.write().await;
                                state.connected = true;
                            }
                            Ok(Event::Incoming(Incoming::Disconnect)) => {
                                tracing::warn!("MQTT disconnected");
                                let mut state = state.write().await;
                                state.connected = false;
                            }
                            Err(e) => {
                                tracing::error!("MQTT error: {}", e);
                                let mut state = state.write().await;
                                state.connected = false;
                                // Check shutdown before sleeping
                                tokio::select! {
                                    _ = shutdown.cancelled() => break,
                                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        // Spawn bus event forwarder
        let client_clone = self.client.clone();
        let bus_clone = self.bus.clone();
        let prefix_clone = topic_prefix.clone();
        let shutdown2 = self.shutdown.clone();

        tokio::spawn(async move {
            let mut rx = bus_clone.subscribe();

            loop {
                tokio::select! {
                    _ = shutdown2.cancelled() => {
                        tracing::info!("MQTT bus forwarder shutting down");
                        break;
                    }
                    result = rx.recv() => {
                        match result {
                            Ok(event) => {
                                // Check if client is still connected before publishing
                                if let Some(client) = client_clone.read().await.as_ref() {
                                    let _ = Self::publish_event(client, &prefix_clone, &event).await;
                                }
                            }
                            Err(_) => {
                                // Channel lagged or closed, continue
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    /// Publish event to MQTT
    async fn publish_event(client: &AsyncClient, prefix: &str, event: &BusEvent) -> Result<()> {
        let (topic_suffix, payload) = match event {
            BusEvent::RoonConnected { core_name, version } => (
                "roon/status".to_string(),
                serde_json::json!({
                    "connected": true,
                    "core_name": core_name,
                    "version": version
                }),
            ),
            BusEvent::RoonDisconnected => (
                "roon/status".to_string(),
                serde_json::json!({
                    "connected": false
                }),
            ),
            BusEvent::ZoneUpdated {
                zone_id,
                display_name,
                state,
            } => (
                format!("zones/{}/state", zone_id),
                serde_json::json!({
                    "zone_id": zone_id,
                    "display_name": display_name,
                    "state": state
                }),
            ),
            BusEvent::ZoneRemoved { zone_id } => (
                format!("zones/{}/state", zone_id),
                serde_json::json!({
                    "zone_id": zone_id,
                    "removed": true
                }),
            ),
            BusEvent::NowPlayingChanged {
                zone_id,
                title,
                artist,
                album,
                image_key,
            } => (
                format!("zones/{}/now_playing", zone_id),
                serde_json::json!({
                    "zone_id": zone_id,
                    "title": title,
                    "artist": artist,
                    "album": album,
                    "image_key": image_key
                }),
            ),
            BusEvent::SeekPositionChanged { zone_id, position } => (
                format!("zones/{}/position", zone_id),
                serde_json::json!({
                    "zone_id": zone_id,
                    "position": position
                }),
            ),
            BusEvent::VolumeChanged {
                output_id,
                value,
                is_muted,
            } => (
                format!("outputs/{}/volume", output_id),
                serde_json::json!({
                    "output_id": output_id,
                    "value": value,
                    "is_muted": is_muted
                }),
            ),
            BusEvent::HqpConnected { host } => (
                "hqplayer/status".to_string(),
                serde_json::json!({
                    "connected": true,
                    "host": host
                }),
            ),
            BusEvent::HqpDisconnected { host } => (
                "hqplayer/status".to_string(),
                serde_json::json!({
                    "connected": false,
                    "host": host
                }),
            ),
            BusEvent::HqpStateChanged { host, state } => (
                "hqplayer/state".to_string(),
                serde_json::json!({
                    "host": host,
                    "state": state
                }),
            ),
            BusEvent::HqpPipelineChanged {
                host,
                filter,
                shaper,
                rate,
            } => (
                "hqplayer/pipeline".to_string(),
                serde_json::json!({
                    "host": host,
                    "filter": filter,
                    "shaper": shaper,
                    "rate": rate
                }),
            ),
            BusEvent::LmsConnected { host } => (
                "lms/status".to_string(),
                serde_json::json!({
                    "connected": true,
                    "host": host
                }),
            ),
            BusEvent::LmsDisconnected { host } => (
                "lms/status".to_string(),
                serde_json::json!({
                    "connected": false,
                    "host": host
                }),
            ),
            BusEvent::LmsPlayerStateChanged { player_id, state } => (
                format!("lms/players/{}/state", player_id),
                serde_json::json!({
                    "player_id": player_id,
                    "state": state
                }),
            ),
            BusEvent::ControlCommand { .. } => {
                // Don't re-publish control commands
                return Ok(());
            }
            // New architecture events
            BusEvent::ZoneDiscovered { zone } => (
                format!("zones/{}/discovered", zone.zone_id),
                serde_json::json!({
                    "zone_id": zone.zone_id,
                    "zone_name": zone.zone_name,
                    "source": zone.source,
                    "state": zone.state.to_string()
                }),
            ),
            BusEvent::CommandReceived { zone_id, command, request_id } => (
                format!("zones/{}/command", zone_id),
                serde_json::json!({
                    "zone_id": zone_id,
                    "command": command,
                    "request_id": request_id
                }),
            ),
            BusEvent::CommandResult { response, request_id } => (
                format!("zones/{}/command_result", response.zone_id),
                serde_json::json!({
                    "zone_id": response.zone_id,
                    "success": response.success,
                    "error": response.error,
                    "request_id": request_id
                }),
            ),
            BusEvent::AdapterStopping { adapter, reason } => (
                format!("adapters/{}/stopping", adapter),
                serde_json::json!({
                    "adapter": adapter,
                    "reason": reason
                }),
            ),
            BusEvent::AdapterStopped { adapter } => (
                format!("adapters/{}/stopped", adapter),
                serde_json::json!({
                    "adapter": adapter
                }),
            ),
            BusEvent::ZonesFlushed { adapter, zone_ids } => (
                format!("adapters/{}/zones_flushed", adapter),
                serde_json::json!({
                    "adapter": adapter,
                    "zone_ids": zone_ids
                }),
            ),
            BusEvent::AdapterConnected { adapter, details } => (
                format!("adapters/{}/status", adapter),
                serde_json::json!({
                    "adapter": adapter,
                    "connected": true,
                    "details": details
                }),
            ),
            BusEvent::AdapterDisconnected { adapter, reason } => (
                format!("adapters/{}/status", adapter),
                serde_json::json!({
                    "adapter": adapter,
                    "connected": false,
                    "reason": reason
                }),
            ),
            BusEvent::ShuttingDown { reason } => (
                "system/shutdown".to_string(),
                serde_json::json!({
                    "shutting_down": true,
                    "reason": reason
                }),
            ),
            BusEvent::HealthCheck { timestamp: _ } => {
                // Don't publish health checks to MQTT
                return Ok(());
            }
        };

        let topic = format!("{}/{}", prefix, topic_suffix);
        let payload_str = serde_json::to_string(&payload)?;

        client
            .publish(&topic, QoS::AtMostOnce, false, payload_str.as_bytes())
            .await?;

        Ok(())
    }

    /// Stop MQTT connection
    pub async fn stop(&self) {
        // Cancel background tasks first
        self.shutdown.cancel();

        // Then disconnect client
        let mut client = self.client.write().await;
        if let Some(c) = client.take() {
            let _ = c.disconnect().await;
        }

        let mut state = self.state.write().await;
        state.connected = false;

        tracing::info!("MQTT adapter stopped");
    }

    /// Publish a message
    pub async fn publish(&self, topic: &str, payload: &str) -> Result<()> {
        let prefix = self.state.read().await.topic_prefix.clone();
        let full_topic = format!("{}/{}", prefix, topic);

        let client = self.client.read().await;
        if let Some(c) = client.as_ref() {
            c.publish(&full_topic, QoS::AtMostOnce, false, payload.as_bytes())
                .await?;
        }

        Ok(())
    }
}
