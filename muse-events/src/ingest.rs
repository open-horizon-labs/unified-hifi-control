//! Ingest wire protocol types for muse-ingest.
//!
//! These types define the format expected by the muse-ingest proxy
//! when UHC's EventReporter forwards events.

use crate::MuseEvent;
use serde::{Deserialize, Serialize};

/// Event payload sent to the ingest proxy.
///
/// Wraps a typed `MuseEvent` with a timestamp. The event is the single
/// source of truth — no string event types, no untyped JSON payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngestEvent {
    /// The typed event (NowPlayingChanged, HqpPipelineChanged, etc.)
    pub event: MuseEvent,

    /// Unix timestamp in seconds
    pub timestamp: u64,
}

/// Request body for the ingest endpoint.
///
/// Events are batched for efficiency — UHC buffers events
/// and sends them in batches to reduce network overhead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngestRequest {
    /// Batch of events to process
    pub events: Vec<IngestEvent>,
}

impl IngestRequest {
    /// Create a new request with the given events
    pub fn new(events: Vec<IngestEvent>) -> Self {
        Self { events }
    }

    /// Check if the request is empty
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Get the number of events
    pub fn len(&self) -> usize {
        self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zone::{NowPlaying, TrackMetadata};

    #[test]
    fn test_ingest_event_roundtrip() {
        let event = IngestEvent {
            event: MuseEvent::NowPlayingChanged {
                zone_id: "lms:dc:a6:32:5c:34:bc".to_string(),
                zone_name: Some("Kitchen".to_string()),
                source: Some("lms".to_string()),
                now_playing: Some(NowPlaying {
                    title: "Blue 7".to_string(),
                    artist: "Sonny Rollins".to_string(),
                    album: "Saxophone Colossus".to_string(),
                    image_key: None,
                    seek_position: None,
                    duration: Some(693.0),
                    metadata: Some(TrackMetadata {
                        format: Some("FLAC".to_string()),
                        sample_rate: Some(44100),
                        bit_depth: Some(16),
                        bitrate: None,
                        genre: None,
                        composer: None,
                        track_number: None,
                        disc_number: None,
                    }),
                }),
            },
            timestamp: 1771183341,
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: IngestEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_ingest_request() {
        let request = IngestRequest::new(vec![IngestEvent {
            event: MuseEvent::HqpPipelineChanged {
                host: "192.168.1.100".to_string(),
                filter: Some("poly-sinc-gauss-hires-lp".to_string()),
                shaper: Some("NS9".to_string()),
                rate: Some("DSD256".to_string()),
            },
            timestamp: 1771183342,
        }]);

        assert_eq!(request.len(), 1);
        assert!(!request.is_empty());

        let json = serde_json::to_string(&request).unwrap();
        let deserialized: IngestRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(request, deserialized);
    }

    #[test]
    fn test_empty_request() {
        let request = IngestRequest::new(vec![]);
        assert!(request.is_empty());
        assert_eq!(request.len(), 0);
    }
}
