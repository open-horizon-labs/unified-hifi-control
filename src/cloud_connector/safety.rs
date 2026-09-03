//! Local containment remains independent of cloud availability and remote identity.
//!
//! Recovery is deliberately manual: stop UHC, resolve the incident, then remove
//! `hiphi-relay-epoch.quarantine` in the configuration directory. Wait for the
//! reconnect window to expire or explicitly remove `hiphi-relay-epoch.attempts`
//! during the same recovery. Never remove the epoch replay ledger or identity key.
//! The cloud relay needs compatible 45-minute liveness before installing a
//! connector with 15-minute steady heartbeats. State snapshots keep their own
//! 20-second schedule; playback commands do not wait for a heartbeat.
use std::{path::Path, time::Duration};

pub fn heartbeat_delay(step: u32) -> Duration {
    Duration::from_secs(match step {
        0 => 5,
        1 => 30,
        2 => 120,
        3 => 300,
        _ => 900,
    })
}

#[derive(Default)]
pub struct TrafficBudget {
    minute: u64,
    hour: u64,
    messages: u32,
    bytes: usize,
    hourly_messages: u32,
    artwork: u32,
}
impl TrafficBudget {
    pub fn admit(&mut self, now: u64, bytes: usize) -> bool {
        if now.saturating_sub(self.minute) >= 60_000 {
            self.minute = now;
            self.messages = 0;
            self.bytes = 0;
        }
        if now.saturating_sub(self.hour) >= 3_600_000 {
            self.hour = now;
            self.hourly_messages = 0;
            self.artwork = 0;
        }
        self.messages = self.messages.saturating_add(1);
        self.hourly_messages = self.hourly_messages.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        self.messages <= 120 && self.hourly_messages <= 6_000 && self.bytes <= 8 * 1024 * 1024
    }
    pub fn artwork(&mut self) -> bool {
        self.artwork = self.artwork.saturating_add(1);
        self.artwork <= 60
    }
}

pub fn quarantine(epoch_path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let path = epoch_path.with_extension("quarantine");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut f) => {
            f.write_all(
                b"Cloud traffic limit exceeded. Inspect the relay before removing this file.\n",
            )?;
            f.sync_all()
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Reserve each attempt on disk before doing network work. Restart is not a refill.
pub fn admit_reconnect(epoch_path: &Path, now: u64) -> std::io::Result<bool> {
    use std::io::Write;
    if epoch_path.with_extension("quarantine").try_exists()? {
        return Ok(false);
    }
    let path = epoch_path.with_extension("attempts");
    let (mut start, mut attempts): (u64, u32) = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).map_err(std::io::Error::other)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (now, 0),
        Err(e) => return Err(e),
    };
    if now.saturating_sub(start) >= 3_600_000 {
        start = now;
        attempts = 0;
    }
    if attempts >= 32 {
        quarantine(epoch_path)?;
        return Ok(false);
    }
    attempts += 1;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(serde_json::to_string(&(start, attempts))?.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heartbeat_cadence_slows_and_stays_bounded() {
        assert_eq!(
            (0..6)
                .map(|i| heartbeat_delay(i).as_secs())
                .collect::<Vec<_>>(),
            vec![5, 30, 120, 300, 900, 900]
        );
        assert!(heartbeat_delay(u32::MAX) < super::super::transport::PEER_HEARTBEAT_TIMEOUT);
    }
    #[test]
    fn flood_bytes_and_artwork_are_independently_bounded() {
        let mut b = TrafficBudget::default();
        for _ in 0..120 {
            assert!(b.admit(10_000_000, 100));
        }
        assert!(!b.admit(10_000_000, 100));
        let mut b = TrafficBudget::default();
        assert!(!b.admit(10_000_000, 9 * 1024 * 1024));
        for _ in 0..60 {
            assert!(b.artwork());
        }
        assert!(!b.artwork());
    }
    #[test]
    fn reconnect_budget_survives_restart_and_time_does_not_clear_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let epoch = dir.path().join("epoch");
        for _ in 0..32 {
            assert!(admit_reconnect(&epoch, 10_000).unwrap());
        }
        assert!(!admit_reconnect(&epoch, 10_000).unwrap());
        assert!(!admit_reconnect(&epoch, 10_000_000).unwrap());
    }
}
