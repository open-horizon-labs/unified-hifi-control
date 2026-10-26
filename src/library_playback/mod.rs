//! Shared typed content-playback ingress. Surfaces submit durable intent to the bus; adapters own
//! resolution, retries and native execution. Acceptance never claims a confirmed audio effect.
pub mod identity;
use crate::bus::{
    runtime::{
        CommandDeadlines, CommandGateway, CommandLane, CommandRequest, CommandStatus,
        RuntimeCommand,
    },
    PrefixedZoneId,
};
use anyhow::{Context, Result};
use identity::{CollectionLocation, LmsDurableTarget};
use serde::{Deserialize, Serialize};

/// What a shortcut points at, in a form that survives restarts and expired sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum DurableTarget {
    /// The Roon browse walk with each row's subtitle and position, replayed by the same resolver
    /// the Library uses for a durable URL. Shortcut identity excludes list positions and playback
    /// uses an explicit resolution policy: strict Library playback requires uniqueness, while
    /// saved shortcuts may select the first exact duplicate and return a notice.
    Roon {
        location: CollectionLocation,
    },
    Lms {
        target: LmsDurableTarget,
    },
    Spotify {
        uri: String,
    },
    MusicAssistant {
        uri: String,
    },
}

impl DurableTarget {
    /// Provider label as the API reports it.
    pub fn provider_label(&self) -> &'static str {
        match self {
            Self::Roon { .. } => "roon",
            Self::Lms { .. } => "lms",
            Self::Spotify { .. } => "spotify",
            Self::MusicAssistant { .. } => "musicassistant",
        }
    }

    /// List positions are recovery hints, not identity. The command's explicit duplicate policy
    /// controls exact semantic matches; neither policy falls back to title-only lookalikes.
    pub fn canonical_identity(&self) -> Self {
        let mut canonical = self.clone();
        if let Self::Roon {
            location: CollectionLocation::Roon { steps, .. },
        } = &mut canonical
        {
            for step in steps {
                step.position = None;
            }
        }
        canonical
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryPlayRequest {
    pub target: DurableTarget,
    pub title: String,
    /// Only saved shortcuts opt into the maintainer-approved first exact duplicate policy.
    pub first_exact_match: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ItemUnavailable(pub String);

pub fn native_failure(error: anyhow::Error) -> crate::bus::runtime::NativeResult {
    if error.is::<ItemUnavailable>() {
        crate::bus::runtime::NativeResult::ItemUnavailable(error.to_string())
    } else {
        crate::bus::runtime::NativeResult::Failed(error.to_string())
    }
}

pub async fn play(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    target: &DurableTarget,
    title: &str,
) -> Result<()> {
    play_with_policy(gateway, zone_id, target, title, false)
        .await
        .map(|_| ())
}

pub async fn play_shortcut(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    target: &DurableTarget,
    title: &str,
) -> Result<Option<String>> {
    play_with_policy(gateway, zone_id, target, title, true).await
}

async fn play_with_policy(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    target: &DurableTarget,
    title: &str,
    first_exact_match: bool,
) -> Result<Option<String>> {
    let gateway = gateway.context("content command runtime is unavailable")?;
    let zone =
        PrefixedZoneId::parse(zone_id).context("content playback needs a canonical zone ID")?;
    anyhow::ensure!(
        zone.source() == target.provider_label(),
        "item and zone providers differ"
    );
    let now = tokio::time::Instant::now();
    let mut ticket = gateway
        .submit(CommandRequest {
            target: zone,
            command: RuntimeCommand::LibraryPlay(Box::new(LibraryPlayRequest {
                target: target.canonical_identity(),
                title: title.to_string(),
                first_exact_match,
            })),
            correlation_id: None,
            lane: CommandLane::Interactive,
            deadlines: CommandDeadlines {
                dispatch_by: crate::command_dispatch::deadline(),
                confirm_by: now + std::time::Duration::from_secs(45),
            },
        })
        .await
        .map_err(|error| anyhow::anyhow!("content admission refused: {error:?}"))?;
    use crate::bus::runtime::NativeResult;
    match ticket.wait_for_native_receipt().await {
        Ok(NativeResult::Accepted) => Ok(None),
        Ok(NativeResult::AcceptedWithNotice(notice)) => Ok(Some(notice)),
        Ok(NativeResult::ItemUnavailable(detail))
        | Err(CommandStatus::ItemUnavailable { detail }) => Err(ItemUnavailable(detail).into()),
        Ok(NativeResult::Failed(detail))
        | Err(CommandStatus::NotDispatched { detail } | CommandStatus::Failed { detail }) => {
            anyhow::bail!("{detail}")
        }
        Err(status) => anyhow::bail!(
            "content command outcome is uncertain ({status:?}); do not retry automatically"
        ),
    }
}
