//! Run on the target Pi; this measures inference directly, bypassing caches.
//! Build first, then use /usr/bin/time -v on the executable (not on Cargo).
use std::time::Instant;
use uhc_artwork_inference::ContentAnalyzer;
use zensally::{ImageRef, PixelFormat};

fn main() -> anyhow::Result<()> {
    let pixels = match std::env::args_os().nth(1) {
        Some(path) => image::open(path)?.to_rgba8(),
        None => image::load_from_memory(include_bytes!("../tests/fixtures/artwork/crown.png"))?
            .to_rgba8(),
    };
    // Match the server's bounded probe before preprocessing into model inputs.
    let pixels = if pixels.width().max(pixels.height()) > 640 {
        image::DynamicImage::ImageRgba8(pixels)
            .thumbnail(640, 640)
            .to_rgba8()
    } else {
        pixels
    };
    let input = ImageRef::new(
        pixels.as_raw(),
        pixels.width(),
        pixels.height(),
        PixelFormat::Rgba,
    )
    .map_err(anyhow::Error::msg)?;
    let start = Instant::now();
    let mut analyzer = ContentAnalyzer::new()?;
    let load_ms = start.elapsed().as_secs_f64() * 1000.;
    let start = Instant::now();
    let output = analyzer.analyze(&input);
    let cold_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut times = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        std::hint::black_box(analyzer.analyze(&input));
        times.push(start.elapsed().as_secs_f64() * 1000.);
    }
    times.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::json!({
            "architecture": std::env::consts::ARCH,
            "model_load_ms": load_ms, "first_inference_ms": cold_ms,
            "warm_uncached_inference_runs": times.len(), "warm_p50_ms": times[9], "warm_p95_ms": times[18],
            "detected_faces": output.faces.len(), "probe_width": pixels.width(), "probe_height": pixels.height()
        })
    );
    Ok(())
}
