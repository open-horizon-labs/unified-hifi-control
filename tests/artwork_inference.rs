//! Compare the deployment backend against the previous ONNX implementation.
//! This detects smaller-but-wrong models, omitted face inference, changed pixel
//! normalization, output ordering and saliency/crop drift.
use zensally::{AnalysisOutput, ImageRef, PixelFormat};

fn compare(expected: AnalysisOutput, actual: AnalysisOutput, width: u32, height: u32) {
    assert_eq!(expected.faces.len(), actual.faces.len());
    for (a, b) in expected.faces.iter().zip(&actual.faces) {
        for (x, y) in [a.x1, a.y1, a.x2, a.y2, a.confidence].into_iter().zip([
            b.x1,
            b.y1,
            b.x2,
            b.y2,
            b.confidence,
        ]) {
            assert!(
                x.is_finite() && y.is_finite() && (x - y).abs() <= 1e-5,
                "face output drift: {x} vs {y}"
            );
        }
    }
    let a = expected.saliency.as_ref().expect("reference saliency");
    let b = actual
        .saliency
        .as_ref()
        .expect("deployment saliency must not be skipped");
    assert_eq!(
        (a.width, a.height, a.data.len()),
        (b.width, b.height, b.data.len())
    );
    assert_eq!((b.width, b.height), (128, 128));
    for (&x, &y) in a.data.iter().zip(&b.data) {
        assert!(
            x.is_finite() && y.is_finite() && (x - y).abs() <= 1e-5,
            "saliency output drift: {x} vs {y}"
        );
    }
    let a = zensally::bridge::build_smart_crop_input(expected, &[]);
    let b = zensally::bridge::build_smart_crop_input(actual, &[]);
    for (w, h) in [(4, 3), (3, 4), (1, 1), (16, 9)] {
        use zenlayout::smart_crop::{AspectRatio, CropMode};
        let targets = [(AspectRatio { w, h }, CropMode::Minimal)];
        let a = a.compute_crops(width, height, &targets);
        let b = b.compute_crops(width, height, &targets);
        assert_eq!(a[0], b[0], "crop geometry changed for {w}:{h}");
    }
}

#[test]
fn preconverted_models_preserve_analysis_and_crops() {
    let mut old = zensally_tract::ContentAnalyzer::new().unwrap();
    let mut new = uhc_artwork_inference::ContentAnalyzer::new().unwrap();
    for (width, height) in [(256, 256), (640, 180), (180, 640), (1, 1)] {
        for format in [PixelFormat::Rgb, PixelFormat::Rgba, PixelFormat::Bgra] {
            let channels = format.bytes_per_pixel();
            let pixels: Vec<u8> = (0..width * height * channels as u32)
                .map(|i| ((i * 31 + i / 17) % 256) as u8)
                .collect();
            let image = ImageRef::new(&pixels, width, height, format).unwrap();
            compare(old.analyze(&image), new.analyze(&image), width, height);
        }
    }
    let crown = image::load_from_memory(include_bytes!("fixtures/artwork/crown.png"))
        .unwrap()
        .to_rgba8();
    let image = ImageRef::new(
        crown.as_raw(),
        crown.width(),
        crown.height(),
        PixelFormat::Rgba,
    )
    .unwrap();
    let expected = old.analyze(&image);
    assert!(
        !expected.faces.is_empty(),
        "crown fixture must exercise positive face detection"
    );
    compare(expected, new.analyze(&image), crown.width(), crown.height());

    // Optional private/provider corpus: no copyrighted covers are copied into the repository.
    if let Some(paths) = std::env::var_os("UHC_ARTWORK_EQUIVALENCE_IMAGES") {
        for path in std::env::split_paths(&paths) {
            let pixels = image::open(&path).unwrap().to_rgba8();
            let image = ImageRef::new(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                PixelFormat::Rgba,
            )
            .unwrap();
            let expected = old.analyze(&image);
            let actual = new.analyze(&image);
            eprintln!(
                "{}: {} reference faces",
                path.display(),
                expected.faces.len()
            );
            compare(expected, actual, pixels.width(), pixels.height());
        }
    }
}
