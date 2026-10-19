//! Home Assistant `media_player` entities as playback devices (#790). **Alpha.**
//!
//! Every `media_player` entity Home Assistant exposes is offered as a device — no allowlist —
//! except UHC's own exported zones. Credentials are the ones UHC already uses for the Home
//! Assistant core API (`SUPERVISOR_TOKEN` under the add-on, `UHC_HA_API_URL`/`UHC_HA_API_TOKEN`
//! standalone). With neither, the family is dormant.
//!
//! Safety rules that stay (they are hazards, not device limits):
//!
//! - Volume is Home Assistant's 0–1 `volume_level`, published as `Linear` with 1.0 as its
//!   maximum. Any user ceiling lowers that further.
//! - Self-exclusion fails closed: if the entity registry cannot be read, nothing is offered.
//!
//! Users avoid linking one amplifier both natively and through Home Assistant (decision #790);
//! nothing here detects it.
//!
//! State is polled from the REST API ([`POLL_INTERVAL`]); the registry comes from one WebSocket
//! request per [`REGISTRY_REFRESH`]. Commands are `media_player.*` service calls, each followed by
//! readbacks so the device service confirms against Home Assistant's reported state.

pub mod protocol;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::adapters::devices::{DeviceFamily, FamilyContext};
use crate::bus::{BusEvent, SharedBus, VolumeScale};
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    ControlChannel, Device, DeviceConnection, DeviceId, DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::EntityState;

pub const FAMILY: &str = "homeassistant";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const REGISTRY_REFRESH: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const READBACK_SCHEDULE: &[Duration] = &[
    Duration::from_millis(300),
    Duration::from_millis(700),
    Duration::from_millis(1200),
];

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn next_revision() -> u64 {
    static REVISION: AtomicU64 = AtomicU64::new(0);
    REVISION
        .compare_exchange(
            0,
            now_ms().saturating_mul(1000),
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .ok();
    REVISION.fetch_add(1, Ordering::SeqCst) + 1
}

#[derive(Clone)]
struct Endpoint {
    rest: String,
    websocket: String,
    token: String,
}

#[derive(Default)]
struct State {
    running: bool,
    cancel: CancellationToken,
    /// Device id → entity id, for entities currently offered.
    entities: HashMap<String, String>,
    /// Last published state per entity, to publish only on change during polling.
    last: HashMap<String, EntityState>,
    excluded: Option<HashSet<String>>,
    unique_ids: HashMap<String, String>,
}

#[derive(Clone)]
pub struct HomeAssistantDevices {
    bus: SharedBus,
    http: Client,
    endpoint: Option<Endpoint>,
    state: Arc<RwLock<State>>,
}

impl HomeAssistantDevices {
    /// Uses the core-API credentials from the environment; dormant without them.
    pub fn new(bus: SharedBus) -> Self {
        let endpoint = crate::mqtt::consumer::core_api_credentials().and_then(|(url, token)| {
            protocol::endpoints(&url).map(|(rest, websocket)| Endpoint {
                rest,
                websocket,
                token,
            })
        });
        Self::with_endpoint(bus, endpoint.map(|e| (e.rest, e.websocket, e.token)))
    }

    /// Explicit endpoint (REST base, WebSocket URL, token), for tests and configuration.
    pub fn with_endpoint(bus: SharedBus, endpoint: Option<(String, String, String)>) -> Self {
        Self {
            bus,
            http: Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .unwrap_or_default(),
            endpoint: endpoint.map(|(rest, websocket, token)| Endpoint {
                rest,
                websocket,
                token,
            }),
            state: Arc::new(RwLock::new(State::default())),
        }
    }

    fn device_id(unique_id: &str) -> Option<DeviceId> {
        DeviceId::new("ha", unique_id)
    }

    async fn run(&self, endpoint: Endpoint, cancel: CancellationToken) {
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        let mut registry = tokio::time::interval(REGISTRY_REFRESH);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = registry.tick() => self.refresh_registry(&endpoint).await,
                _ = poll.tick() => self.poll(&endpoint, false, None).await,
            }
        }
    }

    async fn refresh_registry(&self, endpoint: &Endpoint) {
        match fetch_registry(endpoint).await {
            Ok(registry) => {
                let excluded = protocol::excluded_entities(&registry);
                if excluded.is_none() {
                    tracing::warn!(
                        "Home Assistant entity registry unreadable; offering no devices"
                    );
                }
                let mut state = self.state.write().await;
                state.excluded = excluded;
                state.unique_ids = protocol::unique_ids(&registry);
            }
            Err(error) => {
                // Fail closed: without the registry, UHC cannot tell its own entities apart.
                tracing::warn!("Home Assistant entity registry unavailable: {error}");
                self.state.write().await.excluded = None;
            }
        }
    }

    /// Read every media_player and publish. `force` publishes unchanged entities too (a
    /// readback after a command); `only` limits the publish to one entity.
    async fn poll(&self, endpoint: &Endpoint, force: bool, only: Option<&str>) {
        let url = format!("{}/states", endpoint.rest);
        let states: Vec<Value> = match self
            .http
            .get(&url)
            .bearer_auth(&endpoint.token)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                response.json().await.unwrap_or_default()
            }
            Ok(response) => {
                tracing::debug!("Home Assistant states returned {}", response.status());
                return;
            }
            Err(error) => {
                tracing::debug!("Home Assistant states unavailable: {error}");
                return;
            }
        };
        let mut publish = Vec::new();
        {
            let mut state = self.state.write().await;
            let Some(excluded) = state.excluded.clone() else {
                return;
            };
            let mut seen = HashSet::new();
            for entity in states.iter().filter_map(protocol::parse_state) {
                if excluded.contains(&entity.entity_id) {
                    continue;
                }
                if only.is_some_and(|only| only != entity.entity_id) {
                    continue;
                }
                // No registry unique_id, no stable identity: not offered.
                let Some(device_id) = state
                    .unique_ids
                    .get(&entity.entity_id)
                    .and_then(|unique| Self::device_id(unique))
                else {
                    continue;
                };
                seen.insert(device_id.to_string());
                state
                    .entities
                    .insert(device_id.to_string(), entity.entity_id.clone());
                let changed = state.last.get(&entity.entity_id) != Some(&entity);
                if force || changed {
                    state.last.insert(entity.entity_id.clone(), entity.clone());
                    publish.push((device_id, entity));
                }
            }
            if only.is_none() {
                let gone: Vec<String> = state
                    .entities
                    .keys()
                    .filter(|id| !seen.contains(*id))
                    .cloned()
                    .collect();
                for id in gone {
                    if let Some(entity_id) = state.entities.remove(&id) {
                        state.last.remove(&entity_id);
                    }
                    if let Some(device_id) = DeviceId::parse(&id) {
                        self.bus.publish(BusEvent::DeviceRemoved { device_id });
                    }
                }
            }
        }
        for (device_id, entity) in publish {
            self.bus.publish(BusEvent::DeviceObserved {
                device: Box::new(device(device_id, &entity, &endpoint.rest)),
            });
        }
    }
}

