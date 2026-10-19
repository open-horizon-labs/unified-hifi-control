//! The one place device commands and links are decided (#774).
//!
//! HTTP (`/knob/control`, `/devices/*`) and MCP (`hifi_control`, `hifi_devices`) all call this
//! service, so the safety rules below hold no matter which surface a command came from:
//!
//! - **Range before level.** Absolute and relative volume are refused until the device has
//!   reported both its current level and its hardware maximum. No level is ever guessed.
//! - **One ceiling, one place.** Every volume target is clamped to `[min, min(hardware max,
//!   ceiling)]` here, then quantised to the device's step. A surface cannot reach a driver except
//!   through this function.
//! - **Relative is resolved here, never by the device.** A relative request becomes an absolute
//!   target computed from the *observed* level, so a device-side `UP` that ignores UHC's ceiling
//!   is never sent.
//! - **Confirmed, not sent.** A command succeeds only when the device reports a readback newer
//!   than the one current at dispatch ([`crate::devices::Device::revision`]). A write whose target
//!   equals the cached value is therefore still confirmed against the device (#621), and a write
//!   the device never acknowledges is reported as unconfirmed, never as success.
//! - **One command per device at a time.** Commands to one device are serialised, so a stream of
//!   relative steps (a Watch crown) resolves each step against the previous step's readback. There
//!   is no queue that outlives a request: nothing replays after a reconnect.
//! - **No automatic input switching** (decision D2): input changes only on an explicit request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

use super::anomalies::{AnomalyJournal, AnomalyKind, DeviceAnomaly};
use super::links::{DeviceLinkStore, DeviceLinksDocument, StoreError};
use super::{Device, DeviceId, DeviceLink, DevicePower, ZoneDeviceBinding};
use crate::aggregator::ZoneAggregator;
use crate::bus::{BusEvent, SharedBus};

/// How long a command waits for the device's readback before it is reported unconfirmed.
pub const DEFAULT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(4);

/// One native operation a driver performs. Volume is always absolute and already clamped: drivers
/// never compute a level.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceOperation {
    SetVolume(f32),
    SetMute(bool),
    PowerOn,
    PowerStandby,
    SelectInput(String),
}

/// A device driver: the adapter that owns one family's sessions.
///
/// `execute` sends the operation and then asks the device for its state, so the readback that
/// confirms the command is always a fresh device report, never a cached value.
#[async_trait]
pub trait DeviceDriver: Send + Sync {
    /// The device kinds (id prefixes) this driver serves.
    fn kinds(&self) -> &'static [&'static str];

    async fn execute(&self, device_id: &DeviceId, operation: DeviceOperation)
        -> Result<(), String>;
}

/// A queued native operation keeps its caller's lifetime across worker tasks.
/// Dropped callers and expired work never acquire a native-dispatch permit.
pub(crate) struct DeviceWork {
    operation: DeviceOperation,
    reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    dispatch_by: tokio::time::Instant,
}
pub(crate) const DISPATCH_EXPIRED: &str = "device_command_expired";
impl DeviceWork {
    pub(crate) fn new(
        operation: DeviceOperation,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    ) -> Self {
        Self {
            operation,
            reply,
            dispatch_by: crate::command_dispatch::deadline(),
        }
    }
    pub(crate) fn begin_dispatch(
        self,
    ) -> Option<(
        DeviceOperation,
        tokio::sync::oneshot::Sender<Result<(), String>>,
    )> {
        if self.reply.is_closed() {
            return None;
        }
        if tokio::time::Instant::now() >= self.dispatch_by {
            let _ = self.reply.send(Err(DISPATCH_EXPIRED.into()));
            return None;
        }
        Some((self.operation, self.reply))
    }
}

/// What a caller asked for, in surface-neutral terms.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceRequest {
    /// Absolute level in dB.
    VolumeAbsolute(f32),
    /// Relative change in dB.
    VolumeRelative(f32),
    Mute(bool),
    MuteToggle,
    PowerOn,
    PowerOff,
    PowerToggle,
    SelectInput(String),
}

impl DeviceRequest {
    pub fn label(&self) -> &'static str {
        match self {
            Self::VolumeAbsolute(_) => "volume_set",
            Self::VolumeRelative(_) => "volume_step",
            Self::Mute(_) | Self::MuteToggle => "mute",
            Self::PowerOn => "power_on",
            Self::PowerOff => "power_off",
            Self::PowerToggle => "power_toggle",
            Self::SelectInput(_) => "select_input",
        }
    }
}

/// Every action string the device surfaces accept, for refusals that must list them.
pub const DEVICE_ACTIONS: &[&str] = &[
    "power_on",
    "power_off",
    "power_toggle",
    "select_input",
    "volume_set",
    "volume_up",
    "volume_down",
    "mute",
    "unmute",
];

/// Translate a surface action into a device request.
///
/// Accepts the `/knob/control` spellings (`vol_abs`, `vol_up`, `vol_down`, `vol_rel`) and the MCP
/// spellings (`volume_set`, `volume_up`, `volume_down`) so every surface shares one vocabulary.
/// Returns `Ok(None)` for an action that is not a device action (transport stays with the
/// provider).
///
/// There is **no default level**. The provider paths historically treat a missing `vol_abs`
/// value as 50; on a dB device that is a request with no safe interpretation, so it is refused.
pub fn request_from_action(
    action: &str,
    value: Option<&serde_json::Value>,
    default_step_db: f32,
) -> Result<Option<DeviceRequest>, String> {
    let number = || value.and_then(serde_json::Value::as_f64).map(|v| v as f32);
    let request = match action {
        "vol_abs" | "volume" | "volume_set" => DeviceRequest::VolumeAbsolute(
            number().ok_or_else(|| format!("{action} requires a numeric value in dB"))?,
        ),
        "vol_rel" => DeviceRequest::VolumeRelative(
            number().ok_or_else(|| "vol_rel requires a numeric dB delta".to_string())?,
        ),
        "vol_up" | "volume_up" => {
            DeviceRequest::VolumeRelative(number().map(f32::abs).unwrap_or(default_step_db))
        }
        "vol_down" | "volume_down" => {
            DeviceRequest::VolumeRelative(-number().map(f32::abs).unwrap_or(default_step_db))
        }
        "mute" => match value {
            None | Some(serde_json::Value::Null) => DeviceRequest::MuteToggle,
            Some(serde_json::Value::Bool(muted)) => DeviceRequest::Mute(*muted),
            Some(other) => match other.as_f64() {
                Some(v) => DeviceRequest::Mute(v != 0.0),
                None => return Err("mute takes true, false, or no value to toggle".to_string()),
            },
        },
        "unmute" => DeviceRequest::Mute(false),
        "power_on" => DeviceRequest::PowerOn,
        "power_off" => DeviceRequest::PowerOff,
        "power_toggle" => DeviceRequest::PowerToggle,
        "select_input" => DeviceRequest::SelectInput(
            value
                .and_then(serde_json::Value::as_str)
                .filter(|input| !input.trim().is_empty())
                .ok_or_else(|| "select_input requires the input id or name as value".to_string())?
                .to_string(),
        ),
        _ => return Ok(None),
    };
    Ok(Some(request))
}

