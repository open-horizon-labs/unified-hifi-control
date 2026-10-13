//! Byte-budgeted artwork caches (#801).
//!
//! Two caches sit behind `AppState::get_image`:
//!
//! * **Source cache** -- the provider's bytes as fetched, keyed by
//!   `(provider, image_key, provider_width, provider_height)`. One cover served
//!   at several sizes is one provider round trip. Provider errors are never
//!   inserted (the insert happens only after a successful, size-checked fetch).
//! * **Output cache** -- the composed/encoded result for every format (JPEG,
//!   PNG, RGB565, ACeP6), keyed by [`OutputCacheKey`].
//!
//! Both are LRUs bounded by **bytes**, not entry count, so a few 2 MiB covers
//! cannot evict hundreds of small outputs and a flood of tiny entries cannot
//! blow the memory budget.
//!
//! # Worst-case memory budget
//!
//! | cache                                   | bound                         |
//! |-----------------------------------------|-------------------------------|
//! | source bytes ([`SOURCE_CACHE_BUDGET`])  | 32 MiB                        |
//! | outputs ([`OUTPUT_CACHE_BUDGET`])       | 64 MiB                        |
//! | analysis (`artwork_analysis`, 32 x 128x128 f32 heatmaps) | ~2 MiB       |
//! | **total retained**                      | **~98 MiB**                   |
//!
//! Transient decode buffers (at most two, the blocking-worker semaphore) are
//! additional and unchanged by this module. Entries larger than a quarter of
//! their cache's budget are served but not retained, so one oversized cover
//! cannot flush the cache. RAM-limited Pi/knob deployments that need less can
//! lower the two constants; nothing else depends on their values.

use super::image::Rgb565ResizePolicy;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Retained provider bytes. See the module docs for the full budget.
pub const SOURCE_CACHE_BUDGET: usize = 32 * 1024 * 1024;
/// Retained composed outputs. See the module docs for the full budget.
pub const OUTPUT_CACHE_BUDGET: usize = 64 * 1024 * 1024;
/// A cached source is trusted for this long. Provider image keys are normally
/// content-addressed, but some (HQPlayer "current cover", LMS URLs) can serve
/// new bytes under a stable key; the TTL bounds how long that can be masked.
pub const SOURCE_CACHE_TTL: Duration = Duration::from_secs(300);

/// Converter revision for non-e-ink outputs. Bump when JPEG/PNG/RGB565
/// composition changes so older cached bytes and old ETags are abandoned.
/// 2: linear-light zenresize resampling (#802).
pub const RASTER_CONVERTER_VERSION: u8 = 2;
/// Converter revision for packed ACeP6 outputs. 4: the composition before
/// dithering is resampled in linear light (#802).
pub const EINK_CONVERTER_VERSION: u8 = 4;

/// LRU bounded by the summed byte size of its values.
struct ByteLru<K, V> {
    entries: VecDeque<(K, Arc<V>, usize)>,
    bytes: usize,
    budget: usize,
}

