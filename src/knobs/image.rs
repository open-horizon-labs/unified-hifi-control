//! Artwork decoding, composition and device encodings.
//!
//! Serves every artwork client from one pipeline: round/square knob LCDs
//! (240-360 px RGB565), Frame e-ink panels (packed six-colour ACeP), browser
//! and Roku/TV displays up to 4K (PNG/JPEG). This module handles:
//! - JPEG, PNG, GIF, BMP, WebP decoding (with EXIF orientation), SVG via resvg
//! - Composition policies: subject-aware cover (zensally analysis + zenlayout
//!   crop, falling back to libvips attention, then a centered crop), explicit
//!   Fit on a gallery mat, SmartCover and Exact
//! - Resampling in linear light with zenresize (#802): Ginseng with mild
//!   sharpening when enlarging, Robidoux when reducing; see [`resampler_for`]
//! - RGB565 (little-endian, ESP32) and ACeP6 packing

use image::{
    codecs::jpeg::JpegEncoder,
    imageops::{self, FilterType},
    DynamicImage, ImageFormat, Rgba, RgbaImage,
};
use std::io::Cursor;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// RGB565 image data for LCD display
pub struct Rgb565Image {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Native framebuffer data for Waveshare's six-color ACeP/Spectra panel.
/// Two pixels are packed per byte, with the left pixel in the high nibble.
pub struct EinkAcep6Image {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Rgb565ResizePolicy {
    /// Proportionate fill using cached face/saliency analysis, with libvips fallback.
    #[default]
    AttentionCover,
    /// Legacy behavior for clients that explicitly need every output pixel filled.
    Exact,
    /// Preserve the complete source and compose unused space as a gallery mat.
    Fit,
    /// Fill only when the center crop stays within the stated source-area budget.
    SmartCover { max_crop_percent: u8 },
}

const ACEP6_PALETTE: [[u8; 3]; 6] = [
    [0, 0, 0],
    [255, 255, 255],
    [255, 255, 0],
    [255, 0, 0],
    [0, 0, 255],
    [0, 255, 0],
];
const ACEP6_PANEL_INDEX: [u8; 6] = [0, 1, 2, 3, 5, 6];

fn nearest_acep6_color(r: u8, g: u8, b: u8) -> usize {
    ACEP6_PALETTE
        .iter()
        .enumerate()
        .min_by_key(|(_, color)| {
            let dr = i32::from(r) - i32::from(color[0]);
            let dg = i32::from(g) - i32::from(color[1]);
            let db = i32::from(b) - i32::from(color[2]);
            dr * dr + dg * dg + db * db
        })
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn diffuse_channel(channel: &mut u8, error: i32, numerator: i32) {
    *channel = (i32::from(*channel) + error * numerator / 16).clamp(0, 255) as u8;
}

/// Reproduce the accepted Frame firmware conversion on the server: ideal
/// six-color palette, RGB-distance matching, and raster Floyd-Steinberg error
/// diffusion with each propagated update clamped to a byte.
fn floyd_steinberg_acep6(rgb: &mut [u8], width: usize, height: usize) -> Vec<u8> {
    let bytes_per_row = width.div_ceil(2);
    let mut packed = vec![0u8; bytes_per_row * height];

    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 3;
            let r = rgb[offset];
            let g = rgb[offset + 1];
            let b = rgb[offset + 2];
            let palette_index = nearest_acep6_color(r, g, b);
            let chosen = ACEP6_PALETTE[palette_index];
            let panel_index = ACEP6_PANEL_INDEX[palette_index];
            let packed_offset = y * bytes_per_row + x / 2;
            if x % 2 == 0 {
                packed[packed_offset] = panel_index << 4;
            } else {
                packed[packed_offset] |= panel_index;
            }

            let errors = [
                i32::from(r) - i32::from(chosen[0]),
                i32::from(g) - i32::from(chosen[1]),
                i32::from(b) - i32::from(chosen[2]),
            ];
            let mut add_error = |neighbor: usize, weight: i32| {
                diffuse_channel(&mut rgb[neighbor], errors[0], weight);
                diffuse_channel(&mut rgb[neighbor + 1], errors[1], weight);
                diffuse_channel(&mut rgb[neighbor + 2], errors[2], weight);
            };

            if x + 1 < width {
                add_error(offset + 3, 7);
            }
            if y + 1 < height {
                let next_row = (y + 1) * width * 3;
                if x > 0 {
                    add_error(next_row + (x - 1) * 3, 3);
                }
                add_error(next_row + x * 3, 5);
                if x + 1 < width {
                    add_error(next_row + (x + 1) * 3, 1);
                }
            }
        }
    }
    packed
}

const GALLERY_MAT: Rgba<u8> = Rgba([248, 250, 251, 255]);
const GALLERY_KEYLINE: Rgba<u8> = Rgba([23, 37, 54, 255]);

/// Bound composed outputs, including direct callers outside the HTTP handler.
pub fn valid_artwork_dimensions(width: u32, height: u32, max_pixels: u64) -> bool {
    width > 0
        && height > 0
        && width <= 8192
        && height <= 8192
        && u64::from(width) * u64::from(height) <= max_pixels
}

fn dimension_error() -> image::ImageError {
    image::ImageError::Limits(image::error::LimitError::from_kind(
        image::error::LimitErrorKind::DimensionError,
    ))
}

/// Decode bounded raster data and honor EXIF before normalizing for libvips.
/// The native process receives our PNG, never a provider path or SVG loader.
fn decode_artwork(data: &[u8]) -> Result<DynamicImage, image::ImageError> {
    use image::ImageDecoder;
    let mut reader = image::ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    if !valid_artwork_dimensions(width, height, 16_777_216) {
        return Err(dimension_error());
    }
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

const VIPS_DEADLINE: Duration = Duration::from_secs(3);

fn run_vips_thumbnail(
    executable: &std::ffi::OsStr,
    input: &std::path::Path,
    output: &std::path::Path,
    width: u32,
    height: u32,
    deadline: Duration,
) -> anyhow::Result<()> {
    let mut child = Command::new(executable)
        .arg("thumbnail")
        .arg(input)
        .arg(output)
        .arg(width.to_string())
        .arg(format!("--height={height}"))
        .arg("--crop=attention")
        .arg("--size=both")
        .env("VIPS_CONCURRENCY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::ensure!(status.success(), "vips thumbnail exited with {status}");
                return Ok(());
            }
            Ok(None) if start.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                // Reap even on a polling error; dropping Child does not kill it.
                let _ = child.kill();
                let _ = child.wait();
                match result {
                    Err(error) => return Err(error.into()),
                    _ => anyhow::bail!("vips thumbnail exceeded its deadline"),
                }
            }
        }
    }
}

fn attention_cover(img: &DynamicImage, width: u32, height: u32) -> anyhow::Result<DynamicImage> {
    attention_cover_using(
        img,
        width,
        height,
        std::ffi::OsStr::new("vips"),
        VIPS_DEADLINE,
    )
}