/// Actions that only a device can carry out. On a zone with no device link they are refused
/// rather than offered to the zone's provider.
pub fn is_device_only_action(action: &str) -> bool {
    matches!(
        action,
        "power_on" | "power_off" | "power_toggle" | "select_input"
    )
}

/// Whether the device confirmed the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirmation {
    /// The device reported the requested state after the command.
    Confirmed,
    /// The device reported state after the command, but not the requested state — it applied a
    /// limit of its own. `observed` is the truth.
    ConfirmedDifferent,
}

/// The result of a confirmed command.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceCommandReport {
    pub device_id: DeviceId,
    pub operation: &'static str,
    pub confirmation: Confirmation,
    /// The level UHC sent, after clamping and quantising, for volume operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_volume: Option<f32>,
    /// True when the requested level was clamped to the effective range before sending.
    pub clamped: bool,
    /// The device as it reported itself after the command.
    pub observed: Device,
    pub maturity: &'static str,
}

/// Why a device request was not carried out. Each variant names a different caller action.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceError {
    /// No native I/O began before the request dispatch lifetime ended.
    Expired { device_id: String },
    /// The zone has no device link. Link it, or control the zone's provider instead.
    NotLinked { zone_id: String },
    /// No such device is linked or observed.
    UnknownDevice { device_id: String },
    /// The device is linked but not currently observed (offline, adapter disabled).
    DeviceUnavailable { device_id: String, detail: String },
    /// The device has not reported a range that can bound a level.
    RangeUnknown { device_id: String, detail: String },
    /// A request parameter is invalid.
    InvalidValue {
        parameter: &'static str,
        detail: String,
    },
    /// The input is not in the device's catalog.
    UnknownInput {
        device_id: String,
        input: String,
        accepted: Vec<String>,
    },
    /// The zone id names no zone UHC knows.
    UnknownZone { zone_id: String },
    /// The link store is refusing writes.
    Store(String),
    /// No driver for this device kind is running.
    NoDriver { device_id: String },
    /// The driver could not send the operation.
    Dispatch { device_id: String, detail: String },
    /// The command was sent, but the device did not report state in time. Not a success.
    Unconfirmed { device_id: String, detail: String },
}

impl DeviceError {
    /// Stable machine code, shared by HTTP `error_code` and MCP refusals.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotLinked { .. } => "DEVICE_NOT_LINKED",
            Self::UnknownDevice { .. } => "DEVICE_NOT_FOUND",
            Self::DeviceUnavailable { .. } | Self::Expired { .. } => "DEVICE_UNAVAILABLE",
            Self::RangeUnknown { .. } => "DEVICE_RANGE_UNKNOWN",
            Self::InvalidValue { .. } => "INVALID_VALUE",
            Self::UnknownInput { .. } => "DEVICE_INPUT_UNKNOWN",
            Self::UnknownZone { .. } => "ZONE_NOT_FOUND",
            Self::Store(_) => "DEVICE_LINKS_UNAVAILABLE",
            Self::NoDriver { .. } => "DEVICE_ADAPTER_NOT_RUNNING",
            Self::Dispatch { .. } => "DEVICE_DISPATCH_FAILED",
            Self::Unconfirmed { .. } => "DEVICE_UNCONFIRMED",
        }
    }

    /// HTTP status for the HTTP surfaces.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::NotLinked { .. }
            | Self::InvalidValue { .. }
            | Self::UnknownInput { .. }
            | Self::RangeUnknown { .. } => 400,
            Self::UnknownDevice { .. } | Self::UnknownZone { .. } => 404,
            Self::DeviceUnavailable { .. }
            | Self::Expired { .. }
            | Self::NoDriver { .. }
            | Self::Store(_) => 503,
            Self::Dispatch { .. } => 502,
            Self::Unconfirmed { .. } => 504,
        }
    }
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired { device_id } => write!(f, "device {device_id} command expired before dispatch"),
            Self::NotLinked { zone_id } => write!(f, "zone {zone_id} is not linked to a device"),
            Self::UnknownDevice { device_id } => write!(f, "no device {device_id}"),
            Self::DeviceUnavailable { device_id, detail } => {
                write!(f, "device {device_id} is unavailable: {detail}")
            }
            Self::RangeUnknown { device_id, detail } => write!(
                f,
                "device {device_id} has not reported a volume range, so no level can be sent: {detail}"
            ),
            Self::InvalidValue { parameter, detail } => write!(f, "invalid {parameter}: {detail}"),
            Self::UnknownInput {
                device_id,
                input,
                accepted,
            } => write!(
                f,
                "device {device_id} has no input {input}; available: {}",
                accepted.join(", ")
            ),
            Self::UnknownZone { zone_id } => write!(f, "no zone {zone_id}"),
            Self::Store(detail) => f.write_str(detail),
            Self::NoDriver { device_id } => write!(
                f,
                "no running adapter can control {device_id}; enable it in Settings"
            ),
            Self::Dispatch { device_id, detail } => {
                write!(f, "could not send to {device_id}: {detail}")
            }
            Self::Unconfirmed { device_id, detail } => write!(
                f,
                "sent to {device_id}, but the device did not confirm it: {detail}"
            ),
        }
    }
}

impl std::error::Error for DeviceError {}

impl From<StoreError> for DeviceError {
    fn from(error: StoreError) -> Self {
        Self::Store(error.to_string())
    }
}

/// Resolved target of a request: by zone (through its link) or by device id.
#[derive(Debug, Clone)]
pub enum DeviceTarget {
    Zone(String),
    Device(DeviceId),
}

