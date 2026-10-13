//! Process-local analysis cache shared by every display size and zone.
//! Models load lazily once. A single lock serializes inference and coalesces
//! concurrent misses, bounding model working memory on small servers.
use image::DynamicImage;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock, Mutex},
};
use zenlayout::smart_crop::{AspectRatio, CropMode, SmartCropInput};
use zensally::{ImageRef, PixelFormat};
use zensally_tract::ContentAnalyzer;

const CAPACITY: usize = 32; // ~2 MiB of 128x128 float heatmaps.
type Key = [u8; 32];
struct AnalysisCache {
    analyzer: Option<ContentAnalyzer>,
    entries: VecDeque<(Key, Arc<SmartCropInput>)>,
    #[cfg(test)]
    analyses: usize,
}
impl AnalysisCache {
    fn new() -> Self {
        Self {
            analyzer: None,
            entries: VecDeque::new(),
            #[cfg(test)]
            analyses: 0,
        }
    }
    fn analyze(&mut self, source: &DynamicImage) -> anyhow::Result<Arc<SmartCropInput>> {
        let mut hash = Sha256::new();
        hash.update(source.width().to_le_bytes());
        hash.update(source.height().to_le_bytes());
        hash.update([source.color() as u8]);
        hash.update(source.as_bytes());
        let key: Key = hash.finalize().into();
        if let Some(index) = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == &key)
        {
            if let Some(entry) = self.entries.remove(index) {
                let analysis = Arc::clone(&entry.1);
                self.entries.push_front(entry);
                return Ok(analysis);
            }
        }
        if self.analyzer.is_none() {
            self.analyzer = Some(ContentAnalyzer::new()?);
        }
        let analyzer = self
            .analyzer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("artwork analyzer unavailable"))?;
        // Models internally resize to fixed 320x240 and 256x256 inputs. Limit
        // the RGBA conversion too, rather than copying a large decoded cover.
        let probe = if source.width().max(source.height()) > 640 {
            source.thumbnail(640, 640).to_rgba8()
        } else {
            source.to_rgba8()
        };
        let input = ImageRef::new(
            probe.as_raw(),
            probe.width(),
            probe.height(),
            PixelFormat::Rgba,
        )
        .map_err(anyhow::Error::msg)?;
        let output = analyzer.analyze(&input);
        let analysis = Arc::new(zensally::bridge::build_smart_crop_input(output, &[]));
        self.entries.push_front((key, Arc::clone(&analysis)));
        self.entries.truncate(CAPACITY);
        #[cfg(test)]
        {
            self.analyses += 1;
        }
        Ok(analysis)
    }
}
static ANALYSIS: LazyLock<Mutex<AnalysisCache>> =
    LazyLock::new(|| Mutex::new(AnalysisCache::new()));

pub(super) fn compose(
    source: &DynamicImage,
    width: u32,
    height: u32,
) -> anyhow::Result<DynamicImage> {
    let analysis = ANALYSIS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .analyze(source)?;
    compose_analyzed(source, width, height, &analysis)
}
fn compose_analyzed(
    source: &DynamicImage,
    width: u32,
    height: u32,
    analysis: &SmartCropInput,
) -> anyhow::Result<DynamicImage> {
    let crops = analysis.compute_crops(
        source.width(),
        source.height(),
        &[(
            AspectRatio {
                w: width,
                h: height,
            },
            CropMode::Minimal,
        )],
    );
    let rect = crops
        .first()
        .and_then(Option::as_ref)
        .ok_or_else(|| anyhow::anyhow!("no viable artwork crop"))?;
    anyhow::ensure!(
        rect.width > 0
            && rect.height > 0
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|right| right <= source.width())
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|bottom| bottom <= source.height()),
        "invalid artwork crop bounds"
    );
    super::image::resample(
        source,
        (rect.x, rect.y, rect.width, rect.height),
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_requests_share_one_model_analysis() {
        let source = Arc::new(
            image::load_from_memory(include_bytes!("../../tests/fixtures/artwork/crown.png"))
                .unwrap(),
        );
        let cache = Arc::new(Mutex::new(AnalysisCache::new()));
        std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|_| {
                    let source = Arc::clone(&source);
                    let cache = Arc::clone(&cache);
                    scope.spawn(move || cache.lock().unwrap().analyze(&source).unwrap())
                })
                .collect();
            let results: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
            assert!(results
                .iter()
                .all(|result| Arc::ptr_eq(result, &results[0])));
        });
        assert_eq!(
            cache.lock().unwrap().analyses,
            1,
            "concurrent cache misses must not duplicate inference"
        );
    }

    #[test]
    fn one_analysis_serves_landscape_portrait_and_repeat_requests() {
        let source =
            image::load_from_memory(include_bytes!("../../tests/fixtures/artwork/crown.png"))
                .unwrap();
        let mut cache = AnalysisCache::new();
        let first = cache.analyze(&source).unwrap();
        let faces = &first.focus_regions;
        assert_eq!(faces.len(), 1);
        for (width, height) in [(800, 450), (450, 800), (160, 90)] {
            let repeated = cache.analyze(&source).unwrap();
            assert!(Arc::ptr_eq(&first, &repeated));
            let crop = repeated.compute_crops(
                240,
                240,
                &[(
                    AspectRatio {
                        w: width,
                        h: height,
                    },
                    CropMode::Minimal,
                )],
            )[0]
            .unwrap();
            let face = &faces[0];
            assert!(crop.x as f32 <= face.x1 * 2.4 && crop.y as f32 <= face.y1 * 2.4);
            assert!(
                (crop.x + crop.width) as f32 >= face.x2 * 2.4
                    && (crop.y + crop.height) as f32 >= face.y2 * 2.4
            );
            assert_eq!(
                compose_analyzed(&source, width, height, &repeated)
                    .unwrap()
                    .width(),
                width
            );
        }
        assert_eq!(cache.analyses, 1);
        let changed = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            240,
            240,
            image::Rgba([0, 0, 0, 255]),
        ));
        cache.analyze(&changed).unwrap();
        assert_eq!(
            cache.analyses, 2,
            "different cover pixels must invalidate analysis"
        );
        for value in 1..=CAPACITY as u8 {
            let different = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                24,
                24,
                image::Rgba([value, 0, 0, 255]),
            ));
            cache.analyze(&different).unwrap();
        }
        assert_eq!(
            cache.entries.len(),
            CAPACITY,
            "heatmap cache must remain bounded"
        );
        let count = cache.analyses;
        cache.analyze(&source).unwrap();
        assert_eq!(
            cache.analyses,
            count + 1,
            "evicted artwork must be analyzed again"
        );
    }
}
