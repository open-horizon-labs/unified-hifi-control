//! Durable library identity, independent of HTTP/MCP sessions and persistence.
use serde::{Deserialize, Serialize};

/// One human-visible step in a collection walk. `provider_path` never leaves
/// the server; Roon uses `subtitle` + `position` to safely find the equivalent
/// row in a fresh Core browse session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionStep {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    #[serde(default = "visible_breadcrumb")]
    pub breadcrumb: bool,
}

const fn visible_breadcrumb() -> bool {
    true
}

impl CollectionStep {
    pub fn new(title: impl Into<String>, provider_path: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            provider_path: Some(provider_path.into()),
            position: None,
            breadcrumb: true,
        }
    }

    pub fn roon(title: impl Into<String>, subtitle: Option<String>, position: Option<u32>) -> Self {
        Self {
            title: title.into(),
            subtitle,
            provider_path: None,
            position,
            breadcrumb: true,
        }
    }

    pub fn hidden_roon(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            provider_path: None,
            position: None,
            breadcrumb: false,
        }
    }
}

/// Provider-specific resolution material behind one provider-neutral URL
/// token. The enum is persisted, but never serialized into a client response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum CollectionLocation {
    Roon {
        origin: RoonLocationOrigin,
        steps: Vec<CollectionStep>,
    },
    Lms {
        steps: Vec<CollectionStep>,
    },
    #[serde(rename = "musicassistant")]
    MusicAssistant {
        steps: Vec<CollectionStep>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoonLocationOrigin {
    BrowseRoot,
    Search {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

impl CollectionLocation {
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Roon { .. } => "roon",
            Self::Lms { .. } => "lms",
            Self::MusicAssistant { .. } => "musicassistant",
        }
    }

    pub fn steps(&self) -> &[CollectionStep] {
        match self {
            Self::Roon { steps, .. } | Self::Lms { steps } | Self::MusicAssistant { steps } => {
                steps
            }
        }
    }

    pub fn with_steps(&self, steps: Vec<CollectionStep>) -> Self {
        match self {
            Self::Roon { origin, .. } => Self::Roon {
                origin: origin.clone(),
                steps,
            },
            Self::Lms { .. } => Self::Lms { steps },
            Self::MusicAssistant { .. } => Self::MusicAssistant { steps },
        }
    }

    pub fn appended(&self, step: CollectionStep) -> Self {
        let mut steps = self.steps().to_vec();
        steps.push(step);
        self.with_steps(steps)
    }

    pub fn last_provider_path(&self) -> Option<&str> {
        self.steps().last()?.provider_path.as_deref()
    }
}

/// Type of search result from LMS
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LmsSearchResultType {
    Album,
    Artist,
    Track,
}

impl std::fmt::Display for LmsSearchResultType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LmsSearchResultType::Album => write!(f, "album"),
            LmsSearchResultType::Artist => write!(f, "artist"),
            LmsSearchResultType::Track => write!(f, "track"),
        }
    }
}

/// The part of an LMS play target that is safe to keep across restarts, for callers that store a
/// pointer (controller shortcuts). It has no variant for a positional search breadcrumb:
/// that is a positional breadcrumb that can play the wrong item with no error, so it can never be
/// expressed here, let alone persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LmsDurableTarget {
    Library { item: LmsSearchResultType, id: i64 },
    SavedPlaylist { id: i64 },
    Url { url: String },
}