pub struct DeviceService {
    aggregator: Arc<ZoneAggregator>,
    bus: SharedBus,
    store: DeviceLinkStore,
    // Serialize each persisted edit through its aggregator/journal installation.
    // The store's file lock alone ends before the installed projection updates.
    // Lock order: configuration -> store (released on return) -> aggregator.
    // Store and aggregator operations never reacquire the configuration lock.
    configuration_lock: Mutex<()>,
    drivers: RwLock<HashMap<&'static str, Arc<dyn DeviceDriver>>>,
    device_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    confirm_timeout: Duration,
    journal: Arc<AnomalyJournal>,
}

impl DeviceService {
    pub fn new(aggregator: Arc<ZoneAggregator>, bus: SharedBus, store: DeviceLinkStore) -> Self {
        Self {
            aggregator,
            bus,
            store,
            configuration_lock: Mutex::new(()),
            drivers: RwLock::new(HashMap::new()),
            device_locks: Mutex::new(HashMap::new()),
            confirm_timeout: DEFAULT_CONFIRM_TIMEOUT,
            journal: Arc::new(AnomalyJournal::new()),
        }
    }

    /// The mismatch journal (opt-in; see `crate::devices::anomalies`).
    pub fn journal(&self) -> Arc<AnomalyJournal> {
        self.journal.clone()
    }

    /// Turn mismatch recording on or off, persisting the choice. Off forgets everything recorded.
    pub async fn set_recording(&self, enabled: bool) -> Result<(), DeviceError> {
        let _configuration = self.configuration_lock.lock().await;
        self.store
            .update(move |document| document.record_mismatches = enabled)
            .await?;
        self.journal.set_enabled(enabled);
        Ok(())
    }

    /// Shorter confirmation deadline, for tests.
    pub fn with_confirm_timeout(mut self, timeout: Duration) -> Self {
        self.confirm_timeout = timeout;
        self
    }

    pub async fn register_driver(&self, driver: Arc<dyn DeviceDriver>) {
        let mut drivers = self.drivers.write().await;
        for kind in driver.kinds() {
            drivers.insert(kind, driver.clone());
        }
    }

    /// Install the persisted document into the aggregator. Call once at startup, after the
    /// aggregator is running.
    pub async fn load(&self) -> Result<(), DeviceError> {
        let _configuration = self.configuration_lock.lock().await;
        let document = self.store.document().await?;
        self.install(document).await;
        Ok(())
    }

    /// Manual hosts recorded in the link document (for drivers that cannot rely on multicast).
    pub async fn manual_hosts(&self) -> Vec<String> {
        self.store
            .document()
            .await
            .map(|document| document.manual_hosts)
            .unwrap_or_default()
    }

