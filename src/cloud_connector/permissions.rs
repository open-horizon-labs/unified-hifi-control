//! Owner-requested repair of access bits, never identity or replay contents.
use super::config::CloudConnectorConfig;

/// Only classify valid, readable state as a permission problem. Missing keys,
/// corrupt ledgers and links remain distinct from changed access.
pub(super) fn changed(config: &CloudConnectorConfig) -> anyhow::Result<bool> {
    #[cfg(unix)]
    {
        Ok(files(config)?.iter().any(|file| file.changed))
    }
    #[cfg(not(unix))]
    {
        super::InstallationIdentity::load(&config.key_path, config.installation_id.clone())?;
        super::SessionEpochGuard::load(&config.epoch_path)?;
        Ok(false)
    }
}

pub(super) fn repair(config: &CloudConnectorConfig) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Open and validate the complete set before modifying any file. Use
        // file handles for chmod so a path swap cannot redirect the operation.
        let files = files(config)?;
        for file in &files {
            if file.changed {
                file.handle
                    .set_permissions(std::fs::Permissions::from_mode(0o600))?;
                file.handle.sync_all()?;
            }
        }
        anyhow::ensure!(
            !changed(config)?,
            "Connection file access could not be restored."
        );
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        anyhow::bail!("Connection file access cannot be repaired on this platform.")
    }
}

#[cfg(unix)]
struct ConnectionFile {
    handle: std::fs::File,
    changed: bool,
}

#[cfg(unix)]
fn files(config: &CloudConnectorConfig) -> anyhow::Result<Vec<ConnectionFile>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let directory = config
        .key_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Missing connection directory"))?;
    let mut files = Vec::new();
    for (path, required) in [
        (config.key_path.clone(), true),
        (config.epoch_path.clone(), false),
        (directory.join("hiphi.env"), false),
        (config.epoch_path.with_extension("attempts"), false),
        (config.epoch_path.with_extension("resume"), false),
        (config.epoch_path.with_extension("retry"), false),
        (config.epoch_path.with_extension("quarantine"), false),
    ] {
        let handle = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let metadata = handle.metadata()?;
        // Do not chmod another user's file or an alias of an unrelated file,
        // even when UHC happens to run as root on a NAS.
        anyhow::ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == rustix::process::geteuid().as_raw()
                && metadata.len() <= 16 * 1024,
            "Connection state is not an owned regular file."
        );
        let mut bytes = Vec::new();
        (&handle).take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 16 * 1024, "Connection state is too large.");
        if path == config.key_path {
            anyhow::ensure!(bytes.len() == 32, "The saved pairing key is damaged.");
        } else if path == config.epoch_path {
            std::str::from_utf8(&bytes)?.trim().parse::<u64>()?;
        }
        files.push(ConnectionFile {
            changed: metadata.permissions().mode() & 0o077 != 0,
            handle,
        });
    }
    anyhow::ensure!(
        super::safety::pause_reason(&config.epoch_path) != Some("safety_state_unavailable"),
        "Saved connection state is damaged or unreadable."
    );
    Ok(files)
}
