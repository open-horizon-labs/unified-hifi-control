//! Presentation clock for the playing source selected by the aggregator.
use crate::bus::NowPlaying;
use std::time::{Duration, Instant};

// Bound estimates so a stalled provider cannot make a stopped track run indefinitely.
const MAX_EXTRAPOLATION: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct RelayPosition {
    anchor: Option<Anchor>,
}

struct Anchor {
    source: String,
    title: String,
    artist: String,
    album: String,
    position: Duration,
    observed: Instant,
}

impl RelayPosition {
    /// Called only for an unambiguous playing source. The owner discards this clock on
    /// pause, missing metadata/binding, or instance removal. Never consult HQPlayer timing.
    pub(super) fn project(
        &mut self,
        source: &str,
        np: &NowPlaying,
        now: Instant,
    ) -> Option<Duration> {
        let Some(position) = np
            .seek_position
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        else {
            self.anchor = None;
            return None;
        };
        let unchanged = self.anchor.as_ref().is_some_and(|anchor| {
            anchor.source == source
                && anchor.title == np.title
                && anchor.artist == np.artist
                && anchor.album == np.album
                && anchor.position == position
        });
        if !unchanged {
            self.anchor = Some(Anchor {
                source: source.into(),
                title: np.title.clone(),
                artist: np.artist.clone(),
                album: np.album.clone(),
                position,
                observed: now,
            });
        }
        let anchor = self.anchor.as_ref()?;
        let estimated = position.saturating_add(
            now.saturating_duration_since(anchor.observed)
                .min(MAX_EXTRAPOLATION),
        );
        let duration = np
            .duration
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
        Some(duration.map_or(estimated, |duration| estimated.min(duration)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(position: Option<f64>) -> NowPlaying {
        NowPlaying {
            title: "Track".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            seek_position: position,
            duration: Some(180.0),
            image_key: None,
            metadata: None,
            repeat_mode: None,
            shuffle: None,
        }
    }

    #[test]
    fn advances_between_source_reports_but_bounds_stale_extrapolation() {
        let now = Instant::now();
        let mut clock = RelayPosition::default();
        let np = track(Some(42.0));
        assert_eq!(
            clock.project("roon:a", &np, now),
            Some(Duration::from_secs(42))
        );
        assert_eq!(
            clock.project("roon:a", &np, now + Duration::from_millis(750)),
            Some(Duration::from_millis(42750))
        );
        assert_eq!(
            clock.project("roon:a", &np, now + Duration::from_secs(30)),
            Some(Duration::from_secs(44))
        );
    }

    #[test]
    fn artwork_only_changes_preserve_the_position_anchor() {
        let now = Instant::now();
        let mut clock = RelayPosition::default();
        let mut np = track(Some(42.0));
        clock.project("roon:a", &np, now);
        np.image_key = Some("refreshed-art".into());
        assert_eq!(
            clock.project("roon:a", &np, now + Duration::from_millis(750)),
            Some(Duration::from_millis(42750))
        );
    }

    #[test]
    fn source_reports_and_seeks_override_the_estimate_immediately() {
        let now = Instant::now();
        let mut clock = RelayPosition::default();
        clock.project("roon:a", &track(Some(42.0)), now);
        for (i, position) in [43.0, 12.0, 100.0].into_iter().enumerate() {
            assert_eq!(
                clock.project(
                    "roon:a",
                    &track(Some(position)),
                    now + Duration::from_secs(i as u64 + 1)
                ),
                Some(Duration::from_secs_f64(position))
            );
        }
    }

    #[test]
    fn track_source_and_missing_position_reset_the_clock() {
        let now = Instant::now();
        let mut clock = RelayPosition::default();
        let mut np = track(Some(42.0));
        clock.project("roon:a", &np, now);
        np.title = "Next track".into();
        assert_eq!(
            clock.project("roon:a", &np, now + Duration::from_secs(1)),
            Some(Duration::from_secs(42))
        );
        assert_eq!(
            clock.project("roon:b", &np, now + Duration::from_secs(2)),
            Some(Duration::from_secs(42))
        );
        np.seek_position = None;
        assert_eq!(
            clock.project("roon:b", &np, now + Duration::from_secs(3)),
            None
        );
        np.seek_position = Some(42.0);
        assert_eq!(
            clock.project("roon:b", &np, now + Duration::from_secs(4)),
            Some(Duration::from_secs(42))
        );
    }

    #[test]
    fn caps_at_duration_and_does_not_invent_unknown_or_invalid_positions() {
        let now = Instant::now();
        let mut clock = RelayPosition::default();
        let np = track(Some(179.0));
        clock.project("roon:a", &np, now);
        assert_eq!(
            clock.project("roon:a", &np, now + Duration::from_secs(2)),
            Some(Duration::from_secs(180))
        );
        for position in [None, Some(f64::NAN), Some(-1.0), Some(f64::INFINITY)] {
            assert_eq!(clock.project("roon:a", &track(position), now), None);
        }
    }
}