fn device(device_id: DeviceId, entity: &EntityState, host: &str) -> Device {
    Device {
        device_id,
        vendor: "Home Assistant".to_string(),
        model: entity.entity_id.clone(),
        name: entity.name.clone(),
        host: host.to_string(),
        discovery: "homeassistant".to_string(),
        maturity: DEVICE_CONTROL_MATURITY.to_string(),
        connection: DeviceConnection {
            control_channel: ControlChannel::Polling,
            detail: None,
        },
        power: entity.power,
        input: entity.source.clone(),
        inputs: protocol::inputs(entity),
        volume: DeviceVolume {
            scale: VolumeScale::Linear,
            value: entity.volume,
            min: 0.0,
            // Home Assistant's volume_level is 0–1 by definition.
            hardware_max: Some(1.0),
            ceiling: None,
            step: 0.01,
            is_muted: entity.muted,
            display: None,
        },
        capabilities: protocol::capabilities(entity.features),
        revision: next_revision(),
        last_updated: now_ms(),
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn read_json(socket: &mut Socket) -> Result<Value, String> {
    loop {
        match tokio::time::timeout(HTTP_TIMEOUT, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                return serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => return Err(e.to_string()),
            Ok(None) => return Err("websocket closed".to_string()),
            Err(_) => return Err("websocket read timed out".to_string()),
        }
    }
}

async fn send_json(socket: &mut Socket, value: Value) -> Result<(), String> {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(|e| e.to_string())
}

/// One WebSocket request for the entity registry.
async fn fetch_registry(endpoint: &Endpoint) -> Result<Value, String> {
    let (mut socket, _) = tokio::time::timeout(
        HTTP_TIMEOUT,
        tokio_tungstenite::connect_async(endpoint.websocket.as_str()),
    )
    .await
    .map_err(|_| "websocket connect timed out".to_string())?
    .map_err(|e| e.to_string())?;
    let greeting = read_json(&mut socket).await?;
    if greeting.get("type").and_then(Value::as_str) != Some("auth_required") {
        return Err("unexpected websocket greeting".to_string());
    }
    send_json(
        &mut socket,
        json!({"type": "auth", "access_token": endpoint.token}),
    )
    .await?;
    let auth = read_json(&mut socket).await?;
    if auth.get("type").and_then(Value::as_str) != Some("auth_ok") {
        return Err("Home Assistant refused the token".to_string());
    }
    send_json(
        &mut socket,
        json!({"id": 1, "type": "config/entity_registry/list"}),
    )
    .await?;
    // Bounded: Home Assistant answers the one request we sent; anything else is skipped.
    for _ in 0..32 {
        let message = read_json(&mut socket).await?;
        if message.get("id").and_then(Value::as_u64) == Some(1) {
            if message.get("success").and_then(Value::as_bool) != Some(true) {
                return Err("entity registry request was refused".to_string());
            }
            if socket.close(None).await.is_err() {
                tracing::debug!("Home Assistant websocket close failed");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }
    Err("no entity registry answer".to_string())
}

#[async_trait]
impl DeviceDriver for HomeAssistantDevices {
    fn kinds(&self) -> &'static [&'static str] {
        &["ha"]
    }

    async fn execute(
        &self,
        device_id: &DeviceId,
        operation: DeviceOperation,
    ) -> Result<(), String> {
        let endpoint = self
            .endpoint
            .clone()
            .ok_or_else(|| "Home Assistant is not configured".to_string())?;
        let entity_id = self
            .state
            .read()
            .await
            .entities
            .get(device_id.as_str())
            .cloned()
            .ok_or_else(|| format!("{device_id} is not a current Home Assistant entity"))?;
        let (service, data) = match &operation {
            DeviceOperation::SetVolume(level) => (
                "volume_set",
                json!({"entity_id": entity_id, "volume_level": level.clamp(0.0, 1.0)}),
            ),
            DeviceOperation::SetMute(muted) => (
                "volume_mute",
                json!({"entity_id": entity_id, "is_volume_muted": muted}),
            ),
            DeviceOperation::PowerOn => ("turn_on", json!({"entity_id": entity_id})),
            DeviceOperation::PowerStandby => ("turn_off", json!({"entity_id": entity_id})),
            DeviceOperation::SelectInput(source) => (
                "select_source",
                json!({"entity_id": entity_id, "source": source}),
            ),
        };
        let url = format!("{}/services/media_player/{service}", endpoint.rest);
        if tokio::time::Instant::now() >= crate::command_dispatch::deadline() {
            return Err(crate::devices::service::DISPATCH_EXPIRED.into());
        }
        let response = self
            .http
            .post(&url)
            .bearer_auth(&endpoint.token)
            .json(&data)
            .send()
            .await
            .map_err(|e| format!("Home Assistant service call failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Home Assistant refused media_player.{service}: {}",
                response.status()
            ));
        }
        // Read back on a schedule so the device service confirms against reported state.
        let this = self.clone();
        tokio::spawn(async move {
            for delay in READBACK_SCHEDULE {
                tokio::time::sleep(*delay).await;
                this.poll(&endpoint, true, Some(&entity_id)).await;
            }
        });
        Ok(())
    }
}

#[async_trait]
impl DeviceFamily for HomeAssistantDevices {
    fn family(&self) -> &'static str {
        FAMILY
    }

    async fn start_family(&self, _context: FamilyContext) -> anyhow::Result<()> {
        let Some(endpoint) = self.endpoint.clone() else {
            tracing::info!("Home Assistant device family dormant: no core API credentials");
            return Ok(());
        };
        let cancel = {
            let mut state = self.state.write().await;
            if state.running {
                return Ok(());
            }
            state.running = true;
            state.cancel = CancellationToken::new();
            state.cancel.clone()
        };
        let this = self.clone();
        tokio::spawn(async move { this.run(endpoint, cancel).await });
        Ok(())
    }

    async fn stop_family(&self) {
        let ids: Vec<String> = {
            let mut state = self.state.write().await;
            state.running = false;
            state.cancel.cancel();
            state.last.clear();
            std::mem::take(&mut state.entities).into_keys().collect()
        };
        for id in ids {
            if let Some(device_id) = DeviceId::parse(&id) {
                self.bus.publish(BusEvent::DeviceRemoved { device_id });
            }
        }
    }
}