impl<K: PartialEq, V> ByteLru<K, V> {
    fn new(budget: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            budget,
        }
    }

    fn get(&mut self, key: &K) -> Option<Arc<V>> {
        let index = self.entries.iter().position(|(k, _, _)| k == key)?;
        let entry = self.entries.remove(index)?;
        let value = Arc::clone(&entry.1);
        self.entries.push_front(entry);
        Some(value)
    }

    fn remove(&mut self, key: &K) {
        if let Some(index) = self.entries.iter().position(|(k, _, _)| k == key) {
            if let Some((_, _, size)) = self.entries.remove(index) {
                self.bytes -= size;
            }
        }
    }

    fn insert(&mut self, key: K, value: V, size: usize) -> Arc<V> {
        let value = Arc::new(value);
        self.remove(&key);
        if size > self.budget / 4 {
            return value;
        }
        self.entries.push_front((key, Arc::clone(&value), size));
        self.bytes += size;
        while self.bytes > self.budget {
            match self.entries.pop_back() {
                Some((_, _, evicted)) => self.bytes -= evicted,
                None => break,
            }
        }
        value
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceCacheKey {
    /// `roon`, `lms`, `spotify`, ... or `hqplayer:<instance>` (its cover is a
    /// per-instance resource). Never a bare zone id: two zones of one provider
    /// share an image namespace, and provider namespaces must not collide.
    pub provider: String,
    pub image_key: String,
    /// Dimensions the *provider* was asked for (only the `Exact` policy and
    /// `/roon/image` ask), since those change the bytes returned.
    pub provider_width: Option<u32>,
    pub provider_height: Option<u32>,
}

/// A fetched provider image plus its content digest.
#[derive(Debug)]
pub struct SourceArtwork {
    pub content_type: String,
    pub data: Vec<u8>,
    /// Hex SHA-256 prefix of `data`. It joins the output key and the ETag so
    /// a changed cover under an unchanged `image_key` can never be served from
    /// a stale output or revalidated with a stale tag.
    pub digest: String,
    /// Decoded (EXIF-oriented) pixel size, read from the image headers once at
    /// fetch time; `None` for SVG or undecodable bytes. Reported to clients as
    /// `X-Artwork-Source` (#802).
    pub dimensions: Option<(u32, u32)>,
    fetched_at: Instant,
}

impl SourceArtwork {
    pub fn new(content_type: String, data: Vec<u8>) -> Self {
        use sha2::{Digest, Sha256};
        let digest = hex::encode(&Sha256::digest(&data)[..16]);
        let dimensions = super::image::probe_artwork_dimensions(&data);
        Self {
            content_type,
            data,
            digest,
            dimensions,
            fetched_at: Instant::now(),
        }
    }
}

#[derive(Clone)]
pub struct SourceArtworkCache {
    entries: Arc<Mutex<ByteLru<SourceCacheKey, SourceArtwork>>>,
    ttl: Duration,
}

impl Default for SourceArtworkCache {
    fn default() -> Self {
        Self::with_limits(SOURCE_CACHE_BUDGET, SOURCE_CACHE_TTL)
    }
}

impl SourceArtworkCache {
    pub fn with_limits(budget: usize, ttl: Duration) -> Self {
        Self {
            entries: Arc::new(Mutex::new(ByteLru::new(budget))),
            ttl,
        }
    }

    pub fn get(&self, key: &SourceCacheKey) -> Option<Arc<SourceArtwork>> {
        let mut entries = locked(&self.entries);
        let hit = entries.get(key)?;
        if hit.fetched_at.elapsed() > self.ttl {
            entries.remove(key);
            return None;
        }
        Some(hit)
    }

    /// Retain a successfully fetched source. Callers must not call this for a
    /// provider error.
    pub fn insert(&self, key: SourceCacheKey, source: SourceArtwork) -> Arc<SourceArtwork> {
        let size = source.data.len();
        locked(&self.entries).insert(key, source, size)
    }

    pub fn retained_bytes(&self) -> usize {
        locked(&self.entries).bytes
    }
}

/// Identity of one composed output.
///
/// There is deliberately no `zone_id`: the output is a pure function of the
/// source bytes and the conversion parameters, and zone identity only ever
/// mattered as a proxy for "which provider's image_key namespace". `provider`
/// carries that, so a track shown on two Roon zones is composed once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputCacheKey {
    pub provider: String,
    pub image_key: String,
    /// See [`SourceArtwork::digest`].
    pub source_digest: String,
    pub width: u32,
    pub height: u32,
    /// `rgb565`, `rgb565-smart`, `eink_acep6`, `raster` (JPEG/PNG) or
    /// `source` (unmodified provider bytes).
    pub format: &'static str,
    pub resize_policy: Rgb565ResizePolicy,
    pub converter_version: u8,
}

impl OutputCacheKey {
    /// Strong validator derived from every field of the key. Quoted per
    /// RFC 9110; changes whenever the bytes could change.
    pub fn etag(&self) -> String {
        use sha2::{Digest, Sha256};
        let canonical = format!(
            "{}\n{}\n{}\n{}x{}\n{}\n{:?}\n{}",
            self.provider,
            self.image_key,
            self.source_digest,
            self.width,
            self.height,
            self.format,
            self.resize_policy,
            self.converter_version
        );
        format!("\"{}\"", hex::encode(&Sha256::digest(canonical)[..16]))
    }
}

