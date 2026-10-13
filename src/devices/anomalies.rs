//! Opt-in local journal of device protocol mismatches (#774, alpha).
//!
//! Thirteen of the device families are built from specs and community clients, not hardware.
//! When a real device disagrees with that — a command it never confirms, a level it reports
//! differently, a protocol line or value the adapter does not understand — the journal remembers
//! it, so the user can choose to send a report that becomes a test fixture.
//!
//! Privacy rules:
//!
//! - **Off by default.** Nothing is recorded until the user turns recording on.
//! - **Memory only, bounded.** A ring buffer per device; nothing is written to disk; turning
//!   recording off clears everything.
//! - **Nothing leaves the machine here.** Reports are built on request, redacted, previewed, and
//!   only ever submitted by the user from GitHub's own page (`crate::devices::report`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::DeviceId;
use crate::bus::{BusEvent, SharedBus};

/// Anomalies kept per device.
pub const PER_DEVICE_CAPACITY: usize = 200;

/// What kind of disagreement between the implementation and the device was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyKind {
    /// A command was sent and the device never reported the requested state.
    Unconfirmed,
    /// The device reported a different state than requested (it applied its own limit).
    ConfirmedDifferent,
    /// A protocol line or payload the adapter models, but could not parse.
    UnparsedLine,
    /// A field value the adapter does not recognise (an undocumented mode, code or enum).
    UnknownValue,
    /// The device behaved contrary to its published capability claim.
    CapabilityContradiction,
}

impl AnomalyKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unconfirmed => "unconfirmed",
            Self::ConfirmedDifferent => "confirmed_different",
            Self::UnparsedLine => "unparsed_line",
            Self::UnknownValue => "unknown_value",
            Self::CapabilityContradiction => "capability_contradiction",
        }
    }
}

/// One observed disagreement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceAnomaly {
    pub at_ms: u64,
    pub device_id: DeviceId,
    pub kind: AnomalyKind,
    /// What was expected versus what happened, in the adapter's words.
    pub detail: String,
    /// The raw protocol excerpt, when there is one. Redacted before it is ever shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

impl DeviceAnomaly {
    pub fn new(device_id: DeviceId, kind: AnomalyKind, detail: impl Into<String>) -> Self {
        Self {
            at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            device_id,
            kind,
            detail: detail.into(),
            excerpt: None,
        }
    }

    pub fn with_excerpt(mut self, excerpt: impl Into<String>) -> Self {
        let excerpt: String = excerpt.into();
        // Bound one excerpt; a runaway payload must not dominate the buffer or a report.
        self.excerpt = Some(excerpt.chars().take(2000).collect());
        self
    }
}

/// Publish an anomaly from an adapter. Cheap when recording is off: the journal drops it.
pub fn report(bus: &SharedBus, anomaly: DeviceAnomaly) {
    bus.publish(BusEvent::DeviceAnomaly {
        anomaly: Box::new(anomaly),
    });
}

/// The bounded in-memory journal.
#[derive(Default)]
pub struct AnomalyJournal {
    enabled: AtomicBool,
    entries: Mutex<HashMap<String, VecDeque<DeviceAnomaly>>>,
}

impl AnomalyJournal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Turn recording on or off. Turning it off forgets everything recorded.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            if let Ok(mut entries) = self.entries.lock() {
                entries.clear();
            }
        }
    }

    /// Remember an anomaly, if recording is on.
    pub fn record(&self, anomaly: DeviceAnomaly) {
        if !self.is_enabled() {
            return;
        }
        if let Ok(mut entries) = self.entries.lock() {
            let buffer = entries.entry(anomaly.device_id.to_string()).or_default();
            if buffer.len() == PER_DEVICE_CAPACITY {
                buffer.pop_front();
            }
            buffer.push_back(anomaly);
        }
    }

    /// Everything recorded for one device, oldest first.
    pub fn entries(&self, device_id: &DeviceId) -> Vec<DeviceAnomaly> {
        self.entries
            .lock()
            .map(|entries| {
                entries
                    .get(device_id.as_str())
                    .map(|buffer| buffer.iter().cloned().collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Recorded anomaly counts per device.
    pub fn counts(&self) -> HashMap<String, usize> {
        self.entries
            .lock()
            .map(|entries| {
                entries
                    .iter()
                    .map(|(id, buffer)| (id.clone(), buffer.len()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Forget one device's anomalies (after the user has reported them).
    pub fn clear(&self, device_id: &DeviceId) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(device_id.as_str());
        }
    }

    /// Record anomalies adapters publish on the bus.
    pub async fn run(self: Arc<Self>, bus: SharedBus, cancel: CancellationToken) {
        let mut events = bus.subscribe();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                event = events.recv() => match event {
                    Ok(BusEvent::DeviceAnomaly { anomaly }) => self.record(*anomaly),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anomaly(n: usize) -> DeviceAnomaly {
        DeviceAnomaly::new(
            DeviceId::parse("marantz:0006781E0AB8").unwrap(),
            AnomalyKind::UnparsedLine,
            format!("line {n}"),
        )
    }

    /// Off means off: nothing is recorded, so there is nothing that could ever be reported.
    #[test]
    fn critical_nothing_is_recorded_while_recording_is_off() {
        let journal = AnomalyJournal::new();
        journal.record(anomaly(1));
        assert!(journal.counts().is_empty());
        journal.set_enabled(true);
        journal.record(anomaly(2));
        assert_eq!(journal.counts().values().sum::<usize>(), 1);
    }

    #[test]
    fn turning_recording_off_forgets_everything() {
        let journal = AnomalyJournal::new();
        journal.set_enabled(true);
        journal.record(anomaly(1));
        journal.set_enabled(false);
        assert!(journal.counts().is_empty());
    }

    #[test]
    fn the_buffer_is_bounded_and_keeps_the_newest() {
        let journal = AnomalyJournal::new();
        journal.set_enabled(true);
        for n in 0..(PER_DEVICE_CAPACITY + 10) {
            journal.record(anomaly(n));
        }
        let entries = journal.entries(&DeviceId::parse("marantz:0006781E0AB8").unwrap());
        assert_eq!(entries.len(), PER_DEVICE_CAPACITY);
        assert_eq!(entries[0].detail, "line 10");
    }

    #[test]
    fn excerpts_are_bounded() {
        let long = anomaly(0).with_excerpt("x".repeat(10_000));
        assert_eq!(long.excerpt.unwrap().len(), 2000);
    }
}