fn attention_cover_using(
    img: &DynamicImage,
    width: u32,
    height: u32,
    executable: &std::ffi::OsStr,
    deadline: Duration,
) -> anyhow::Result<DynamicImage> {
    let scale = (f64::from(width) / f64::from(img.width()))
        .max(f64::from(height) / f64::from(img.height()));
    anyhow::ensure!(
        f64::from(img.width()) * f64::from(img.height()) * scale * scale <= 67_108_864.0,
        "attention composition exceeds intermediate pixel budget"
    );
    let temp = tempfile::tempdir()?;
    let input = temp.path().join("source.png");
    let output = temp.path().join("cover.png");
    img.save_with_format(&input, ImageFormat::Png)?;
    run_vips_thumbnail(executable, &input, &output, width, height, deadline)?;
    let composed = decode_artwork(&std::fs::read(output)?)?;
    anyhow::ensure!(
        composed.width() == width && composed.height() == height,
        "vips returned incorrect dimensions"
    );
    Ok(composed)
}

/// Normal image clients use the same composition as packed displays. PNG keeps
/// its lossless/alpha representation; other raster formats become JPEG.
pub fn image_to_display_with_policy(
    data: &[u8],
    width: u32,
    height: u32,
    policy: Rgb565ResizePolicy,
    png: bool,
) -> Result<Vec<u8>, image::ImageError> {
    if !valid_artwork_dimensions(width, height, 8_388_608) {
        return Err(dimension_error());
    }
    let image = decode_artwork(data)?;
    let composed = resize_with_policy(&image, width, height, policy);
    let mut output = Cursor::new(Vec::new());
    if png {
        composed.write_to(&mut output, ImageFormat::Png)?;
    } else {
        DynamicImage::ImageRgb8(composed.to_rgb8())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut output, 90))?;
    }
    Ok(output.into_inner())
}

fn cover_crop_percent(source_w: u32, source_h: u32, target_w: u32, target_h: u32) -> f64 {
    let source_ratio = source_w as f64 / source_h as f64;
    let target_ratio = target_w as f64 / target_h as f64;
    let visible_fraction = if source_ratio < target_ratio {
        source_ratio / target_ratio
    } else {
        target_ratio / source_ratio
    };
    (1.0 - visible_fraction) * 100.0
}

fn fit_on_gallery_mat(img: &DynamicImage, target_w: u32, target_h: u32) -> DynamicImage {
    let (fit_w, fit_h) = zenresize::fit_dims(
        img.width(),
        img.height(),
        target_w,
        target_h,
        zenresize::FitMode::Fit,
    );
    let fitted =
        resample_or_bilinear(img, (0, 0, img.width(), img.height()), fit_w, fit_h).to_rgba8();
    let offset_x = (target_w - fitted.width()) / 2;
    let offset_y = (target_h - fitted.height()) / 2;
    let mut canvas = RgbaImage::from_pixel(target_w, target_h, GALLERY_MAT);

    // A one-pixel keyline makes the complete sleeve feel deliberately mounted,
    // especially when pale artwork meets the near-white mat on e-ink.
    if offset_x > 0 || offset_y > 0 {
        let left = offset_x.saturating_sub(1);
        let top = offset_y.saturating_sub(1);
        let right = (offset_x + fitted.width()).min(target_w - 1);
        let bottom = (offset_y + fitted.height()).min(target_h - 1);
        for x in left..=right {
            canvas.put_pixel(x, top, GALLERY_KEYLINE);
            canvas.put_pixel(x, bottom, GALLERY_KEYLINE);
        }
        for y in top..=bottom {
            canvas.put_pixel(left, y, GALLERY_KEYLINE);
            canvas.put_pixel(right, y, GALLERY_KEYLINE);
        }
    }

    imageops::overlay(&mut canvas, &fitted, offset_x.into(), offset_y.into());
    DynamicImage::ImageRgba8(canvas)
}

/// Resampling kernel and in-kernel sharpening chosen for one resize (#802).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Resampler {
    pub filter: zenresize::Filter,
    /// `resize_sharpen` goal in percent; `None` keeps the filter's natural lobes.
    pub sharpen_percent: Option<f32>,
}

/// Negative-lobe goal for enlargement, in percent.
///
/// `resize_sharpen` only ever amplifies: a goal at or below the filter's
/// natural negative-lobe area is a silent no-op. Measured with zenresize's own
/// metric, Ginseng's natural ratio is 13.2% (Lanczos-3 is 13.7%), so the
/// 10-13% range commonly quoted for "mild" sharpening would do nothing here.
/// 15% raises the negative lobes by ~14%: enough to restore some of the edge
/// contrast a 2-3x enlargement of a 600-1500 px cover loses, small enough that
/// ringing stays below the level that outlines typography. Zero cost: it
/// reshapes the weight table rather than running an unsharp pass.
pub(crate) const ENLARGE_SHARPEN_PERCENT: f32 = 15.0;

/// The one place the resampling filter is chosen.
///
/// Enlargement on either axis (output larger than the cropped source) uses
/// Ginseng, a jinc-windowed sinc that keeps enlarged edges crisp without
/// Lanczos' blockiness, plus [`ENLARGE_SHARPEN_PERCENT`]. Everything else is a
/// reduction (or 1:1) and uses Robidoux, zenresize's default: a near-zero-ringing
/// cubic that does not alias fine cover detail into moire.
pub(crate) fn resampler_for(crop_w: u32, crop_h: u32, out_w: u32, out_h: u32) -> Resampler {
    if out_w > crop_w || out_h > crop_h {
        Resampler {
            filter: zenresize::Filter::Ginseng,
            sharpen_percent: Some(ENLARGE_SHARPEN_PERCENT),
        }
    } else {
        Resampler {
            filter: zenresize::Filter::Robidoux,
            sharpen_percent: None,
        }
    }
}

/// Source-pixel rectangle `(x, y, width, height)`.
pub(crate) type CropRect = (u32, u32, u32, u32);

/// Crop `rect` out of `img` and resample it to `out_w`x`out_h` in linear
/// light, in one zenresize pass (source-region crop + resize).
///
/// sRGB bytes are linearized, filtered in f32 and re-encoded, so a
/// high-contrast edge averages to the correct brightness instead of the dark
/// fringe gamma-space filtering produces. Straight alpha is premultiplied
/// for the filter and restored afterwards.
pub(crate) fn resample(
    img: &DynamicImage,
    rect: CropRect,
    out_w: u32,
    out_h: u32,
) -> anyhow::Result<DynamicImage> {
    use zenresize::{PixelDescriptor, ResizeConfig, Resizer};
    let alpha = img.color().has_alpha();
    let converted;
    let pixels: &[u8] = match img {
        DynamicImage::ImageRgb8(buffer) => buffer.as_raw(),
        DynamicImage::ImageRgba8(buffer) => buffer.as_raw(),
        other if alpha => {
            converted = other.to_rgba8().into_raw();
            &converted
        }
        other => {
            converted = other.to_rgb8().into_raw();
            &converted
        }
    };
    let descriptor = if alpha {
        PixelDescriptor::RGBA8_SRGB
    } else {
        PixelDescriptor::RGB8_SRGB
    };
    let (x, y, crop_w, crop_h) = rect;
    let resampler = resampler_for(crop_w, crop_h, out_w, out_h);
    let mut builder = ResizeConfig::builder(img.width(), img.height(), out_w, out_h)
        .filter(resampler.filter)
        .format(descriptor)
        .linear()
        .crop(x, y, crop_w, crop_h);
    if let Some(percent) = resampler.sharpen_percent {
        builder = builder.resize_sharpen(percent);
    }
    let config = builder.build();
    config.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        pixels.len() == config.input_row_len() * img.height() as usize,
        "artwork pixel buffer does not match its dimensions"
    );
    let output = Resizer::new(&config).resize(pixels);
    let resized = if alpha {
        RgbaImage::from_raw(out_w, out_h, output).map(DynamicImage::ImageRgba8)
    } else {
        image::RgbImage::from_raw(out_w, out_h, output).map(DynamicImage::ImageRgb8)
    };
    resized.ok_or_else(|| anyhow::anyhow!("resampler returned a short buffer"))
}