    /// Whether the link store can accept writes, and why not.
    pub async fn store_status(&self) -> Result<(), String> {
        self.store
            .document()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn install(&self, document: DeviceLinksDocument) {
        self.journal.set_enabled(document.record_mismatches);
        let ceilings = document
            .devices
            .iter()
            .filter_map(|(device_id, settings)| {
                settings
                    .volume_ceiling
                    .map(|ceiling| (device_id.clone(), ceiling))
            })
            .collect();
        self.aggregator
            .install_device_configuration(document.links, ceilings)
            .await;
    }

    // =========================================================================
    // Links
    // =========================================================================

    /// Link a zone to a device, optionally naming the input it arrives on. Replaces any previous
    /// link for the zone.
    ///
    /// The zone must currently exist (a typo would otherwise create a link nothing can use). The
    /// device must be a valid id of a known kind; it need not be online, so a person can link a
    /// receiver that is in standby. A `source_id` is checked against the device's catalog only
    /// when the device is observed.
    pub async fn link(
        &self,
        zone_id: &str,
        device_id: &str,
        source_id: Option<String>,
    ) -> Result<DeviceLink, DeviceError> {
        let _configuration = self.configuration_lock.lock().await;
        let device_id = DeviceId::parse(device_id).ok_or_else(|| DeviceError::InvalidValue {
            parameter: "device_id",
            detail: format!(
                "expected <kind>:<serial> with kind one of {}",
                super::DEVICE_KINDS.join(", ")
            ),
        })?;
        if self.aggregator.get_zone(zone_id).await.is_none() {
            return Err(DeviceError::UnknownZone {
                zone_id: zone_id.to_string(),
            });
        }
        let source_id = source_id.filter(|source| !source.trim().is_empty());
        if let (Some(source), Some(device)) = (
            source_id.as_deref(),
            self.aggregator.get_device(&device_id).await,
        ) {
            if !device.inputs.is_empty() && !device.inputs.iter().any(|input| input.id == source) {
                return Err(DeviceError::UnknownInput {
                    device_id: device_id.to_string(),
                    input: source.to_string(),
                    accepted: device.inputs.iter().map(|input| input.id.clone()).collect(),
                });
            }
        }
        let link = DeviceLink {
            zone_id: zone_id.to_string(),
            device_id,
            source_id,
        };
        let stored = link.clone();
        let document = self
            .store
            .update(move |document| {
                document
                    .links
                    .retain(|existing| existing.zone_id != stored.zone_id);
                document.links.push(stored);
                document.links.sort_by(|a, b| a.zone_id.cmp(&b.zone_id));
            })
            .await?;
        self.install(document).await;
        tracing::info!(zone_id, device_id = %link.device_id, "Linked zone to device");
        Ok(link)
    }

    /// Remove a zone's link. Returns whether one existed.
    pub async fn unlink(&self, zone_id: &str) -> Result<bool, DeviceError> {
        let _configuration = self.configuration_lock.lock().await;
        let existed = self.aggregator.get_device_link(zone_id).await.is_some();
        let zone = zone_id.to_string();
        let document = self
            .store
            .update(move |document| document.links.retain(|link| link.zone_id != zone))
            .await?;
        self.install(document).await;
        if existed {
            tracing::info!(zone_id, "Unlinked zone from device");
        }
        Ok(existed)
    }

    /// Set or clear a device's volume ceiling (dB). A ceiling can only lower the published maximum.
    pub async fn set_ceiling(
        &self,
        device_id: &str,
        ceiling: Option<f32>,
    ) -> Result<(), DeviceError> {
        let _configuration = self.configuration_lock.lock().await;
        let device_id = DeviceId::parse(device_id).ok_or_else(|| DeviceError::UnknownDevice {
            device_id: device_id.to_string(),
        })?;
        if let Some(ceiling) = ceiling {
            if !ceiling.is_finite() {
                return Err(DeviceError::InvalidValue {
                    parameter: "ceiling",
                    detail: "must be a finite number of dB".to_string(),
                });
            }
        }
        let document = self
            .store
            .update(move |document| {
                let settings = document.devices.entry(device_id).or_default();
                settings.volume_ceiling = ceiling;
            })
            .await?;
        self.install(document).await;
        Ok(())
    }

    // =========================================================================
    // Commands
    // =========================================================================

    /// The installed composition configuration and observed device, read together.
    /// Link/input validation and persistence stay in this service; this view is
    /// always derived from the aggregator, never a second cached authority.
    pub async fn zone_binding(&self, zone_id: &str) -> Option<ZoneDeviceBinding> {
        self.aggregator.zone_device_binding(zone_id).await
    }

    async fn resolve_device(&self, target: DeviceTarget) -> Result<DeviceId, DeviceError> {
        match target {
            DeviceTarget::Device(device_id) => Ok(device_id),
            DeviceTarget::Zone(zone_id) => self
                .zone_binding(&zone_id)
                .await
                .map(|binding| binding.link.device_id)
                .ok_or(DeviceError::NotLinked { zone_id }),
        }
    }

    /// Decode a device action using the observed device's own native step.
    /// `Ok(None)` means this vocabulary does not describe a device operation.
    /// Surfaces remain responsible only for their response/error formatting.
    pub async fn control_action(
        &self,
        target: DeviceTarget,
        action: &str,
        value: Option<&serde_json::Value>,
    ) -> Result<Option<DeviceCommandReport>, DeviceError> {
        crate::command_dispatch::local(self.control_action_unscoped(target, action, value)).await
    }

    async fn control_action_unscoped(
        &self,
        target: DeviceTarget,
        action: &str,
        value: Option<&serde_json::Value>,
    ) -> Result<Option<DeviceCommandReport>, DeviceError> {
        if matches!(request_from_action(action, value, 1.0), Ok(None)) {
            return Ok(None);
        }
        let device_id = self.resolve_device(target).await?;
        let device = self.aggregator.get_device(&device_id).await;
        let step = device
            .as_ref()
            .map(|device| device.volume.step)
            .filter(|step| step.is_finite() && *step > 0.0);
        let needs_step = matches!(action, "volume_up" | "volume_down" | "vol_up" | "vol_down")
            && value.and_then(serde_json::Value::as_f64).is_none();
        if needs_step && step.is_none() {
            return Err(DeviceError::RangeUnknown {
                device_id: device_id.to_string(),
                detail: "no observed native volume step".into(),
            });
        }
        let request =
            request_from_action(action, value, step.unwrap_or(1.0)).map_err(|detail| {
                DeviceError::InvalidValue {
                    parameter: "value",
                    detail,
                }
            })?;
        let Some(request) = request else {
            return Ok(None);
        };
        self.control(DeviceTarget::Device(device_id), request)
            .await
            .map(Some)
    }

    /// Convert a normalized client percentage only when the observed native
    /// scale has a defined linear mapping. dB and unknown scales are refused.
    pub async fn control_percentage(
        &self,
        target: DeviceTarget,
        value: f64,
    ) -> Result<DeviceCommandReport, DeviceError> {
        crate::command_dispatch::local(self.control_percentage_unscoped(target, value)).await
    }

    async fn control_percentage_unscoped(
        &self,
        target: DeviceTarget,
        value: f64,
    ) -> Result<DeviceCommandReport, DeviceError> {
        if !value.is_finite() {
            return Err(DeviceError::InvalidValue {
                parameter: "value",
                detail: "volume must be finite".into(),
            });
        }
        let device_id = self.resolve_device(target).await?;
        let volume = self
            .aggregator
            .get_device(&device_id)
            .await
            .and_then(|device| device.volume_control())
            .ok_or_else(|| DeviceError::RangeUnknown {
                device_id: device_id.to_string(),
                detail: "no observed bounded volume range".into(),
            })?;
        let native =
            crate::volume::native(&volume, value).ok_or_else(|| DeviceError::InvalidValue {
                parameter: "value",
                detail: "percentage volume is unavailable for this device scale".into(),
            })?;
        self.control(
            DeviceTarget::Device(device_id),
            DeviceRequest::VolumeAbsolute(native as f32),
        )
        .await
    }

    /// Resolve the device a zone's surface controls, when the zone is linked.
    pub async fn linked_device_id(&self, zone_id: &str) -> Option<DeviceId> {
        self.zone_binding(zone_id)
            .await
            .map(|binding| binding.link.device_id)
    }

    /// Carry out a request and wait for the device to confirm it. Outcomes that show the device
    /// disagreeing with the implementation are recorded in the mismatch journal (when enabled).
    pub async fn control(
        &self,
        target: DeviceTarget,
        request: DeviceRequest,
    ) -> Result<DeviceCommandReport, DeviceError> {
        crate::command_dispatch::local(self.control_unscoped(target, request)).await
    }

    async fn control_unscoped(
        &self,
        target: DeviceTarget,
        request: DeviceRequest,
    ) -> Result<DeviceCommandReport, DeviceError> {
        let label = request.label();
        let result = self.control_inner(target, request).await;
        match &result {
            Ok(report) if report.confirmation == Confirmation::ConfirmedDifferent => {
                self.journal.record(DeviceAnomaly::new(
                    report.device_id.clone(),
                    AnomalyKind::ConfirmedDifferent,
                    format!(
                        "{label}: sent {:?}, device reported {:?}",
                        report.sent_volume, report.observed.volume.value
                    ),
                ));
            }
            Err(DeviceError::Unconfirmed { device_id, detail }) => {
                if let Some(device_id) = DeviceId::parse(device_id) {
                    self.journal.record(DeviceAnomaly::new(
                        device_id,
                        AnomalyKind::Unconfirmed,
                        format!("{label}: {detail}"),
                    ));
                }
            }
            _ => {}
        }
        result
    }

    async fn control_inner(
        &self,
        target: DeviceTarget,
        request: DeviceRequest,
    ) -> Result<DeviceCommandReport, DeviceError> {
        let device_id = self.resolve_device(target).await?;
        let lock = self.device_lock(&device_id).await;
        let dispatch_by = crate::command_dispatch::deadline();
        let expired = || DeviceError::Expired {
            device_id: device_id.to_string(),
        };
        let _serialised = tokio::time::timeout_at(dispatch_by, lock.lock())
            .await
            .map_err(|_| expired())?;

        let device = self
            .aggregator
            .get_device(&device_id)
            .await
            .ok_or_else(|| DeviceError::DeviceUnavailable {
                device_id: device_id.to_string(),
                detail: "not currently observed; check that it is on the network and its adapter \
                         is enabled"
                    .to_string(),
            })?;
        let driver = self
            .drivers
            .read()
            .await
            .get(device_id.kind())
            .cloned()
            .ok_or_else(|| DeviceError::NoDriver {
                device_id: device_id.to_string(),
            })?;

        let plan = plan(&device, &request)?;

        if tokio::time::Instant::now() >= dispatch_by {
            return Err(expired());
        }
        let deadline = tokio::time::Instant::now() + self.confirm_timeout;
        // Subscribe before dispatch: the readback may arrive before `execute` returns.
        let mut events = self.bus.subscribe();
        let revision_at_dispatch = device.revision;
        tokio::time::timeout_at(deadline, driver.execute(&device_id, plan.operation.clone()))
            .await
            .map_err(|_| DeviceError::Unconfirmed {
                device_id: device_id.to_string(),
                detail: "native dispatch did not finish within the confirmation budget".into(),
            })?
            .map_err(|detail| {
                if detail == DISPATCH_EXPIRED {
                    expired()
                } else {
                    DeviceError::Dispatch {
                        device_id: device_id.to_string(),
                        detail,
                    }
                }
            })?;

        let mut latest_readback: Option<Device> = None;
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv()).await;
            match event {
                Ok(Ok(BusEvent::DeviceObserved { device: observed }))
                    if observed.device_id == device_id
                        && observed.revision > revision_at_dispatch =>
                {
                    if plan.satisfied_by(&observed) {
                        return Ok(plan.report(device_id, Confirmation::Confirmed, *observed));
                    }
                    latest_readback = Some(*observed);
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                    // A lagged receiver missed events, possibly our readback. Fall back to the
                    // aggregator, which applied every event in order.
                    if let Some(current) = self.aggregator.get_device(&device_id).await {
                        if current.revision > revision_at_dispatch {
                            if plan.satisfied_by(&current) {
                                return Ok(plan.report(
                                    device_id,
                                    Confirmation::Confirmed,
                                    current,
                                ));
                            }
                            latest_readback = Some(current);
                        }
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => break,
            }
        }

        match latest_readback {
            // The device answered after the command and *moved*, but not to the target: it applied
            // a limit of its own. Report the truth. A readback that has not moved at all is not a
            // device limit — the command may simply not have been applied — so it stays unconfirmed.
            Some(observed)
                if plan.accepts_device_limit && observed.volume.value != device.volume.value =>
            {
                Ok(plan.report(device_id, Confirmation::ConfirmedDifferent, observed))
            }
            Some(observed) => Err(DeviceError::Unconfirmed {
                device_id: device_id.to_string(),
                detail: format!(
                    "the device reported {} after the command",
                    describe(&observed, &plan.operation)
                ),
            }),
            None => Err(DeviceError::Unconfirmed {
                device_id: device_id.to_string(),
                detail: format!("no readback within {} ms", self.confirm_timeout.as_millis()),
            }),
        }
    }

    async fn device_lock(&self, device_id: &DeviceId) -> Arc<Mutex<()>> {
        self.device_locks
            .lock()
            .await
            .entry(device_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

/// A request resolved against the device's observed state.
struct Plan {
    operation: DeviceOperation,
    sent_volume: Option<f32>,
    clamped: bool,
    /// Whether a readback that differs from the target is still a truthful completion (a device
    /// clamping a level to its own limit) rather than a failure.
    accepts_device_limit: bool,
}

impl Plan {
    fn satisfied_by(&self, device: &Device) -> bool {
        match &self.operation {
            DeviceOperation::SetVolume(target) => device
                .volume
                .value
                .is_some_and(|observed| (observed - target).abs() < 0.01),
            DeviceOperation::SetMute(muted) => device.volume.is_muted == Some(*muted),
            DeviceOperation::PowerOn => device.power == DevicePower::On,
            DeviceOperation::PowerStandby => device.power == DevicePower::Standby,
            DeviceOperation::SelectInput(input) => device.input.as_deref() == Some(input.as_str()),
        }
    }

    fn report(
        &self,
        device_id: DeviceId,
        confirmation: Confirmation,
        observed: Device,
    ) -> DeviceCommandReport {
        DeviceCommandReport {
            device_id,
            operation: operation_label(&self.operation),
            confirmation,
            sent_volume: self.sent_volume,
            clamped: self.clamped,
            observed,
            maturity: super::DEVICE_CONTROL_MATURITY,
        }
    }
}

fn operation_label(operation: &DeviceOperation) -> &'static str {
    match operation {
        DeviceOperation::SetVolume(_) => "volume_set",
        DeviceOperation::SetMute(_) => "mute",
        DeviceOperation::PowerOn => "power_on",
        DeviceOperation::PowerStandby => "power_off",
        DeviceOperation::SelectInput(_) => "select_input",
    }
}

fn describe(device: &Device, operation: &DeviceOperation) -> String {
    match operation {
        DeviceOperation::SetVolume(_) => match device.volume.value {
            Some(db) => format!("{db} dB"),
            None => "no level".to_string(),
        },
        DeviceOperation::SetMute(_) => format!("mute {:?}", device.volume.is_muted),
        DeviceOperation::PowerOn | DeviceOperation::PowerStandby => {
            format!("power {:?}", device.power)
        }
        DeviceOperation::SelectInput(_) => format!("input {:?}", device.input),
    }
}

/// Resolve a request against observed state. Pure, so every safety rule is unit-testable.
fn plan(device: &Device, request: &DeviceRequest) -> Result<Plan, DeviceError> {
    let device_id = device.device_id.to_string();
    let capability = match request {
        DeviceRequest::VolumeAbsolute(_) | DeviceRequest::VolumeRelative(_) => {
            &device.capabilities.volume
        }
        DeviceRequest::Mute(_) | DeviceRequest::MuteToggle => &device.capabilities.mute,
        DeviceRequest::PowerOn | DeviceRequest::PowerOff | DeviceRequest::PowerToggle => {
            &device.capabilities.power
        }
        DeviceRequest::SelectInput(_) => &device.capabilities.input_select,
    };
    if !capability.supported {
        if matches!(
            request,
            DeviceRequest::VolumeAbsolute(_) | DeviceRequest::VolumeRelative(_)
        ) {
            return Err(DeviceError::RangeUnknown {
                device_id: device_id.clone(),
                detail: "volume is not supported by this device".into(),
            });
        }
        return Err(DeviceError::InvalidValue {
            parameter: "action",
            detail: format!(
                "{} is not supported by this device: {}",
                request.label(),
                capability.detail
            ),
        });
    }
    let volume_plan = |target_db: f32| -> Result<Plan, DeviceError> {
        let max = device
            .volume
            .effective_max()
            .ok_or_else(|| DeviceError::RangeUnknown {
                device_id: device_id.clone(),
                detail: "the device has not reported its maximum level".to_string(),
            })?;
        let min = device.volume.min;
        if !(min.is_finite() && max.is_finite() && min < max) {
            return Err(DeviceError::RangeUnknown {
                device_id: device_id.clone(),
                detail: "the reported range cannot bound a level".to_string(),
            });
        }
        let clamped_target = target_db.clamp(min, max);
        let step = if device.volume.step > 0.0 {
            device.volume.step
        } else {
            0.5
        };
        // Quantise *down* to a step so the result can never exceed the clamped maximum.
        let quantised = ((clamped_target - min) / step).floor() * step + min;
        let quantised = (quantised * 100.0).round() / 100.0;
        Ok(Plan {
            operation: DeviceOperation::SetVolume(quantised),
            sent_volume: Some(quantised),
            clamped: (clamped_target - target_db).abs() > f32::EPSILON,
            accepts_device_limit: true,
        })
    };
    match request {
        DeviceRequest::VolumeAbsolute(db) => {
            if !db.is_finite() {
                return Err(DeviceError::InvalidValue {
                    parameter: "value",
                    detail: "volume must be a finite number of dB".to_string(),
                });
            }
            volume_plan(*db)
        }
        DeviceRequest::VolumeRelative(delta) => {
            if !delta.is_finite() {
                return Err(DeviceError::InvalidValue {
                    parameter: "value",
                    detail: "volume step must be a finite number of dB".to_string(),
                });
            }
            let current = device
                .volume
                .value
                .ok_or_else(|| DeviceError::RangeUnknown {
                    device_id: device_id.clone(),
                    detail: "the device has not reported its current level".to_string(),
                })?;
            // A step finer than the device's own would quantise back to the current level (up)
            // or a whole step (down); widen it to one device step so both directions move.
            let device_step = device.volume.step;
            let delta = if device_step > 0.0 && delta.abs() < device_step {
                device_step.copysign(*delta)
            } else {
                *delta
            };
            volume_plan(current + delta)
        }
        DeviceRequest::Mute(muted) => Ok(Plan {
            operation: DeviceOperation::SetMute(*muted),
            sent_volume: None,
            clamped: false,
            accepts_device_limit: false,
        }),
        DeviceRequest::MuteToggle => {
            // Expressed as a set to the opposite of the *observed* state, never a device toggle:
            // a toggle sent twice by a retry would undo itself.
            let muted = device
                .volume
                .is_muted
                .ok_or_else(|| DeviceError::DeviceUnavailable {
                    device_id: device_id.clone(),
                    detail: "mute state not yet observed".to_string(),
                })?;
            Ok(Plan {
                operation: DeviceOperation::SetMute(!muted),
                sent_volume: None,
                clamped: false,
                accepts_device_limit: false,
            })
        }
        DeviceRequest::PowerOn => Ok(power_plan(DeviceOperation::PowerOn)),
        DeviceRequest::PowerOff => Ok(power_plan(DeviceOperation::PowerStandby)),
        DeviceRequest::PowerToggle => match device.power {
            DevicePower::On => Ok(power_plan(DeviceOperation::PowerStandby)),
            DevicePower::Standby => Ok(power_plan(DeviceOperation::PowerOn)),
            DevicePower::Unknown => Err(DeviceError::DeviceUnavailable {
                device_id,
                detail: "power state not yet observed, so a toggle has no direction".to_string(),
            }),
        },
        DeviceRequest::SelectInput(input) => {
            let input = input.trim();
            if device.inputs.is_empty() {
                return Err(DeviceError::DeviceUnavailable {
                    device_id,
                    detail: "the input catalog has not been read yet".to_string(),
                });
            }
            let Some(entry) = device.inputs.iter().find(|candidate| {
                candidate.id == input || candidate.name.eq_ignore_ascii_case(input)
            }) else {
                return Err(DeviceError::UnknownInput {
                    device_id,
                    input: input.to_string(),
                    accepted: device
                        .inputs
                        .iter()
                        .map(|candidate| candidate.id.clone())
                        .collect(),
                });
            };
            Ok(Plan {
                operation: DeviceOperation::SelectInput(entry.id.clone()),
                sent_volume: None,
                clamped: false,
                accepts_device_limit: false,
            })
        }
    }
}

fn power_plan(operation: DeviceOperation) -> Plan {
    Plan {
        operation,
        sent_volume: None,
        clamped: false,
        accepts_device_limit: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{
        CapabilityEvidence, ControlChannel, DeviceCapabilities, DeviceCapability, DeviceConnection,
        DeviceInput, DeviceVolume,
    };

    pub(crate) fn device(db: Option<f32>, hardware_max: Option<f32>) -> Device {
        let capability = DeviceCapability {
            supported: true,
            evidence: CapabilityEvidence::ImplementationDerived,
            detail: String::new(),
        };
        Device {
            device_id: DeviceId::parse("marantz:0006781E0AB8").unwrap(),
            vendor: "Marantz".to_string(),
            model: "SR6008".to_string(),
            name: "front".to_string(),
            host: "192.0.2.10".to_string(),
            discovery: "ssdp".to_string(),
            maturity: "alpha".to_string(),
            connection: DeviceConnection {
                control_channel: ControlChannel::Push,
                detail: None,
            },
            power: DevicePower::On,
            input: Some("MPLAY".to_string()),
            inputs: vec![
                DeviceInput {
                    id: "MPLAY".to_string(),
                    name: "WiiM".to_string(),
                    available: true,
                },
                DeviceInput {
                    id: "TV".to_string(),
                    name: "TV AUDIO".to_string(),
                    available: true,
                },
            ],
            volume: DeviceVolume {
                scale: crate::bus::VolumeScale::Decibel,
                value: db,
                min: -80.0,
                hardware_max: hardware_max,
                ceiling: None,
                step: 0.5,
                is_muted: Some(false),
                display: Some("absolute".to_string()),
            },
            capabilities: DeviceCapabilities {
                observe: capability.clone(),
                volume: capability.clone(),
                mute: capability.clone(),
                power: capability.clone(),
                input_select: capability,
            },
            revision: 1,
            last_updated: 0,
        }
    }

    fn sent_level(plan: Result<Plan, DeviceError>) -> f32 {
        match plan.expect("plan").operation {
            DeviceOperation::SetVolume(db) => db,
            other => panic!("expected a volume operation, got {other:?}"),
        }
    }

    /// CRITICAL: no level may be sent to a device whose adapter publishes no scale at all (for
    /// example a unit that has not yet reported state). Adapters with a known protocol scale
    /// publish it as the maximum, so this does not gate them.
    #[test]
    fn critical_vol_abs_refused_until_range_read() {
        let unknown_max = device(Some(-40.0), None);
        assert!(matches!(
            plan(&unknown_max, &DeviceRequest::VolumeAbsolute(-30.0)),
            Err(DeviceError::RangeUnknown { .. })
        ));
        assert!(matches!(
            plan(&unknown_max, &DeviceRequest::VolumeRelative(0.5)),
            Err(DeviceError::RangeUnknown { .. })
        ));
        let unknown_level = device(None, Some(-10.0));
        assert!(matches!(
            plan(&unknown_level, &DeviceRequest::VolumeRelative(0.5)),
            Err(DeviceError::RangeUnknown { .. })
        ));
    }

    /// CRITICAL: a request above the device maximum is clamped to it, never sent as asked.
    #[test]
    fn critical_requests_above_the_hardware_maximum_are_clamped() {
        let amp = device(Some(-40.0), Some(-10.0));
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeAbsolute(18.0))),
            -10.0
        );
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeAbsolute(100.0))),
            -10.0
        );
    }

    /// CRITICAL (#246, O1): the user ceiling bounds every volume request, absolute or relative.
    #[test]
    fn critical_ceiling_bounds_absolute_and_relative_requests() {
        let mut amp = device(Some(-31.0), Some(-10.0));
        amp.volume.ceiling = Some(-30.0);
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeAbsolute(-5.0))),
            -30.0
        );
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeRelative(5.0))),
            -30.0
        );
        // A level already above the ceiling (set before the ceiling existed) is pulled down by
        // the next relative step rather than climbing further.
        amp.volume.value = Some(-20.0);
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeRelative(0.5))),
            -30.0
        );
    }

    /// CRITICAL: -12 dB stays -12 dB. The v2.5 incident clamped dB into 0..100 and sent 0 dB.
    #[test]
    fn critical_db_levels_are_not_rescaled() {
        let amp = device(Some(-40.0), Some(-10.0));
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeAbsolute(-12.0))),
            -12.0
        );
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeAbsolute(-80.0))),
            -80.0
        );
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeAbsolute(-200.0))),
            -80.0
        );
    }

    #[test]
    fn levels_quantise_down_to_the_device_step() {
        let amp = device(Some(-40.0), Some(-10.0));
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeAbsolute(-39.7))),
            -40.0
        );
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeAbsolute(-39.5))),
            -39.5
        );
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeRelative(0.5))),
            -39.5
        );
        assert_eq!(
            sent_level(plan(&amp, &DeviceRequest::VolumeRelative(-0.5))),
            -40.5
        );
    }

    /// A relative step smaller than the device's own step must still move the volume, in both
    /// directions. The tempting bug: quantising 55 + 0.5 down to 55 makes "+" a silent no-op on
    /// a 1%-step WiiM while "−" (55 − 0.5 → 54.5 → 54) appears to work.
    #[test]
    fn critical_relative_steps_below_the_device_step_still_move_the_volume() {
        let mut wiim = device(Some(55.0), Some(100.0));
        wiim.volume.scale = crate::bus::VolumeScale::Percentage;
        wiim.volume.min = 0.0;
        wiim.volume.step = 1.0;
        assert_eq!(
            sent_level(plan(&wiim, &DeviceRequest::VolumeRelative(0.5))),
            56.0
        );
        assert_eq!(
            sent_level(plan(&wiim, &DeviceRequest::VolumeRelative(-0.5))),
            54.0
        );
        // A full step behaves as before.
        assert_eq!(
            sent_level(plan(&wiim, &DeviceRequest::VolumeRelative(1.0))),
            56.0
        );
        // At the maximum, "+" stays at the maximum rather than overshooting.
        wiim.volume.value = Some(100.0);
        assert_eq!(
            sent_level(plan(&wiim, &DeviceRequest::VolumeRelative(0.5))),
            100.0
        );
    }

    #[test]
    fn non_finite_levels_are_refused() {
        let amp = device(Some(-40.0), Some(-10.0));
        assert!(plan(&amp, &DeviceRequest::VolumeAbsolute(f32::NAN)).is_err());
        assert!(plan(&amp, &DeviceRequest::VolumeRelative(f32::INFINITY)).is_err());
    }

    /// A toggle is resolved to an explicit set against observed state, so a retried toggle can
    /// never undo itself.
    #[test]
    fn mute_toggle_becomes_an_explicit_set() {
        let amp = device(Some(-40.0), Some(-10.0));
        let resolved = plan(&amp, &DeviceRequest::MuteToggle).unwrap();
        assert_eq!(resolved.operation, DeviceOperation::SetMute(true));
    }

    #[test]
    fn inputs_resolve_by_code_or_display_name_and_unknown_inputs_are_refused() {
        let amp = device(Some(-40.0), Some(-10.0));
        assert_eq!(
            plan(&amp, &DeviceRequest::SelectInput("wiim".to_string()))
                .unwrap()
                .operation,
            DeviceOperation::SelectInput("MPLAY".to_string())
        );
        assert_eq!(
            plan(&amp, &DeviceRequest::SelectInput("TV".to_string()))
                .unwrap()
                .operation,
            DeviceOperation::SelectInput("TV".to_string())
        );
        assert!(matches!(
            plan(&amp, &DeviceRequest::SelectInput("PHONO".to_string())),
            Err(DeviceError::UnknownInput { .. })
        ));
    }

    /// CRITICAL: the provider paths default a missing `vol_abs` to 50. On a dB device that is
    /// +18 dB clamped to the maximum — so a device request with no level must be refused.
    #[test]
    fn critical_missing_absolute_level_is_refused_not_defaulted() {
        assert!(request_from_action("vol_abs", None, 0.5).is_err());
        assert!(request_from_action("volume_set", Some(&serde_json::json!("loud")), 0.5).is_err());
        assert_eq!(
            request_from_action("vol_abs", Some(&serde_json::json!(-35.5)), 0.5).unwrap(),
            Some(DeviceRequest::VolumeAbsolute(-35.5))
        );
    }

    #[test]
    fn relative_actions_use_the_device_step_and_ignore_sign_tricks() {
        assert_eq!(
            request_from_action("vol_up", None, 0.5).unwrap(),
            Some(DeviceRequest::VolumeRelative(0.5))
        );
        // A negative "up" amount still goes up; direction comes from the action.
        assert_eq!(
            request_from_action("vol_up", Some(&serde_json::json!(-2.0)), 0.5).unwrap(),
            Some(DeviceRequest::VolumeRelative(2.0))
        );
        assert_eq!(
            request_from_action("volume_down", Some(&serde_json::json!(1.5)), 0.5).unwrap(),
            Some(DeviceRequest::VolumeRelative(-1.5))
        );
        assert_eq!(request_from_action("play", None, 0.5).unwrap(), None);
    }

    /// Power-on is only a power operation. It never carries a level, so the amplifier wakes at
    /// whatever level it restores itself, never at one UHC computed from a stale cache.
    #[test]
    fn critical_power_on_never_carries_a_level() {
        let amp = device(Some(-40.0), Some(-10.0));
        let resolved = plan(&amp, &DeviceRequest::PowerOn).unwrap();
        assert_eq!(resolved.operation, DeviceOperation::PowerOn);
        assert_eq!(resolved.sent_volume, None);
    }
}

