//! Process-wide logging policy.
//!
//! Service managers that already own retention (journald and container log
//! drivers) leave `UHC_LOG_DIR` unset and receive stdout. File-based packages
//! set only the destination; rotation and retention remain identical across
//! platforms.

use anyhow::{Context, Result};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const DEFAULT_FILTER: &str = "unified_hifi_control=info,tower_http=info,roon_api=info";
const DEFAULT_RETENTION_DAYS: usize = 7;
const MAX_RETENTION_DAYS: usize = 365;

/// Keeps the non-blocking file writer alive until server shutdown.
pub struct LoggingGuard {
    _file_writer: Option<WorkerGuard>,
}

pub fn initialize() -> Result<LoggingGuard> {
    let filter = std::env::var("RUST_LOG")
        .or_else(|_| std::env::var("LOG_LEVEL"))
        .unwrap_or_else(|_| DEFAULT_FILTER.to_owned());
    let directory = std::env::var_os("UHC_LOG_DIR").filter(|value| !value.is_empty());

    if let Some(directory) = directory {
        let retention_days =
            retention_days(std::env::var("UHC_LOG_RETENTION_DAYS").ok().as_deref())?;
        std::fs::create_dir_all(&directory).with_context(|| {
            format!(
                "failed to create UHC log directory {}",
                std::path::Path::new(&directory).display()
            )
        })?;
        let appender = file_appender(&directory, retention_days)?;
        let (writer, guard) = tracing_appender::non_blocking(appender);
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(writer),
            )
            .try_init()
            .context("failed to initialize UHC file logging")?;
        return Ok(LoggingGuard {
            _file_writer: Some(guard),
        });
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(filter))
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .context("failed to initialize UHC stdout logging")?;
    Ok(LoggingGuard { _file_writer: None })
}

fn file_appender(
    directory: impl AsRef<std::path::Path>,
    retention_days: usize,
) -> Result<RollingFileAppender> {
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("uhc-server")
        .filename_suffix("log")
        // Keep one spare because the appender documents that retention can
        // briefly dip below its maximum during rollover.
        .max_log_files(retention_days.saturating_add(1))
        .build(directory)
        .context("failed to initialize daily UHC log rotation")
}

fn retention_days(raw: Option<&str>) -> Result<usize> {
    match raw {
        None | Some("") => Ok(DEFAULT_RETENTION_DAYS),
        Some(value) => {
            let days = value
                .parse::<usize>()
                .context("UHC_LOG_RETENTION_DAYS must be an integer")?;
            if !(1..=MAX_RETENTION_DAYS).contains(&days) {
                anyhow::bail!("UHC_LOG_RETENTION_DAYS must be between 1 and 365");
            }
            Ok(days)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn retention_is_bounded_and_defaults_to_seven_days() {
        assert_eq!(retention_days(None).unwrap(), 7);
        assert_eq!(retention_days(Some("")).unwrap(), 7);
        assert_eq!(retention_days(Some("30")).unwrap(), 30);
        assert!(retention_days(Some("0")).is_err());
        assert!(retention_days(Some("366")).is_err());
        assert!(retention_days(Some("daily")).is_err());
    }

    #[test]
    fn daily_appender_removes_old_matching_files_and_writes_the_current_log() {
        let directory = tempfile::tempdir().unwrap();
        for day in 1..=12 {
            std::fs::write(
                directory
                    .path()
                    .join(format!("uhc-server.2026-01-{day:02}.log")),
                b"old\n",
            )
            .unwrap();
        }
        // A similarly named launcher diagnostic is not owned by the core
        // appender and must never be deleted by retention.
        let launcher = directory.path().join("unified-hifi-control-launcher.log");
        std::fs::write(&launcher, b"startup\n").unwrap();

        let mut appender = file_appender(directory.path(), 7).unwrap();
        writeln!(appender, "current").unwrap();
        appender.flush().unwrap();

        let owned = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("uhc-server.")
            })
            .count();
        assert!(owned <= 8, "seven retained days plus the current file");
        assert!(launcher.exists());
    }
}