/// [`resample`] for the infallible composition paths. Every input reaching
/// here has already passed the dimension checks, so the bilinear arm exists
/// only so a resampler fault degrades quality instead of failing the request.
fn resample_or_bilinear(
    img: &DynamicImage,
    rect: CropRect,
    out_w: u32,
    out_h: u32,
) -> DynamicImage {
    resample(img, rect, out_w, out_h).unwrap_or_else(|error| {
        tracing::warn!("Linear-light artwork resampling failed; using bilinear: {error}");
        let (x, y, width, height) = rect;
        img.crop_imm(x, y, width, height)
            .resize_exact(out_w, out_h, FilterType::Triangle)
    })
}

/// Largest centered rectangle of the target aspect inside the source. Cropping
/// before resizing means a very wide source and a tall viewport cannot
/// allocate an enormous full-size intermediate.
fn centered_cover_rect(source_w: u32, source_h: u32, width: u32, height: u32) -> CropRect {
    let (crop_w, crop_h) =
        if u64::from(source_w) * u64::from(height) > u64::from(source_h) * u64::from(width) {
            (
                ((u64::from(source_h) * u64::from(width) / u64::from(height)) as u32).max(1),
                source_h,
            )
        } else {
            (
                source_w,
                ((u64::from(source_w) * u64::from(height) / u64::from(width)) as u32).max(1),
            )
        };
    (
        (source_w - crop_w) / 2,
        (source_h - crop_h) / 2,
        crop_w,
        crop_h,
    )
}

/// Whether a composition enlarged, reduced or kept its source's resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtworkScale {
    Up,
    Down,
    None,
}

impl ArtworkScale {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::None => "none",
        }
    }
}

/// Scale implied by `policy`'s geometry for a `source` composed to `target`,
/// using the same "either axis" enlargement rule as [`resampler_for`]. Cover
/// policies crop to the target aspect (a subject-aware crop has the same size
/// as the centered one); Fit compares the fitted art, not the mat.
pub fn artwork_scale(
    source: (u32, u32),
    target: (u32, u32),
    policy: Rgb565ResizePolicy,
) -> ArtworkScale {
    let (source_w, source_h) = source;
    let (target_w, target_h) = target;
    if source_w == 0 || source_h == 0 || target_w == 0 || target_h == 0 {
        return ArtworkScale::None;
    }
    if source == target {
        return ArtworkScale::None;
    }
    let fit = || {
        let (w, h) = zenresize::fit_dims(
            source_w,
            source_h,
            target_w,
            target_h,
            zenresize::FitMode::Fit,
        );
        ((source_w, source_h), (w, h))
    };
    let cover = || {
        let (_, _, w, h) = centered_cover_rect(source_w, source_h, target_w, target_h);
        ((w, h), target)
    };
    let ((crop_w, crop_h), (out_w, out_h)) = match policy {
        Rgb565ResizePolicy::AttentionCover => cover(),
        Rgb565ResizePolicy::Exact => (source, target),
        Rgb565ResizePolicy::Fit => fit(),
        Rgb565ResizePolicy::SmartCover { max_crop_percent }
            if cover_crop_percent(source_w, source_h, target_w, target_h)
                <= f64::from(max_crop_percent) =>
        {
            cover()
        }
        Rgb565ResizePolicy::SmartCover { .. } => fit(),
    };
    if out_w > crop_w || out_h > crop_h {
        ArtworkScale::Up
    } else if (out_w, out_h) == (crop_w, crop_h) {
        ArtworkScale::None
    } else {
        ArtworkScale::Down
    }
}

/// Decoded dimensions of artwork bytes, after EXIF orientation, from the
/// headers alone (no pixel decode). `None` for SVG and undecodable bytes.
pub fn probe_artwork_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    use image::{metadata::Orientation, ImageDecoder};
    let mut decoder = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let (width, height) = decoder.dimensions();
    let swapped = matches!(
        decoder.orientation().unwrap_or(Orientation::NoTransforms),
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    (width > 0 && height > 0).then_some(if swapped {
        (height, width)
    } else {
        (width, height)
    })
}

fn resize_with_policy(
    img: &DynamicImage,
    target_w: u32,
    target_h: u32,
    policy: Rgb565ResizePolicy,
) -> DynamicImage {
    if img.width() == target_w && img.height() == target_h {
        return img.clone();
    }

    match policy {
        Rgb565ResizePolicy::AttentionCover => attention_or_center(
            img,
            target_w,
            target_h,
            super::artwork_analysis::compose(img, target_w, target_h).or_else(|error| {
                tracing::warn!("Zensally artwork crop unavailable; trying libvips: {error}");
                attention_cover(img, target_w, target_h)
            }),
        ),
        Rgb565ResizePolicy::Exact => {
            resample_or_bilinear(img, (0, 0, img.width(), img.height()), target_w, target_h)
        }
        Rgb565ResizePolicy::Fit => fit_on_gallery_mat(img, target_w, target_h),
        Rgb565ResizePolicy::SmartCover { max_crop_percent }
            if cover_crop_percent(img.width(), img.height(), target_w, target_h)
                <= f64::from(max_crop_percent) =>
        {
            centered_cover(img, target_w, target_h)
        }
        Rgb565ResizePolicy::SmartCover { .. } => fit_on_gallery_mat(img, target_w, target_h),
    }
}

fn attention_or_center(
    img: &DynamicImage,
    width: u32,
    height: u32,
    result: anyhow::Result<DynamicImage>,
) -> DynamicImage {
    match result {
        Ok(composed) => composed,
        Err(error) => {
            tracing::warn!("Attention artwork crop unavailable; using centered fill: {error}");
            centered_cover(img, width, height)
        }
    }
}

/// Centered fill: the last fallback, and SmartCover within its crop budget.
fn centered_cover(img: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    let rect = centered_cover_rect(img.width(), img.height(), width, height);
    resample_or_bilinear(img, rect, width, height)
}

