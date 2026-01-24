//! Aggregator regression tests
//!
//! Bug: NowPlayingChanged updates a separate HashMap, not zone.now_playing.
//! This causes now_playing to become stale after initial ZoneDiscovered.
//!
//! The routes use zone.now_playing which is never updated after discovery.

use std::fs;

/// Aggregator must update zone.now_playing when NowPlayingChanged arrives.
///
/// Bug: NowPlayingChanged stored in separate HashMap (self.now_playing),
/// but routes read from zone.now_playing which is stale.
///
/// Fix: Update zone.now_playing in NowPlayingChanged handler.
#[test]
fn lint_aggregator_updates_zone_now_playing() {
    let src = fs::read_to_string("src/aggregator.rs").expect("Failed to read src/aggregator.rs");

    // The NowPlayingChanged handler must update zone.now_playing, not just self.now_playing
    // Look for pattern: zones.write().await getting zone by zone_id and updating now_playing
    let updates_zone_now_playing = src.contains("zone.now_playing = Some(")
        || src.contains("zone.now_playing = Some(NowPlaying");

    assert!(
        updates_zone_now_playing,
        "REGRESSION: Aggregator NowPlayingChanged must update zone.now_playing.\n\
         Bug: Track info (title, artist, album) not updating after initial discovery.\n\
         Currently NowPlayingChanged only updates self.now_playing HashMap,\n\
         but routes read from zone.now_playing which becomes stale.\n\
         Fix: In NowPlayingChanged handler, also update the zone's now_playing field."
    );
}

/// Routes must not rely on stale zone.now_playing - verify aggregator merges it.
#[test]
fn lint_aggregator_merges_now_playing_for_get_zone() {
    let src = fs::read_to_string("src/aggregator.rs").expect("Failed to read src/aggregator.rs");

    // Either:
    // 1. get_zone merges now_playing from separate HashMap into zone, OR
    // 2. NowPlayingChanged updates zone.now_playing directly (preferred)

    // Check for option 1: get_zone merges now_playing
    let merges_in_get_zone = src.contains("fn get_zone")
        && (src.contains(".now_playing.read()") || src.contains("get_now_playing"));

    // Check for option 2: NowPlayingChanged updates zone directly
    let updates_zone_directly = src.contains("zone.now_playing = Some(");

    assert!(
        merges_in_get_zone || updates_zone_directly,
        "REGRESSION: Aggregator must ensure zone.now_playing is current.\n\
         Either update zone.now_playing in NowPlayingChanged handler,\n\
         or merge it in get_zone() before returning."
    );
}
