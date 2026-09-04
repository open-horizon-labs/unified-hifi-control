//! Local containment remains independent of cloud availability and remote identity.
//!
//! Prefer Settings → Resume Cloud connection after resolving the incident.
//! For manual recovery, stop UHC, resolve the incident, then remove
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

/// Inspect persisted containment without making a network attempt. Invalid
/// state is not a cost quarantine and must not be erased by the resume action.
pub fn pause_reason(epoch_path: &Path) -> Option<&'static str> {
    match inspect(epoch_path) {
        Ok(true) => Some("cost_limit"),
        Ok(false) => None,
        Err(_) => Some("safety_state_unavailable"),
    }
}

fn read_regular(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => std::fs::read_to_string(path).map(Some),
        Ok(_) => Err(std::io::Error::other("safety state is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn inspect(epoch_path: &Path) -> std::io::Result<bool> {
    if let Some(previous) = read_regular(&epoch_path.with_extension("resume"))? {
        previous.parse::<u64>().map_err(std::io::Error::other)?;
    }
    if let Some(attempts) = read_regular(&epoch_path.with_extension("attempts"))? {
        serde_json::from_str::<(u64, u32)>(&attempts).map_err(std::io::Error::other)?;
    }
    Ok(read_regular(&epoch_path.with_extension("quarantine"))?.is_some())
}

const RESUME_COOLDOWN_MS: u64 = 15 * 60 * 1000;

fn check_resume_cooldown(epoch_path: &Path, now: u64) -> std::io::Result<()> {
    if let Some(previous) = read_regular(&epoch_path.with_extension("resume"))? {
        let previous: u64 = previous.parse().map_err(std::io::Error::other)?;
        if now.saturating_sub(previous) < RESUME_COOLDOWN_MS {
            return Err(std::io::Error::other(
                "Wait 15 minutes between Cloud recovery attempts.",
            ));
        }
    }
    Ok(())
}

pub fn can_resume(epoch_path: &Path, now: u64) -> bool {
    pause_reason(epoch_path) == Some("cost_limit") && check_resume_cooldown(epoch_path, now).is_ok()
}

/// Called only while the supervisor excludes startup and the connector is
/// stopped. Commit the cooldown before clearing counters; remove the stop flag
/// last, so failed writes/removals never accidentally release containment.
pub fn resume(epoch_path: &Path, now: u64) -> std::io::Result<()> {
    if !inspect(epoch_path)? {
        return Err(std::io::Error::other(
            "Cloud is not paused by a cost limit.",
        ));
    }
    check_resume_cooldown(epoch_path, now)?;
    let path = epoch_path.with_extension("resume");
    let temporary = epoch_path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(now.to_string().as_bytes())?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        if let Some(parent) = epoch_path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        match std::fs::remove_file(epoch_path.with_extension("attempts")) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        std::fs::remove_file(epoch_path.with_extension("quarantine"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
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
    fn deliberate_resume_preserves_replay_and_retrips_with_persistent_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let epoch = dir.path().join("hiphi-relay-epoch");
        std::fs::write(&epoch, "replay ledger must survive").unwrap();
        let now = 1_000_000;
        for _ in 0..32 {
            assert!(admit_reconnect(&epoch, now).unwrap());
        }
        assert!(!admit_reconnect(&epoch, now).unwrap());
        assert_eq!(pause_reason(&epoch), Some("cost_limit"));
        resume(&epoch, now).unwrap();
        assert_eq!(
            std::fs::read_to_string(&epoch).unwrap(),
            "replay ledger must survive"
        );
        assert_eq!(pause_reason(&epoch), None);
        for _ in 0..32 {
            assert!(admit_reconnect(&epoch, now).unwrap());
        }
        assert!(!admit_reconnect(&epoch, now).unwrap());
        assert!(resume(&epoch, now + 1).is_err());
        assert_eq!(pause_reason(&epoch), Some("cost_limit"));
        resume(&epoch, now + 900_000).unwrap();
    }

    #[test]
    fn resume_does_not_hide_corrupt_state_or_failed_updates() {
        let dir = tempfile::tempdir().unwrap();
        let epoch = dir.path().join("hiphi-relay-epoch");
        quarantine(&epoch).unwrap();
        std::fs::write(epoch.with_extension("attempts"), "broken").unwrap();
        assert_eq!(pause_reason(&epoch), Some("safety_state_unavailable"));
        assert!(resume(&epoch, 1_000_000).is_err());
        assert!(epoch.with_extension("quarantine").exists());
        std::fs::remove_file(epoch.with_extension("attempts")).unwrap();
        std::fs::create_dir(epoch.with_extension("resume")).unwrap();
        assert!(resume(&epoch, 1_000_000).is_err());
        assert!(epoch.with_extension("quarantine").exists());
    }

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