#[derive(Debug)]
pub struct CachedOutput {
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Byte-budgeted LRU of composed artwork for every output format.
#[derive(Clone)]
pub struct ArtworkOutputCache {
    entries: Arc<Mutex<ByteLru<OutputCacheKey, CachedOutput>>>,
}

impl Default for ArtworkOutputCache {
    fn default() -> Self {
        Self::with_budget(OUTPUT_CACHE_BUDGET)
    }
}

impl ArtworkOutputCache {
    pub fn with_budget(budget: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(ByteLru::new(budget))),
        }
    }

    pub fn get(&self, key: &OutputCacheKey) -> Option<Arc<CachedOutput>> {
        locked(&self.entries).get(key)
    }

    pub fn insert(
        &self,
        key: OutputCacheKey,
        content_type: String,
        data: Vec<u8>,
    ) -> Arc<CachedOutput> {
        let size = data.len();
        locked(&self.entries).insert(key, CachedOutput { content_type, data }, size)
    }

    pub fn retained_bytes(&self) -> usize {
        locked(&self.entries).bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out_key(index: usize, width: u32) -> OutputCacheKey {
        OutputCacheKey {
            provider: "roon".into(),
            image_key: format!("art-{index}"),
            source_digest: "d".into(),
            width,
            height: 450,
            format: "eink_acep6",
            resize_policy: Rgb565ResizePolicy::SmartCover {
                max_crop_percent: 10,
            },
            converter_version: 1,
        }
    }

    #[test]
    fn output_cache_evicts_oldest_past_the_byte_budget() {
        let cache = ArtworkOutputCache::with_budget(100);
        for index in 0..4 {
            cache.insert(out_key(index, 800), "x".into(), vec![index as u8; 20]);
        }
        assert_eq!(cache.retained_bytes(), 80);
        // Touch 0 so 1 becomes the oldest, then overflow.
        assert!(cache.get(&out_key(0, 800)).is_some());
        cache.insert(out_key(4, 800), "x".into(), vec![4; 20]);
        cache.insert(out_key(5, 800), "x".into(), vec![5; 20]);
        assert!(cache.retained_bytes() <= 100);
        assert!(cache.get(&out_key(1, 800)).is_none(), "oldest evicted");
        assert!(cache.get(&out_key(0, 800)).is_some(), "recently used kept");
        assert!(cache.get(&out_key(5, 800)).is_some());
    }

    #[test]
    fn output_key_isolates_geometry_and_etag_tracks_every_field() {
        let cache = ArtworkOutputCache::with_budget(1000);
        cache.insert(out_key(0, 800), "x".into(), vec![0]);
        assert!(cache.get(&out_key(0, 799)).is_none());
        let base = out_key(0, 800);
        assert_eq!(base.etag(), out_key(0, 800).etag());
        assert_ne!(base.etag(), out_key(0, 799).etag());
        let mut changed = out_key(0, 800);
        changed.source_digest = "e".into();
        assert_ne!(base.etag(), changed.etag(), "new cover bytes, new tag");
        assert!(base.etag().starts_with('"') && base.etag().ends_with('"'));
    }

    #[test]
    fn oversized_entries_are_served_but_not_retained() {
        let cache = ArtworkOutputCache::with_budget(100);
        let kept = cache.insert(out_key(0, 800), "x".into(), vec![0; 26]);
        assert_eq!(kept.data.len(), 26);
        assert!(cache.get(&out_key(0, 800)).is_none());
        assert_eq!(cache.retained_bytes(), 0);
    }

    #[test]
    fn source_cache_is_byte_bounded_and_expires() {
        let key = |n: usize| SourceCacheKey {
            provider: "roon".into(),
            image_key: format!("k{n}"),
            provider_width: None,
            provider_height: None,
        };
        let cache = SourceArtworkCache::with_limits(100, Duration::from_secs(60));
        for n in 0..6 {
            cache.insert(key(n), SourceArtwork::new("image/jpeg".into(), vec![0; 20]));
        }
        assert!(cache.retained_bytes() <= 100);
        assert!(cache.get(&key(0)).is_none());
        assert!(cache.get(&key(5)).is_some());

        let expired = SourceArtworkCache::with_limits(100, Duration::ZERO);
        expired.insert(key(0), SourceArtwork::new("image/jpeg".into(), vec![0; 4]));
        std::thread::sleep(Duration::from_millis(2));
        assert!(expired.get(&key(0)).is_none(), "ttl");
        assert_eq!(expired.retained_bytes(), 0);
    }
}