pub fn image_to_eink_acep6_with_policy(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
    policy: Rgb565ResizePolicy,
) -> Result<EinkAcep6Image, image::ImageError> {
    if !valid_artwork_dimensions(target_width, target_height, 1_536_000) {
        return Err(dimension_error());
    }
    let image = decode_artwork(image_data)?;
    let resized = resize_with_policy(&image, target_width, target_height, policy);
    let mut rgb = resized.to_rgb8().into_raw();
    let data = floyd_steinberg_acep6(&mut rgb, target_width as usize, target_height as usize);
    Ok(EinkAcep6Image {
        data,
        width: target_width,
        height: target_height,
    })
}

/// Convert any image buffer (JPEG, PNG, SVG, etc.) to RGB565 format for ESP32 LCD
///
/// Returns RGB565 data in little-endian byte order (ESP32 native).
/// Supports JPEG, PNG, GIF, BMP, WebP via the `image` crate, and SVG via `resvg`.
pub fn jpeg_to_rgb565(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
) -> Result<Rgb565Image, image::ImageError> {
    jpeg_to_rgb565_with_policy(
        image_data,
        target_width,
        target_height,
        Rgb565ResizePolicy::Exact,
    )
}

pub fn jpeg_to_rgb565_with_policy(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
    policy: Rgb565ResizePolicy,
) -> Result<Rgb565Image, image::ImageError> {
    if !valid_artwork_dimensions(target_width, target_height, 8_388_608) {
        return Err(dimension_error());
    }
    // Check if it's SVG (starts with '<' after optional whitespace/BOM)
    let trimmed = image_data
        .iter()
        .find(|&&b| b != 0xEF && b != 0xBB && b != 0xBF && !b.is_ascii_whitespace());

    if trimmed == Some(&b'<') {
        // Try SVG rasterization
        if let Ok(rgb565) = svg_to_rgb565(image_data, target_width, target_height) {
            return Ok(rgb565);
        }
        // Fall through to try as regular image if SVG parsing fails
    }

    // Auto-detect format and decode (works with JPEG, PNG, GIF, BMP, etc.)
    let img = decode_artwork(image_data)?;

    // Resize if needed
    let img = resize_with_policy(&img, target_width, target_height, policy);

    // Convert to RGB565
    let rgb565_data = rgba_to_rgb565(&img);

    Ok(Rgb565Image {
        data: rgb565_data,
        width: target_width,
        height: target_height,
    })
}

/// Rasterize SVG to RGB565 format
pub fn svg_to_rgb565(
    svg_data: &[u8],
    target_width: u32,
    target_height: u32,
) -> Result<Rgb565Image, Box<dyn std::error::Error + Send + Sync>> {
    use resvg::tiny_skia::{Pixmap, Transform};
    use resvg::usvg::{Options, Tree};

    // Parse SVG
    let tree = Tree::from_data(svg_data, &Options::default())?;

    // Get original size
    let size = tree.size();
    let (orig_w, orig_h) = (size.width(), size.height());

    // Calculate scale to fit target dimensions
    let scale_x = target_width as f32 / orig_w;
    let scale_y = target_height as f32 / orig_h;
    let scale = scale_x.min(scale_y);

    // Create pixmap for rendering
    let mut pixmap = Pixmap::new(target_width, target_height).ok_or("Failed to create pixmap")?;

    // Fill with dark background (matches placeholder style)
    pixmap.fill(resvg::tiny_skia::Color::from_rgba8(51, 51, 51, 255));

    // Center the scaled image
    let scaled_w = orig_w * scale;
    let scaled_h = orig_h * scale;
    let offset_x = (target_width as f32 - scaled_w) / 2.0;
    let offset_y = (target_height as f32 - scaled_h) / 2.0;

    // Render SVG
    let transform = Transform::from_scale(scale, scale).post_translate(offset_x, offset_y);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // Convert RGBA to RGB565
    let pixels = pixmap.data();
    let mut rgb565 = Vec::with_capacity((target_width * target_height * 2) as usize);

    for chunk in pixels.chunks(4) {
        let r = chunk[0] >> 3; // 5 bits
        let g = chunk[1] >> 2; // 6 bits
        let b = chunk[2] >> 3; // 5 bits

        let pixel_value: u16 = ((r as u16) << 11) | ((g as u16) << 5) | (b as u16);

        // Little-endian for ESP32
        rgb565.push((pixel_value & 0xFF) as u8);
        rgb565.push((pixel_value >> 8) as u8);
    }

    Ok(Rgb565Image {
        data: rgb565,
        width: target_width,
        height: target_height,
    })
}

/// Convert any image buffer to RGB565 format (alias with clearer name)
pub fn image_bytes_to_rgb565(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
) -> Result<Rgb565Image, image::ImageError> {
    jpeg_to_rgb565(image_data, target_width, target_height)
}

/// Convert any image to RGB565 format
pub fn image_to_rgb565(img: &DynamicImage, target_width: u32, target_height: u32) -> Rgb565Image {
    // Avoid clone when dimensions already match
    let resized;
    let img_ref = if img.width() != target_width || img.height() != target_height {
        resized = resample_or_bilinear(
            img,
            (0, 0, img.width(), img.height()),
            target_width,
            target_height,
        );
        &resized
    } else {
        img
    };

    let rgb565_data = rgba_to_rgb565(img_ref);

    Rgb565Image {
        data: rgb565_data,
        width: target_width,
        height: target_height,
    }
}

/// Convert RGBA image to RGB565 bytes (little-endian)
fn rgba_to_rgb565(img: &DynamicImage) -> Vec<u8> {
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut rgb565 = Vec::with_capacity((width * height * 2) as usize);

    for pixel in rgba.pixels() {
        let r = pixel[0] >> 3; // 5 bits
        let g = pixel[1] >> 2; // 6 bits
        let b = pixel[2] >> 3; // 5 bits
                               // Alpha (pixel[3]) is ignored

        // Pack into RGB565: RRRRRGGGGGGBBBBB
        let pixel_value: u16 = ((r as u16) << 11) | ((g as u16) << 5) | (b as u16);

        // Little-endian for ESP32
        rgb565.push((pixel_value & 0xFF) as u8);
        rgb565.push((pixel_value >> 8) as u8);
    }

    rgb565
}

/// Resize JPEG and re-encode with specified quality
pub fn resize_jpeg(
    jpeg_data: &[u8],
    target_width: u32,
    target_height: u32,
    quality: u8,
) -> Result<Vec<u8>, image::ImageError> {
    let img = image::load_from_memory_with_format(jpeg_data, ImageFormat::Jpeg)?;

    let resized = if img.width() != target_width || img.height() != target_height {
        resample_or_bilinear(
            &img,
            (0, 0, img.width(), img.height()),
            target_width,
            target_height,
        )
    } else {
        img
    };

    // Use JPEG encoder with specified quality
    let mut output = Cursor::new(Vec::new());
    let encoder = JpegEncoder::new_with_quality(&mut output, quality);
    resized.write_with_encoder(encoder)?;

    Ok(output.into_inner())
}

