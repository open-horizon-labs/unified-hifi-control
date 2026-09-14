//! Persisted outage recovery. All live scheduling uses a monotonic clock;
//! wall time is only a restart bridge and is clamped when it moves backwards.
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub(super) const MAX_DELAY_MS: u64 = 930_000;
const HEALTHY_FOR: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Admission {
    Attempt,
    Wait(Duration),
    Quarantined,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedRetry {
    last_ms: u64,
    next_ms: u64,
    step: u32,
}

pub(super) struct RetryPolicy {
    epoch: PathBuf,
    saved: SavedRetry,
}

pub(super) fn validate(epoch: &Path) -> std::io::Result<()> {
    read(epoch).map(|_| ())
}
fn read(epoch: &Path) -> std::io::Result<SavedRetry> {
    let Some(text) = super::safety::read_regular(&epoch.with_extension("retry"))? else {
        return Ok(SavedRetry::default());
    };
    let state: SavedRetry = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    if state.step > 3
        || state.next_ms < state.last_ms
        || state.next_ms - state.last_ms > MAX_DELAY_MS
    {
        return Err(std::io::Error::other("invalid reconnect schedule"));
    }
    Ok(state)
}

impl RetryPolicy {
    pub(super) fn load(epoch: &Path, now: u64) -> std::io::Result<Self> {
        let mut policy = Self {
            epoch: epoch.to_owned(),
            saved: read(epoch)?,
        };
        if now < policy.saved.last_ms {
            let delay = policy.saved.next_ms - policy.saved.last_ms;
            policy.saved.last_ms = now;
            policy.saved.next_ms = now.saturating_add(delay);
            policy.save()?;
        }
        Ok(policy)
    }
    #[cfg(test)]
    pub(super) fn next_ms(&self) -> u64 {
        self.saved.next_ms
    }
    pub(super) fn reserve(&mut self, now: u64) -> std::io::Result<Admission> {
        match super::safety::pause_reason(&self.epoch) {
            Some("cost_limit") => return Ok(Admission::Quarantined),
            Some(_) => return Err(std::io::Error::other("persisted safety state unavailable")),
            None => {}
        }
        if now < self.saved.next_ms {
            return Ok(Admission::Wait(Duration::from_millis(
                self.saved.next_ms - now,
            )));
        }
        if !super::safety::admit_reconnect(&self.epoch, now)? {
            return Ok(Admission::Wait(super::safety::reconnect_budget_delay(
                &self.epoch,
                now,
            )?));
        }
        // Reserve the NEXT attempt before network I/O. A crash can spend a
        // reservation but cannot reset the sequence or create a retry burst.
        let minimum = match self.saved.step {
            0 => 5_000,
            1 => 30_000,
            _ => 900_000,
        };
        let jitter = rand::random::<u64>() % 30_001;
        self.saved.step = self.saved.step.saturating_add(1).min(3);
        self.saved.last_ms = now;
        self.saved.next_ms = now.saturating_add(minimum + jitter);
        self.save()?;
        Ok(Admission::Attempt)
    }
    pub(super) fn connected_for(&mut self, elapsed: Duration, now: u64) -> std::io::Result<()> {
        if elapsed >= HEALTHY_FOR {
            match super::safety::pause_reason(&self.epoch) {
                Some("cost_limit") => return Ok(()),
                Some(_) => return Err(std::io::Error::other("persisted safety state unavailable")),
                None => {}
            }
            self.saved.step = 0;
            self.saved.last_ms = now;
            self.saved.next_ms = now;
            self.save()?;
        }
        Ok(())
    }
    fn save(&self) -> std::io::Result<()> {
        use std::io::Write;
        let path = self.epoch.with_extension("retry");
        let tmp = self
            .epoch
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(&serde_json::to_vec(&self.saved).map_err(std::io::Error::other)?)?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)?;
            if let Some(parent) = self.epoch.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn epoch(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("epoch")
    }

    #[test]
    fn day_long_outage_recovers_without_manual_reset_or_hourly_bursts() {
        let dir = tempfile::tempdir().unwrap();
        let path = epoch(&dir);
        let mut retry = RetryPolicy::load(&path, 10_000).unwrap();
        let mut attempts = Vec::new();
        for now in (10_000..=86_410_000).step_by(1000) {
            if retry.reserve(now).unwrap() == Admission::Attempt {
                attempts.push(now);
            }
        }
        assert!(attempts.len() < 103 && attempts.len() > 90, "{attempts:?}");
        for pair in attempts[3..].windows(2) {
            assert!(pair[1] - pair[0] >= 900_000);
        }
        assert_eq!(super::super::safety::pause_reason(&path), None);
        let next = retry.next_ms();
        assert!(next - 86_410_000 <= MAX_DELAY_MS);
        assert_eq!(retry.reserve(next).unwrap(), Admission::Attempt);
    }

    #[test]
    fn restart_and_brief_connections_cannot_refill_fast_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = epoch(&dir);
        let mut now = 10_000;
        for i in 0..12 {
            let mut retry = RetryPolicy::load(&path, now).unwrap();
            assert_eq!(retry.reserve(now).unwrap(), Admission::Attempt);
            retry.connected_for(Duration::from_secs(1), now).unwrap();
            let next = retry.next_ms();
            if i >= 2 {
                assert!(next - now >= 900_000);
            }
            for _ in 0..10 {
                let mut restarted = RetryPolicy::load(&path, now + 1).unwrap();
                assert!(matches!(
                    restarted.reserve(now + 1).unwrap(),
                    Admission::Wait(_)
                ));
                assert_eq!(restarted.next_ms(), next);
            }
            now = next;
        }
    }

    #[test]
    fn healthy_connection_restores_fast_retry_but_never_refills_hourly_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = epoch(&dir);
        let mut retry = RetryPolicy::load(&path, 10_000).unwrap();
        let mut now = 10_000;
        for _ in 0..4 {
            assert_eq!(retry.reserve(now).unwrap(), Admission::Attempt);
            now = retry.next_ms();
        }
        retry.connected_for(Duration::from_secs(900), now).unwrap();
        assert_eq!(retry.reserve(now).unwrap(), Admission::Attempt);
        assert!(retry.next_ms() - now < 60_000);
        std::fs::write(path.with_extension("attempts"), format!("[{now},32]")).unwrap();
        let next = retry.next_ms();
        assert!(matches!(retry.reserve(next).unwrap(), Admission::Wait(_)));
        assert_eq!(super::super::safety::pause_reason(&path), None);
        assert_eq!(retry.reserve(now + 3_600_000).unwrap(), Admission::Attempt);
    }

    #[test]
    fn clock_jumps_across_restart_do_not_strand_or_refill_retry_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = epoch(&dir);
        let mut now = 10_000_000;
        let mut retry = RetryPolicy::load(&path, now).unwrap();
        for _ in 0..4 {
            retry.reserve(now).unwrap();
            now = retry.next_ms();
        }
        let mut backward = RetryPolicy::load(&path, 1000).unwrap();
        assert!(matches!(
            backward.reserve(1000).unwrap(),
            Admission::Wait(_)
        ));
        assert!(backward.next_ms() <= 1000 + MAX_DELAY_MS);
        now = backward.next_ms();
        assert_eq!(backward.reserve(now).unwrap(), Admission::Attempt);
        let mut forward = RetryPolicy::load(&path, 1_000_000_000).unwrap();
        assert_eq!(forward.reserve(1_000_000_000).unwrap(), Admission::Attempt);
        assert!(forward.next_ms() >= 1_000_900_000);
    }

    #[test]
    fn backward_clock_preserves_exhausted_budget_and_eventually_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = epoch(&dir);
        std::fs::write(path.with_extension("attempts"), "[10000000,32]").unwrap();
        let mut retry = RetryPolicy::load(&path, 1000).unwrap();
        assert_eq!(
            retry.reserve(1000).unwrap(),
            Admission::Wait(Duration::from_secs(3600))
        );
        let mut restarted = RetryPolicy::load(&path, 2000).unwrap();
        assert_eq!(
            restarted.reserve(2000).unwrap(),
            Admission::Wait(Duration::from_millis(3_599_000))
        );
        assert_eq!(restarted.reserve(3_601_000).unwrap(), Admission::Attempt);
    }

    #[test]
    fn corruption_after_load_is_not_overwritten_by_success_or_an_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let path = epoch(&dir);
        let mut retry = RetryPolicy::load(&path, 1000).unwrap();
        retry.reserve(1000).unwrap();
        std::fs::write(path.with_extension("retry"), "broken").unwrap();
        assert!(retry
            .connected_for(Duration::from_secs(900), 901_000)
            .is_err());
        assert!(retry.reserve(901_000).is_err());
        assert_eq!(
            std::fs::read_to_string(path.with_extension("retry")).unwrap(),
            "broken"
        );
    }

    #[test]
    fn legacy_quarantine_and_corrupt_state_are_never_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = epoch(&dir);
        super::super::safety::quarantine(&path).unwrap();
        let mut retry = RetryPolicy::load(&path, 10_000).unwrap();
        assert_eq!(retry.reserve(100_000_000).unwrap(), Admission::Quarantined);
        assert!(path.with_extension("quarantine").exists());
        std::fs::write(path.with_extension("retry"), "broken").unwrap();
        assert!(RetryPolicy::load(&path, 10_000).is_err());
        assert!(super::super::safety::resume(&path, 10_000).is_err());
    }
}