#[cfg(test)]
mod admission_regressions {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn waiting_device_action_has_a_bounded_dispatch_lifetime() {
        let directory = tempfile::tempdir().unwrap();
        let bus = crate::bus::create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let service = DeviceService::new(
            aggregator,
            bus,
            DeviceLinkStore::open(directory.path().join("links.json")),
        );
        let id = DeviceId::new("marantz", "FAKEADMISSION").unwrap();
        let lock = service.device_lock(&id).await;
        let _admission_test_blocker = lock.lock().await;
        let result = tokio::time::timeout(
            Duration::from_secs(4),
            service.control(DeviceTarget::Device(id), DeviceRequest::PowerOn),
        )
        .await;
        assert!(
            result.is_ok(),
            "device admission must expire instead of waiting indefinitely"
        );
    }

    #[test]
    fn unsupported_capabilities_are_refused_before_driver_dispatch() {
        let mut device = super::tests::device(Some(-40.0), Some(-10.0));
        device.capabilities.power.supported = false;
        assert!(plan(&device, &DeviceRequest::PowerOn).is_err());
    }
}

#[cfg(test)]
mod worker_lifetime_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn expired_and_cancelled_native_work_never_gets_a_dispatch_permit() {
        let (reply, result) = tokio::sync::oneshot::channel();
        let work = crate::command_dispatch::until(
            tokio::time::Instant::now() + Duration::from_millis(10),
            async { DeviceWork::new(DeviceOperation::PowerOn, reply) },
        )
        .await;
        tokio::time::advance(Duration::from_millis(11)).await;
        assert!(work.begin_dispatch().is_none());
        assert_eq!(result.await.unwrap(), Err(DISPATCH_EXPIRED.into()));
        let (reply, result) = tokio::sync::oneshot::channel();
        let work = DeviceWork::new(DeviceOperation::PowerOn, reply);
        drop(result);
        assert!(work.begin_dispatch().is_none());
        let (reply, result) = tokio::sync::oneshot::channel();
        let (operation, reply) = DeviceWork::new(DeviceOperation::PowerOn, reply)
            .begin_dispatch()
            .unwrap();
        assert_eq!(operation, DeviceOperation::PowerOn);
        reply.send(Ok(())).unwrap();
        assert_eq!(result.await.unwrap(), Ok(()));
    }
    #[test]
    fn every_device_capability_is_checked_before_planning_and_projection() {
        let mut device = super::tests::device(Some(-40.0), Some(-10.0));
        device.capabilities.volume.supported = false;
        device.capabilities.mute.supported = false;
        device.capabilities.power.supported = false;
        device.capabilities.input_select.supported = false;
        assert!(device.volume_control().is_none());
        for request in [
            DeviceRequest::VolumeAbsolute(-45.0),
            DeviceRequest::VolumeRelative(0.5),
            DeviceRequest::Mute(true),
            DeviceRequest::MuteToggle,
            DeviceRequest::PowerOn,
            DeviceRequest::PowerOff,
            DeviceRequest::PowerToggle,
            DeviceRequest::SelectInput("TV".into()),
        ] {
            assert!(matches!(
                plan(&device, &request),
                Err(DeviceError::InvalidValue {
                    parameter: "action",
                    ..
                } | DeviceError::RangeUnknown { .. })
            ));
        }
    }
}

#[cfg(test)]
mod caller_deadline_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn caller_authority_expiry_bounds_device_lock_admission() {
        let directory = tempfile::tempdir().unwrap();
        let bus = crate::bus::create_bus();
        let service = DeviceService::new(
            Arc::new(ZoneAggregator::new(bus.clone())),
            bus,
            DeviceLinkStore::open(directory.path().join("links.json")),
        );
        let id = DeviceId::new("marantz", "FAKEDEADLINE").unwrap();
        let lock = service.device_lock(&id).await;
        let _admission_test_blocker = lock.lock().await;
        let by = tokio::time::Instant::now() + Duration::from_millis(20);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            crate::command_dispatch::until(
                by,
                service.control(DeviceTarget::Device(id), DeviceRequest::PowerOn),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(DeviceError::Expired { .. })));
        drop(_admission_test_blocker);
    }
}
