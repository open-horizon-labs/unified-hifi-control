//! The playback-device adapter set (#774, alpha).
//!
//! Every device family (Denon/Marantz, WiiM, MusicCast, …) implements [`DeviceFamily`]. One
//! Startable, `devices`, starts and stops them all behind a single Settings toggle, and each is
//! registered with [`crate::devices::service::DeviceService`] as the driver for its kinds.
//!
//! A family adapter's whole contract:
//!
//! - publish `BusEvent::DeviceObserved` snapshots (every readback, revision incremented) and
//!   `BusEvent::DeviceRemoved` when it stops seeing a unit;
//! - execute `DeviceOperation`s it is handed — already resolved, clamped to the device's native
//!   scale and quantised. A family never computes a level;
//! - after every write, read the state back so confirmation sees a fresh device report;
//! - send no state-changing command on connect or reconnect.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::devices::links::ManualDevice;
use crate::devices::service::DeviceDriver;

/// Configuration a family receives when the set starts.
#[derive(Debug, Clone, Default)]
pub struct FamilyContext {
    /// Manually configured devices for this family (family field already matched).
    pub manual: Vec<ManualDevice>,
}

/// One device family's adapter.
#[async_trait]
pub trait DeviceFamily: DeviceDriver {
    /// Stable family name, used in logs and `ManualDevice::family`.
    fn family(&self) -> &'static str;
    async fn start_family(&self, context: FamilyContext) -> anyhow::Result<()>;
    async fn stop_family(&self);
}

/// Every device family, started and stopped as one adapter.
pub struct DeviceAdapterSet {
    families: Vec<Arc<dyn DeviceFamily>>,
    manual: RwLock<Vec<ManualDevice>>,
}

impl DeviceAdapterSet {
    pub fn new(families: Vec<Arc<dyn DeviceFamily>>) -> Self {
        Self {
            families,
            manual: RwLock::new(Vec::new()),
        }
    }

    pub fn families(&self) -> &[Arc<dyn DeviceFamily>] {
        &self.families
    }

    pub async fn set_manual_devices(&self, manual: Vec<ManualDevice>) {
        *self.manual.write().await = manual;
    }

    async fn start_internal(&self) -> anyhow::Result<()> {
        let manual = self.manual.read().await.clone();
        for family in &self.families {
            let context = FamilyContext {
                manual: manual
                    .iter()
                    .filter(|device| device.family == family.family())
                    .cloned()
                    .collect(),
            };
            // One family failing to start must not keep the others from running.
            if let Err(error) = family.start_family(context).await {
                tracing::warn!(
                    family = family.family(),
                    "device family failed to start: {error}"
                );
            }
        }
        Ok(())
    }

    async fn stop_internal(&self) {
        for family in &self.families {
            family.stop_family().await;
        }
    }
}

crate::impl_startable!(DeviceAdapterSet, "devices");
