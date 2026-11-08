//! Process-local analysis cache shared by every display size and zone.
//! Models load lazily once. A single lock serializes inference and coalesces
//! concurrent misses, bounding model working memory on small servers.
use image::DynamicImage;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock, Mutex},
};
use uhc_artwork_inference::ContentAnalyzer;
use zenlayout::smart_crop::{AspectRatio, CropMode, FocusRect, SmartCropInput};
use zensally::{ImageRef, PixelFormat};

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
    compose_with_cache(source, width, height, &ANALYSIS)
}
fn compose_with_cache(
    source: &DynamicImage,
    width: u32,
    height: u32,
    cache: &Mutex<AnalysisCache>,
) -> anyhow::Result<DynamicImage> {
    // Equal aspect ratios need no crop decision. Use wide multiplication so
    // dimensions cannot overflow while testing exact equality.
    if u64::from(source.width()) * u64::from(height)
        == u64::from(source.height()) * u64::from(width)
    {
        return super::image::resample(
            source,
            (0, 0, source.width(), source.height()),
            width,
            height,
        );
    }
    let analysis = cache
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

/// Select a full-bleed crop while accounting for pixels hidden by the panel.
pub(super) fn compose_circle(source: &DynamicImage, side: u32) -> anyhow::Result<DynamicImage> {
    let analysis = ANALYSIS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .analyze(source)?;
    let crop = select_circle_crop(source.width(), source.height(), &analysis);
    super::image::resample(
        source,
        (crop.x, crop.y, crop.width, crop.height),
        side,
        side,
    )
}

fn select_circle_crop(width: u32, height: u32, analysis: &SmartCropInput) -> zenlayout::Rect {
    let side = width.min(height);
    let baseline = zenlayout::Rect {
        x: (width - side) / 2,
        y: (height - side) / 2,
        width: side,
        height: side,
    };
    let faces: Vec<_> = analysis
        .focus_regions
        .iter()
        .filter(|face| face.weight >= 0.5)
        .collect();
    if faces.is_empty() {
        return baseline;
    }

    let score = |crop| {
        faces
            .iter()
            .map(|face| face_visibility(face, width, height, crop))
            .collect::<Vec<_>>()
    };
    let current = score(baseline);
    let worst = current.iter().copied().min().unwrap_or(64);
    // On a 64-point grid, 10 points is roughly 15 percentage points.
    if worst > 54 {
        return baseline;
    }

    let square = AspectRatio { w: 1, h: 1 };
    let mut best = baseline;
    let mut best_score = (worst, current.iter().sum::<u32>(), side);
    for crop in analysis
        .compute_crops(
            width,
            height,
            &[(square, CropMode::Minimal), (square, CropMode::Maximal)],
        )
        .into_iter()
        .flatten()
    {
        if crop.width == 0
            || crop.width != crop.height
            || crop.x > width.saturating_sub(crop.width)
            || crop.y > height.saturating_sub(crop.height)
            || crop.width > side
        {
            continue;
        }
        let visible = score(crop);
        let minimum = visible.iter().copied().min().unwrap_or(0);
        // Allow at most 3 points (~5 percentage points) of loss for any face.
        if minimum < worst + 10
            || visible
                .iter()
                .zip(&current)
                .any(|(new, old)| new + 3 < *old)
        {
            continue;
        }
        let candidate_score = (minimum, visible.iter().sum::<u32>(), crop.width);
        if candidate_score > best_score {
            best_score = candidate_score;
            best = crop;
        }
    }
    best
}

fn face_visibility(face: &FocusRect, width: u32, height: u32, crop: zenlayout::Rect) -> u32 {
    let cx = f64::from(crop.x) + f64::from(crop.width) * 0.5;
    let cy = f64::from(crop.y) + f64::from(crop.height) * 0.5;
    let radius_squared = (f64::from(crop.width) * 0.5).powi(2);
    let mut count = 0;
    for row in 0..8 {
        let y = f64::from(height)
            * (f64::from(face.y1) + f64::from(face.y2 - face.y1) * (f64::from(row) + 0.5) / 8.0)
            / 100.0;
        for col in 0..8 {
            let x = f64::from(width)
                * (f64::from(face.x1)
                    + f64::from(face.x2 - face.x1) * (f64::from(col) + 0.5) / 8.0)
                / 100.0;
            count += u32::from((x - cx).powi(2) + (y - cy).powi(2) <= radius_squared);
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    fn face(x1: f32, y1: f32, x2: f32, y2: f32) -> FocusRect {
        FocusRect {
            x1,
            y1,
            x2,
            y2,
            weight: 1.0,
        }
    }

    #[test]
    fn circle_crop_recovers_an_off_center_face_in_a_wide_source() {
        let analysis = SmartCropInput {
            focus_regions: vec![face(12.0, 40.0, 20.0, 60.0)],
            heatmap: None,
        };
        let selected = select_circle_crop(200, 100, &analysis);
        assert_ne!(selected.x, 50);
        assert_eq!(
            face_visibility(&analysis.focus_regions[0], 200, 100, selected),
            64
        );
    }

    #[test]
    fn circle_crop_keeps_full_bleed_when_corner_faces_cannot_both_improve() {
        let analysis = SmartCropInput {
            focus_regions: vec![
                face(1.4, 0.8, 19.3, 24.1),
                face(65.0, 19.9, 78.2, 38.8),
                face(33.9, 16.5, 48.5, 35.5),
                face(50.1, 14.5, 61.5, 32.5),
                face(84.2, 0.0, 98.8, 15.9),
            ],
            heatmap: None,
        };
        let crop = select_circle_crop(2048, 2048, &analysis);
        assert_eq!(
            (crop.x, crop.y, crop.width, crop.height),
            (0, 0, 2048, 2048)
        );
    }

    #[test]
    fn matching_aspect_ratio_resizes_without_loading_models() {
        for (sw, sh, tw, th) in [
            (240, 240, 120, 120),
            (240, 120, 480, 240),
            (120, 240, 60, 120),
        ] {
            let source = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                sw,
                sh,
                image::Rgba([80, 90, 100, 255]),
            ));
            let cache = Mutex::new(AnalysisCache::new());
            let result = compose_with_cache(&source, tw, th, &cache).unwrap();
            let expected = super::super::image::resample(&source, (0, 0, sw, sh), tw, th).unwrap();
            assert_eq!(result.to_rgba8(), expected.to_rgba8());
            let cache = cache.lock().unwrap();
            assert_eq!(cache.analyses, 0, "no crop decision is needed");
            assert!(
                cache.analyzer.is_none(),
                "do not even load models for a plain resize"
            );
        }
    }

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
