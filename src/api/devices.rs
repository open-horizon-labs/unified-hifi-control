//! HTTP surface for playback devices (#782, #774). **Alpha**: every payload says so.
//!
//! Devices and links are read from the aggregator; links, ceilings and commands go through
//! [`crate::devices::service::DeviceService`], the same path `/knob/control` and MCP use.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::AppState;
use crate::devices::service::{request_from_action, DeviceError, DeviceTarget, DEVICE_ACTIONS};
use crate::devices::{Device, DeviceId, DEVICE_CONTROL_MATURITY};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// The error body every device route returns.
pub fn device_error_response(error: DeviceError) -> (StatusCode, Json<Value>) {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
    let mut body = json!({
        "error": error.to_string(),
        "error_code": error.code(),
        "maturity": DEVICE_CONTROL_MATURITY,
    });
    if let DeviceError::UnknownInput { accepted, .. } = &error {
        body["accepted"] = json!(accepted);
    }
    (status, Json(body))
}

fn bad_request(message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": message.into(),
            "error_code": "INVALID_VALUE",
            "maturity": DEVICE_CONTROL_MATURITY,
        })),
    )
}

/// A device plus the zones linked to it.
#[derive(Serialize)]
pub struct DeviceView {
    #[serde(flatten)]
    pub device: Device,
    pub linked_zone_ids: Vec<String>,
    /// The level range linked zones publish, or `None` while it cannot bound a level.
    pub volume_control: Option<crate::bus::VolumeControl>,
}