/// Generate placeholder SVG for missing album art
pub fn placeholder_svg(width: u32, height: u32) -> String {
    format!(
        concat!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}">"##,
            r##"<rect width="100%" height="100%" fill="#333"/>"##,
            r##"<text x="50%" y="50%" fill="#888" text-anchor="middle" "##,
            r##"dy=".3em" font-family="sans-serif" font-size="24">No Image</text>"##,
            r##"</svg>"##
        ),
        width, height
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

    /// 2x2 black/white checkerboard. Its true average is 50% *linear* light,
    /// which is sRGB ~188; averaging the gamma-encoded bytes gives ~128.
    fn checkerboard() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(2, 2, |x, y| {
            if (x + y) % 2 == 0 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            }
        }))
    }

    #[test]
    fn resampling_happens_in_linear_light() {
        let source = checkerboard();
        for (label, out) in [
            (
                "exact",
                resize_with_policy(&source, 1, 1, Rgb565ResizePolicy::Exact),
            ),
            ("centered cover", centered_cover(&source, 1, 1)),
        ] {
            let pixel = out.to_rgb8().get_pixel(0, 0).0;
            for channel in pixel {
                assert!(
                    channel.abs_diff(188) <= 2,
                    "{label}: 50% linear light must encode as sRGB ~188, got {pixel:?} \
                     (128 means the resize averaged gamma-encoded bytes)"
                );
            }
        }
    }

    #[test]
    fn resampler_uses_ginseng_to_enlarge_and_robidoux_to_reduce() {
        use zenresize::Filter;
        let enlarge = Resampler {
            filter: Filter::Ginseng,
            sharpen_percent: Some(ENLARGE_SHARPEN_PERCENT),
        };
        let reduce = Resampler {
            filter: Filter::Robidoux,
            sharpen_percent: None,
        };
        // 1400 px cover onto a 4K frame: enlargement on both axes.
        assert_eq!(resampler_for(1400, 788, 3840, 2160), enlarge);
        // Enlargement on either axis counts (a crop taller than it is wide).
        assert_eq!(resampler_for(600, 200, 300, 400), enlarge);
        assert_eq!(resampler_for(200, 600, 400, 300), enlarge);
        // Knob-sized output from a larger cover: reduction.
        assert_eq!(resampler_for(1400, 1400, 360, 360), reduce);
        assert_eq!(
            resampler_for(640, 360, 640, 360),
            reduce,
            "1:1 is not enlargement"
        );
        assert_eq!(resampler_for(240, 135, 240, 100), reduce);
        // The sharpen goal must actually exceed Ginseng's natural negative-lobe
        // ratio, or `resize_sharpen` is a silent no-op.
        let natural = zenresize::filter::InterpolationDetails::create(Filter::Ginseng)
            .calculate_percent_negative_weight()
            * 100.0;
        assert!(
            f64::from(ENLARGE_SHARPEN_PERCENT) > natural,
            "sharpen {ENLARGE_SHARPEN_PERCENT}% must exceed Ginseng natural {natural:.1}%"
        );
    }

    #[test]
    fn default_landscape_crop_preserves_upper_face() {
        let source =
            image::load_from_memory(include_bytes!("../../tests/fixtures/artwork/crown.png"))
                .unwrap();
        let actual = resize_with_policy(&source, 800, 450, Rgb565ResizePolicy::default());
        let expected = resample(&source, (0, 0, 240, 135), 800, 450).unwrap();
        assert!(
            actual.to_rgb8() == expected.to_rgb8(),
            "landscape cover must retain the face at the top of this artwork"
        );
        let bytes = include_bytes!("../../tests/fixtures/artwork/crown.png");
        let packed =
            jpeg_to_rgb565_with_policy(bytes, 800, 450, Rgb565ResizePolicy::default()).unwrap();
        assert!(
            packed.data == rgba_to_rgb565(&expected),
            "Frame/LCD packing must use the face-preserving composition"
        );
        let browser =
            image_to_display_with_policy(bytes, 800, 450, Rgb565ResizePolicy::default(), true)
                .unwrap();
        assert!(
            image::load_from_memory(&browser).unwrap().to_rgb8() == expected.to_rgb8(),
            "browser images must retain the same face-preserving crop"
        );
        let mut expected_png = Cursor::new(Vec::new());
        expected
            .write_to(&mut expected_png, ImageFormat::Png)
            .unwrap();
        let frame = image_to_eink_acep6_with_policy(bytes, 800, 450, Rgb565ResizePolicy::default())
            .unwrap();
        let expected_frame = image_to_eink_acep6_with_policy(
            expected_png.get_ref(),
            800,
            450,
            Rgb565ResizePolicy::Exact,
        )
        .unwrap();
        assert!(
            frame.data == expected_frame.data,
            "Frame eInk palette packing must use the face-preserving crop"
        );
    }

    #[test]
    fn scale_header_geometry_matches_the_resampler_rule() {
        use ArtworkScale::{Down, None, Up};
        let cover = Rgb565ResizePolicy::default();
        assert_eq!(artwork_scale((1400, 1400), (360, 360), cover), Down);
        assert_eq!(artwork_scale((1400, 1400), (3840, 2160), cover), Up);
        // 1400 wide crop of 788 rows -> 1400x788 is 1:1, not an upscale.
        assert_eq!(artwork_scale((1400, 1400), (1400, 788), cover), None);
        assert_eq!(artwork_scale((100, 100), (100, 100), cover), None);
        // Fit compares the fitted art: 1000x1000 into 1920x1080 -> 1080x1080.
        assert_eq!(
            artwork_scale((1000, 1000), (1920, 1080), Rgb565ResizePolicy::Fit),
            Up
        );
        assert_eq!(
            artwork_scale((1200, 1200), (1920, 1080), Rgb565ResizePolicy::Fit),
            Down
        );
        assert_eq!(
            artwork_scale((600, 600), (300, 900), Rgb565ResizePolicy::Exact),
            Up
        );
    }

    #[test]
    fn probe_reports_oriented_source_dimensions_without_decoding() {
        let png = encode_solid_png(30, 20, Rgba([1, 2, 3, 255]));
        assert_eq!(probe_artwork_dimensions(&png), Some((30, 20)));
        assert_eq!(probe_artwork_dimensions(b"<svg/>"), Option::None);
        let mut jpeg = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 20, image::Rgb([255, 0, 0])))
            .write_with_encoder(JpegEncoder::new_with_quality(&mut jpeg, 90))
            .unwrap();
        let jpeg = jpeg.into_inner();
        let exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut oriented = vec![0xff, 0xd8, 0xff, 0xe1];
        oriented.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        oriented.extend_from_slice(exif);
        oriented.extend_from_slice(&jpeg[2..]);
        assert_eq!(probe_artwork_dimensions(&oriented), Some((20, 40)));
    }

    #[test]
    fn transparent_art_keeps_alpha_and_has_no_dark_premultiply_fringe() {
        // Opaque white beside fully transparent black: a straight-alpha
        // filter that skips premultiplication bleeds the black into the edge.
        let source = DynamicImage::ImageRgba8(RgbaImage::from_fn(8, 8, |x, _| {
            if x < 4 {
                Rgba([255, 255, 255, 255])
            } else {
                Rgba([0, 0, 0, 0])
            }
        }));
        let out = resample(&source, (0, 0, 8, 8), 32, 32).unwrap().to_rgba8();
        for pixel in out.pixels().filter(|p| p[3] > 16) {
            assert!(pixel[0] > 240, "visible edge pixels stay white: {pixel:?}");
        }
        assert_eq!(out.get_pixel(31, 16)[3], 0, "transparency is preserved");
    }

    /// Where the large-render review images go: under the cargo target
    /// directory, never in the repository.
    fn review_dir() -> std::path::PathBuf {
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
        let dir = target.join("artwork-802");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Lossless but barely compressed, so 4K review renders do not dominate
    /// the debug-build test time.
    fn save_fast_png(image: &DynamicImage, path: &std::path::Path) {
        use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
        let rgb = image.to_rgb8();
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        PngEncoder::new_with_quality(file, CompressionType::Fast, PngFilter::NoFilter)
            .write_image(
                &rgb,
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
    }

    /// Deterministic stand-in for a photographic 1400 px cover: smooth
    /// gradients, fine noise and a block of hard black/white "type".
    fn photographic_1400() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(1400, 1400, |x, y| {
            let noise = ((x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663)) % 23) as f32;
            if (500..900).contains(&x) && (620..780).contains(&y) {
                let stroke = (x / 9 + y / 13) % 3 == 0;
                return if stroke {
                    image::Rgb([250, 250, 250])
                } else {
                    image::Rgb([8, 8, 8])
                };
            }
            let fx = x as f32 / 1400.0;
            let fy = y as f32 / 1400.0;
            image::Rgb([
                (40.0 + 180.0 * fx + noise) as u8,
                (60.0 + 120.0 * (fx * 9.0).sin().abs() * fy + noise) as u8,
                (200.0 - 150.0 * fy + noise) as u8,
            ])
        }))
    }

    /// Compose `source` at `width`x`height`, assert the geometry, and save
    /// the compose plus a same-crop zenresize/bilinear pair for side-by-side
    /// review of softness and edge halos. The linear-light property itself
    /// is pinned by `resampling_happens_in_linear_light`.
    fn render_for_review(name: &str, source: &DynamicImage, width: u32, height: u32) {
        let dir = review_dir();
        let start = Instant::now();
        let new = resize_with_policy(source, width, height, Rgb565ResizePolicy::default());
        let elapsed = start.elapsed();
        assert_eq!((new.width(), new.height()), (width, height));
        let rect = centered_cover_rect(source.width(), source.height(), width, height);
        let centered_new = resample(source, rect, width, height).unwrap();
        let centered_old = source
            .crop_imm(rect.0, rect.1, rect.2, rect.3)
            .resize_exact(width, height, FilterType::Triangle);
        let new_path = dir.join(format!("{name}_{width}x{height}_compose.png"));
        save_fast_png(&new, &new_path);
        save_fast_png(
            &centered_new,
            &dir.join(format!("{name}_{width}x{height}_centered_zenresize.png")),
        );
        save_fast_png(
            &centered_old,
            &dir.join(format!("{name}_{width}x{height}_centered_bilinear.png")),
        );
        println!(
            "#802 render {name} -> {width}x{height}: compose {elapsed:?} -> {}",
            new_path.display()
        );
    }

    /// #802 acceptance renders: no panic, exact dimensions, saved for review.
    #[test]
    fn large_display_renders_have_exact_dimensions_and_are_saved_for_review() {
        let crown =
            image::load_from_memory(include_bytes!("../../tests/fixtures/artwork/crown.png"))
                .unwrap();
        render_for_review("crown", &crown, 1600, 900);
        render_for_review("crown", &crown, 3840, 2160);
        render_for_review("photographic1400", &photographic_1400(), 1600, 900);
    }

    /// The 1400 px -> 4K acceptance render. Ignored by default only because
    /// four 8-megapixel images take ~10 s in a debug build; run with
    /// `cargo test --lib knobs::image::tests::photographic_4k -- --ignored`.
    #[test]
    #[ignore = "slow in debug builds; run explicitly for #802 review renders"]
    fn photographic_4k_render_for_review() {
        render_for_review("photographic1400", &photographic_1400(), 3840, 2160);
    }

    /// #802 timing: new resampler vs the old bilinear path at knob and 1080p
    /// sizes. Printed, not asserted: debug-build numbers are only a relative
    /// signal, and the hardware budget is the 3 s libvips deadline on a Pi.
    #[test]
    fn resampler_timing_at_knob_and_1080p_sizes() {
        let photo = photographic_1400();
        for (width, height) in [(360, 360), (1920, 1080)] {
            let rect = centered_cover_rect(photo.width(), photo.height(), width, height);
            let runs = 3;
            let start = Instant::now();
            for _ in 0..runs {
                resample(&photo, rect, width, height).unwrap();
            }
            let new = start.elapsed() / runs;
            let start = Instant::now();
            for _ in 0..runs {
                let _ = photo.crop_imm(rect.0, rect.1, rect.2, rect.3).resize_exact(
                    width,
                    height,
                    FilterType::Triangle,
                );
            }
            let old = start.elapsed() / runs;
            let start = Instant::now();
            let composed = resize_with_policy(&photo, width, height, Rgb565ResizePolicy::default());
            let compose = start.elapsed();
            assert_eq!((composed.width(), composed.height()), (width, height));
            println!(
                "#802 timing 1400x1400 -> {width}x{height}: zenresize {new:?}, \
                 old bilinear {old:?}, full compose (first analysis) {compose:?}"
            );
        }
    }

    fn encode_solid_png(width: u32, height: u32, color: image::Rgba<u8>) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(width, height, color);
        let mut png_data = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png_data)
            .write_image(&img, width, height, image::ExtendedColorType::Rgba8)
            .expect("encode test PNG");
        png_data
    }

    fn rgb565_at(image: &Rgb565Image, x: u32, y: u32) -> u16 {
        let offset = ((y * image.width + x) * 2) as usize;
        u16::from_le_bytes([image.data[offset], image.data[offset + 1]])
    }

    fn off_center_subject() -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(400, 200, |x, y| {
            let dx = i64::from(x) - 350;
            let dy = i64::from(y) - 100;
            if dx * dx + dy * dy <= 30 * 30 {
                Rgba([255, 120, 30, 255])
            } else {
                Rgba([10, 10, 10, 255])
            }
        }))
    }

    #[test]
    fn attention_selects_off_center_subject_without_stretching() {
        let source = off_center_subject();
        let attention = attention_cover(&source, 100, 100)
            .expect("install vips to exercise actual attention")
            .to_rgb8();
        let center = source
            .resize_to_fill(100, 100, FilterType::Triangle)
            .to_rgb8();
        let is_subject = |p: &image::Rgb<u8>| p[0] > 200 && p[1] > 80 && p[2] < 60;
        assert_eq!(
            center.pixels().filter(|p| is_subject(p)).count(),
            0,
            "center crop misses the subject"
        );
        let points: Vec<_> = attention
            .enumerate_pixels()
            .filter(|(_, _, p)| is_subject(p))
            .collect();
        assert!(
            points.len() > 600,
            "attention must preserve the off-center circle"
        );
        let width = points.iter().map(|(x, _, _)| *x).max().unwrap()
            - points.iter().map(|(x, _, _)| *x).min().unwrap()
            + 1;
        let height = points.iter().map(|(_, y, _)| *y).max().unwrap()
            - points.iter().map(|(_, y, _)| *y).min().unwrap()
            + 1;
        assert!(
            width.abs_diff(height) <= 2,
            "the circle must stay round: {width}x{height}"
        );
        assert_eq!(
            attention_cover(&source, 100, 100).unwrap().to_rgb8(),
            attention,
            "crop placement is deterministic"
        );
    }

    #[test]
    fn attention_upscales_small_covers_and_regular_image_fit_is_explicit() {
        let data = encode_solid_png(20, 20, Rgba([255, 0, 0, 255]));
        for png in [false, true] {
            let filled =
                image_to_display_with_policy(&data, 160, 90, Rgb565ResizePolicy::default(), png)
                    .unwrap();
            let filled = image::load_from_memory(&filled).unwrap().to_rgb8();
            assert_eq!(filled.dimensions(), (160, 90));
            assert!(
                filled.pixels().all(|p| p[0] > 240 && p[1] < 10),
                "upscaling must still fill"
            );
            let fitted =
                image_to_display_with_policy(&data, 160, 90, Rgb565ResizePolicy::Fit, png).unwrap();
            assert!(
                image::load_from_memory(&fitted)
                    .unwrap()
                    .to_rgb8()
                    .get_pixel(0, 0)[1]
                    > 240
            );
        }
    }

    #[test]
    fn attention_supports_tall_viewports_and_portrait_sources_without_stretching() {
        for source in [off_center_subject(), off_center_subject().rotate90()] {
            let composed = attention_cover(&source, 90, 160)
                .expect("actual portrait attention crop")
                .to_rgb8();
            assert_eq!(composed.dimensions(), (90, 160));
            let points: Vec<_> = composed
                .enumerate_pixels()
                .filter(|(_, _, p)| p[0] > 200 && p[1] > 80 && p[2] < 60)
                .collect();
            assert!(points.len() > 400, "portrait crops retain the subject");
            let width = points.iter().map(|(x, _, _)| *x).max().unwrap()
                - points.iter().map(|(x, _, _)| *x).min().unwrap()
                + 1;
            let height = points.iter().map(|(_, y, _)| *y).max().unwrap()
                - points.iter().map(|(_, y, _)| *y).min().unwrap()
                + 1;
            assert!(
                width.abs_diff(height) <= 2,
                "portrait must preserve subject proportions: {width}x{height}"
            );
        }
    }

    #[test]
    fn native_failure_does_not_change_centered_fallback_geometry() {
        let source = off_center_subject();
        let result = attention_cover_using(
            &source,
            100,
            100,
            std::ffi::OsStr::new("uhc-nonexistent-vips-770"),
            Duration::from_millis(50),
        );
        assert!(result.is_err(), "missing executable must be observable");
        let fallback = attention_or_center(&source, 100, 100, result);
        assert_eq!((fallback.width(), fallback.height()), (100, 100));
        assert_eq!(
            fallback.to_rgba8(),
            resample(&source, (100, 0, 200, 200), 100, 100)
                .unwrap()
                .to_rgba8(),
            "centered 200x200 crop of the 400x200 source"
        );
    }

    #[test]
    fn extreme_aspect_ratio_uses_bounded_crop_before_resize() {
        let source =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(8192, 1, Rgba([255, 0, 0, 255])));
        let result = attention_cover(&source, 1, 8192);
        assert!(result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("intermediate pixel budget"));
        let composed = attention_or_center(&source, 1, 8192, result).to_rgb8();
        assert_eq!(composed.dimensions(), (1, 8192));
        assert!(composed.pixels().all(|p| *p == image::Rgb([255, 0, 0])));
    }

    #[test]
    fn raster_decode_honors_exif_orientation_before_cropping() {
        let mut jpeg = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 20, image::Rgb([255, 0, 0])))
            .write_with_encoder(JpegEncoder::new_with_quality(&mut jpeg, 90))
            .unwrap();
        let jpeg = jpeg.into_inner();
        // Little-endian TIFF orientation=6 (90 degrees clockwise).
        let exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut oriented = vec![0xff, 0xd8, 0xff, 0xe1];
        oriented.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        oriented.extend_from_slice(exif);
        oriented.extend_from_slice(&jpeg[2..]);
        let decoded = decode_artwork(&oriented).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (20, 40));
    }

    #[cfg(unix)]
    #[test]
    fn native_deadline_kills_and_reaps_stalled_child_and_removes_temporary_art() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("stall");
        let observed = dir.path().join("observed");
        // Record the private image directory and PID; exec avoids a descendant
        // shell that could survive killing the native process under test.
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" \"$2\" > '{}'\nexec sleep 30\n",
                observed.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let start = Instant::now();
        let result = attention_cover_using(
            &off_center_subject(),
            100,
            100,
            executable.as_os_str(),
            Duration::from_millis(150),
        );
        assert!(result.unwrap_err().to_string().contains("deadline"));
        assert!(start.elapsed() < Duration::from_secs(2));
        let record = std::fs::read_to_string(observed).unwrap();
        let mut lines = record.lines();
        let pid = lines.next().unwrap();
        let input = std::path::Path::new(lines.next().unwrap());
        assert!(
            !input.parent().unwrap().exists(),
            "temporary art directory must be removed"
        );
        let status = Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "timed-out child must be gone and reaped");
    }

    #[test]
    fn composed_images_reject_zero_oversized_and_malicious_source_dimensions() {
        let data = encode_solid_png(10, 10, Rgba([255, 0, 0, 255]));
        for (width, height) in [
            (0, 100),
            (100, 0),
            (u32::MAX, u32::MAX),
            (8_388_608, 1),
            (1, 8_388_608),
        ] {
            assert!(image_to_display_with_policy(
                &data,
                width,
                height,
                Rgb565ResizePolicy::default(),
                true
            )
            .is_err());
            assert!(jpeg_to_rgb565_with_policy(
                &data,
                width,
                height,
                Rgb565ResizePolicy::default()
            )
            .is_err());
            assert!(image_to_eink_acep6_with_policy(
                &data,
                width,
                height,
                Rgb565ResizePolicy::default()
            )
            .is_err());
        }
        let mut bmp = [0u8; 54];
        bmp[..2].copy_from_slice(b"BM");
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&100_000i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&100_000i32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        assert!(
            decode_artwork(&bmp).is_err(),
            "reject huge source before allocating pixels"
        );
    }

    #[test]
    fn smart_resize_mats_square_art_instead_of_cropping_it_to_widescreen() {
        let png = encode_solid_png(100, 100, image::Rgba([255, 0, 0, 255]));
        let result = jpeg_to_rgb565_with_policy(
            &png,
            160,
            90,
            Rgb565ResizePolicy::SmartCover {
                max_crop_percent: 10,
            },
        )
        .expect("smart conversion");

        assert_eq!(rgb565_at(&result, 0, 45), 0xFFDF, "left gallery mat");
        assert_eq!(rgb565_at(&result, 80, 45), 0xF800, "complete centered art");
        assert_eq!(rgb565_at(&result, 159, 45), 0xFFDF, "right gallery mat");
    }

    #[test]
    fn smart_resize_allows_a_small_center_crop_without_distortion() {
        // 5:3 into 16:9 loses 6.25% of source height: under the 10% budget.
        let png = encode_solid_png(100, 60, image::Rgba([255, 0, 0, 255]));
        let result = jpeg_to_rgb565_with_policy(
            &png,
            160,
            90,
            Rgb565ResizePolicy::SmartCover {
                max_crop_percent: 10,
            },
        )
        .expect("smart conversion");

        assert_eq!(rgb565_at(&result, 0, 0), 0xF800);
        assert_eq!(rgb565_at(&result, 159, 89), 0xF800);
    }

    #[test]
    fn fit_resize_never_changes_aspect_ratio_or_discards_square_art() {
        let png = encode_solid_png(100, 100, image::Rgba([0, 255, 0, 255]));
        let result = jpeg_to_rgb565_with_policy(&png, 160, 90, Rgb565ResizePolicy::Fit)
            .expect("fit conversion");

        assert_eq!(rgb565_at(&result, 0, 45), 0xFFDF);
        assert_eq!(rgb565_at(&result, 80, 45), 0x07E0);
    }

    #[test]
    fn acep6_packs_left_pixel_into_high_nibble_using_panel_indices() {
        let mut image = image::RgbImage::new(6, 1);
        for (pixel, color) in image.pixels_mut().zip(ACEP6_PALETTE) {
            *pixel = image::Rgb(color);
        }
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&image, 6, 1, image::ExtendedColorType::Rgb8)
            .expect("encode palette PNG");

        let result = image_to_eink_acep6_with_policy(&png, 6, 1, Rgb565ResizePolicy::Exact)
            .expect("convert palette");

        assert_eq!(result.data, vec![0x01, 0x23, 0x56]);
    }

    #[test]
    fn acep6_smart_fit_is_native_4bpp_and_preserves_gallery_mat() {
        let png = encode_solid_png(100, 100, image::Rgba([255, 0, 0, 255]));
        let result = image_to_eink_acep6_with_policy(
            &png,
            160,
            90,
            Rgb565ResizePolicy::SmartCover {
                max_crop_percent: 10,
            },
        )
        .expect("smart e-ink conversion");

        assert_eq!(result.data.len(), 160 * 90 / 2);
        assert_eq!(result.data[45 * 80], 0x11, "left edge remains white mat");
        assert_eq!(result.data[45 * 80 + 40], 0x33, "center remains red art");
        assert_eq!(
            result.data[45 * 80 + 79],
            0x11,
            "right edge remains white mat"
        );
    }

    #[test]
    fn test_rgb565_conversion() {
        // Create a simple 2x2 test image
        let mut img = image::RgbaImage::new(2, 2);

        // Red pixel (255, 0, 0)
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        // Green pixel (0, 255, 0)
        img.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        // Blue pixel (0, 0, 255)
        img.put_pixel(0, 1, image::Rgba([0, 0, 255, 255]));
        // White pixel (255, 255, 255)
        img.put_pixel(1, 1, image::Rgba([255, 255, 255, 255]));

        let dynamic_img = DynamicImage::ImageRgba8(img);
        let result = image_to_rgb565(&dynamic_img, 2, 2);

        assert_eq!(result.width, 2);
        assert_eq!(result.height, 2);
        assert_eq!(result.data.len(), 8); // 2x2 pixels * 2 bytes

        // Verify red pixel (R=31, G=0, B=0) -> 0xF800 -> LE: 0x00, 0xF8
        assert_eq!(result.data[0], 0x00);
        assert_eq!(result.data[1], 0xF8);

        // Verify green pixel (R=0, G=63, B=0) -> 0x07E0 -> LE: 0xE0, 0x07
        assert_eq!(result.data[2], 0xE0);
        assert_eq!(result.data[3], 0x07);

        // Verify blue pixel (R=0, G=0, B=31) -> 0x001F -> LE: 0x1F, 0x00
        assert_eq!(result.data[4], 0x1F);
        assert_eq!(result.data[5], 0x00);

        // Verify white pixel (R=31, G=63, B=31) -> 0xFFFF -> LE: 0xFF, 0xFF
        assert_eq!(result.data[6], 0xFF);
        assert_eq!(result.data[7], 0xFF);
    }

    #[test]
    fn test_placeholder_svg() {
        let svg = placeholder_svg(240, 240);
        assert!(svg.contains("width=\"240\""));
        assert!(svg.contains("height=\"240\""));
        assert!(svg.contains("No Image"));
    }

    #[test]
    fn test_svg_to_rgb565() {
        // Simple red square SVG
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">
            <rect width="100%" height="100%" fill="red"/>
        </svg>"#;

        let result = svg_to_rgb565(svg.as_bytes(), 2, 2).expect("SVG conversion should work");

        assert_eq!(result.width, 2);
        assert_eq!(result.height, 2);
        assert_eq!(result.data.len(), 8); // 2x2 pixels * 2 bytes

        // All pixels should be red: RGB565 0xF800 -> LE: 0x00, 0xF8
        for i in 0..4 {
            assert_eq!(result.data[i * 2], 0x00, "Red low byte at pixel {}", i);
            assert_eq!(result.data[i * 2 + 1], 0xF8, "Red high byte at pixel {}", i);
        }
    }

    #[test]
    fn test_placeholder_svg_to_rgb565() {
        // Verify placeholder SVG can be converted to RGB565
        let svg = placeholder_svg(240, 240);
        let result = svg_to_rgb565(svg.as_bytes(), 240, 240);
        assert!(result.is_ok(), "Placeholder SVG should convert to RGB565");

        let rgb565 = result.unwrap();
        assert_eq!(rgb565.width, 240);
        assert_eq!(rgb565.height, 240);
        assert_eq!(rgb565.data.len(), 240 * 240 * 2);
    }

    #[test]
    fn test_png_to_rgb565() {
        // Create a 2x2 red PNG programmatically
        let mut img = image::RgbaImage::new(2, 2);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgba([255, 0, 0, 255]); // Red
        }

        // Encode as PNG
        let mut png_data = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_data);
        encoder
            .write_image(&img, 2, 2, image::ExtendedColorType::Rgba8)
            .expect("PNG encoding should work");

        // Convert to RGB565
        let result = jpeg_to_rgb565(&png_data, 2, 2);
        assert!(
            result.is_ok(),
            "PNG should convert to RGB565: {:?}",
            result.err()
        );

        let rgb565 = result.unwrap();
        assert_eq!(rgb565.width, 2);
        assert_eq!(rgb565.height, 2);
        assert_eq!(rgb565.data.len(), 8); // 2x2 pixels * 2 bytes

        // All pixels should be red: RGB565 0xF800 -> LE: 0x00, 0xF8
        for i in 0..4 {
            assert_eq!(rgb565.data[i * 2], 0x00, "Red low byte at pixel {}", i);
            assert_eq!(rgb565.data[i * 2 + 1], 0xF8, "Red high byte at pixel {}", i);
        }
    }
}