/// A link, with what the aggregator currently knows about both ends.
#[derive(Serialize)]
pub struct LinkView {
    pub zone_id: String,
    pub zone_name: Option<String>,
    pub zone_present: bool,
    pub device_id: DeviceId,
    pub device_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

/// Every device and link. Shared by `GET /devices` and MCP `hifi_devices action=list`.
pub async fn device_inventory(state: &AppState) -> Value {
    let devices = state.aggregator.get_devices().await;
    let links = state.aggregator.get_device_links().await;
    let mut link_views = Vec::with_capacity(links.len());
    for link in &links {
        let zone = state.aggregator.get_zone(&link.zone_id).await;
        link_views.push(LinkView {
            zone_id: link.zone_id.clone(),
            zone_name: zone.as_ref().map(|zone| zone.zone_name.clone()),
            zone_present: zone.is_some(),
            device_id: link.device_id.clone(),
            device_present: devices.iter().any(|d| d.device_id == link.device_id),
            source_id: link.source_id.clone(),
        });
    }
    let device_views: Vec<DeviceView> = devices
        .into_iter()
        .map(|device| DeviceView {
            linked_zone_ids: links
                .iter()
                .filter(|link| link.device_id == device.device_id)
                .map(|link| link.zone_id.clone())
                .collect(),
            volume_control: device.volume_control(),
            device,
        })
        .collect();
    let link_store = match state.devices.store_status().await {
        Ok(()) => json!({"writable": true}),
        Err(detail) => json!({"writable": false, "detail": detail}),
    };
    json!({
        "maturity": DEVICE_CONTROL_MATURITY,
        "devices": device_views,
        "links": link_views,
        "link_store": link_store,
        "actions": DEVICE_ACTIONS,
    })
}

/// GET /devices
pub async fn devices_handler(State(state): State<AppState>) -> Json<Value> {
    Json(device_inventory(&state).await)
}

/// GET /devices/{device_id}
pub async fn device_handler(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> ApiResult {
    let parsed = DeviceId::parse(&device_id).ok_or_else(|| {
        device_error_response(DeviceError::UnknownDevice {
            device_id: device_id.clone(),
        })
    })?;
    let device = state.aggregator.get_device(&parsed).await.ok_or_else(|| {
        device_error_response(DeviceError::UnknownDevice {
            device_id: device_id.clone(),
        })
    })?;
    let linked_zone_ids = state
        .aggregator
        .get_device_links()
        .await
        .into_iter()
        .filter(|link| link.device_id == parsed)
        .map(|link| link.zone_id)
        .collect();
    Ok(Json(json!({
        "maturity": DEVICE_CONTROL_MATURITY,
        "device": DeviceView {
            volume_control: device.volume_control(),
            device,
            linked_zone_ids,
        },
    })))
}

/// GET /devices/links
pub async fn device_links_handler(State(state): State<AppState>) -> Json<Value> {
    let inventory = device_inventory(&state).await;
    Json(json!({
        "maturity": DEVICE_CONTROL_MATURITY,
        "links": inventory["links"],
        "link_store": inventory["link_store"],
    }))
}

#[derive(Deserialize)]
pub struct DeviceLinkRequest {
    pub zone_id: String,
    pub device_id: String,
    #[serde(default)]
    pub source_id: Option<String>,
}

/// POST /devices/link
pub async fn device_link_handler(
    State(state): State<AppState>,
    Json(request): Json<DeviceLinkRequest>,
) -> ApiResult {
    let link = state
        .devices
        .link(&request.zone_id, &request.device_id, request.source_id)
        .await
        .map_err(device_error_response)?;
    Ok(Json(json!({
        "ok": true,
        "maturity": DEVICE_CONTROL_MATURITY,
        "link": link,
        "linked_device": state.aggregator.linked_device(&link.zone_id).await,
    })))
}

#[derive(Deserialize)]
pub struct DeviceUnlinkRequest {
    pub zone_id: String,
}

/// POST /devices/unlink
pub async fn device_unlink_handler(
    State(state): State<AppState>,
    Json(request): Json<DeviceUnlinkRequest>,
) -> ApiResult {
    let removed = state
        .devices
        .unlink(&request.zone_id)
        .await
        .map_err(device_error_response)?;
    Ok(Json(json!({
        "ok": true,
        "maturity": DEVICE_CONTROL_MATURITY,
        "removed": removed,
    })))
}

#[derive(Deserialize)]
pub struct DeviceCeilingRequest {
    pub device_id: String,
    /// dB; `null` clears the ceiling.
    pub ceiling_db: Option<f32>,
}

/// POST /devices/ceiling
pub async fn device_ceiling_handler(
    State(state): State<AppState>,
    Json(request): Json<DeviceCeilingRequest>,
) -> ApiResult {
    state
        .devices
        .set_ceiling(&request.device_id, request.ceiling_db)
        .await
        .map_err(device_error_response)?;
    Ok(Json(json!({
        "ok": true,
        "maturity": DEVICE_CONTROL_MATURITY,
        "device_id": request.device_id,
        "ceiling_db": request.ceiling_db,
    })))
}

#[derive(Deserialize)]
pub struct DeviceControlRequest {
    /// Address the device directly...
    #[serde(default)]
    pub device_id: Option<String>,
    /// ...or through a linked zone.
    #[serde(default)]
    pub zone_id: Option<String>,
    pub action: String,
    #[serde(default)]
    pub value: Option<Value>,
}

/// POST /devices/control — device control without a linked zone (power on an amplifier before
/// any zone plays), or through one.
pub async fn device_control_handler(
    State(state): State<AppState>,
    Json(request): Json<DeviceControlRequest>,
) -> ApiResult {
    let target = match (&request.device_id, &request.zone_id) {
        (Some(device_id), None) => {
            DeviceTarget::Device(DeviceId::parse(device_id).ok_or_else(|| {
                device_error_response(DeviceError::UnknownDevice {
                    device_id: device_id.clone(),
                })
            })?)
        }
        (None, Some(zone_id)) => DeviceTarget::Zone(zone_id.clone()),
        _ => return Err(bad_request("pass exactly one of device_id or zone_id")),
    };
    let device_request = request_from_action(&request.action, request.value.as_ref(), 0.5)
        .map_err(bad_request)?
        .ok_or_else(|| {
            bad_request(format!(
                "unknown device action {}; accepted: {}",
                request.action,
                DEVICE_ACTIONS.join(", ")
            ))
        })?;
    let report = state
        .devices
        .control(target, device_request)
        .await
        .map_err(device_error_response)?;
    Ok(Json(json!({ "ok": true, "result": report })))
}
