//! Image processing for S3 Knob LCD display
//!
//! The S3 Knob uses a 240x240 LCD that expects RGB565 format (2 bytes per pixel).
//! This module handles:
//! - JPEG, PNG, GIF, BMP, WebP decoding
//! - SVG rasterization (via resvg)
//! - Image resizing (bilinear)
//! - RGB565 conversion (little-endian for ESP32)

use image::{
    codecs::jpeg::JpegEncoder,
    imageops::{self, FilterType},
    DynamicImage, ImageFormat, Rgba, RgbaImage,
};
use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

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

const EINK_CACHE_CAPACITY: usize = 8;
type EinkCacheEntries = VecDeque<(EinkCacheKey, Arc<Vec<u8>>)>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EinkCacheKey {
    pub zone_id: String,
    pub image_key: String,
    pub width: u32,
    pub height: u32,
    pub resize_policy: Rgb565ResizePolicy,
    pub converter_version: u8,
}

/// Small process-local LRU for already packed Frame artwork. Eight 800x450
/// entries consume about 1.4 MiB while covering ordinary back/forward skips.
#[derive(Clone, Default)]
pub struct EinkArtworkCache {
    entries: Arc<Mutex<EinkCacheEntries>>,
}

impl EinkArtworkCache {
    pub fn get(&self, key: &EinkCacheKey) -> Option<Arc<Vec<u8>>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let index = entries.iter().position(|(candidate, _)| candidate == key)?;
        let entry = entries.remove(index)?;
        let data = Arc::clone(&entry.1);
        entries.push_front(entry);
        Some(data)
    }

    pub fn insert(&self, key: EinkCacheKey, data: Vec<u8>) -> Arc<Vec<u8>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(index) = entries.iter().position(|(candidate, _)| candidate == &key) {
            entries.remove(index);
        }
        let data = Arc::new(data);
        entries.push_front((key, Arc::clone(&data)));
        entries.truncate(EINK_CACHE_CAPACITY);
        data
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Rgb565ResizePolicy {
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
    let fitted = img
        .resize(target_w, target_h, FilterType::Triangle)
        .to_rgba8();
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
        Rgb565ResizePolicy::Exact => img.resize_exact(target_w, target_h, FilterType::Triangle),
        Rgb565ResizePolicy::Fit => fit_on_gallery_mat(img, target_w, target_h),
        Rgb565ResizePolicy::SmartCover { max_crop_percent }
            if cover_crop_percent(img.width(), img.height(), target_w, target_h)
                <= f64::from(max_crop_percent) =>
        {
            img.resize_to_fill(target_w, target_h, FilterType::Triangle)
        }
        Rgb565ResizePolicy::SmartCover { .. } => fit_on_gallery_mat(img, target_w, target_h),
    }
}

pub fn image_to_eink_acep6_with_policy(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
    policy: Rgb565ResizePolicy,
) -> Result<EinkAcep6Image, image::ImageError> {
    let image = image::load_from_memory(image_data)?;
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
    let img = image::load_from_memory(image_data)?;

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
        resized = img.resize_exact(target_width, target_height, FilterType::Triangle);
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
        img.resize_exact(target_width, target_height, FilterType::Triangle)
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
    fn eink_cache_key_prevents_stale_dimensions_and_evicts_lru() {
        let cache = EinkArtworkCache::default();
        let key = |index: usize, width: u32| EinkCacheKey {
            zone_id: "roon:zone".to_string(),
            image_key: format!("art-{index}"),
            width,
            height: 450,
            resize_policy: Rgb565ResizePolicy::SmartCover {
                max_crop_percent: 10,
            },
            converter_version: 1,
        };

        cache.insert(key(0, 800), vec![0]);
        assert!(cache.get(&key(0, 799)).is_none());
        for index in 1..=EINK_CACHE_CAPACITY {
            cache.insert(key(index, 800), vec![index as u8]);
        }
        assert!(cache.get(&key(0, 800)).is_none(), "oldest entry is evicted");
        assert_eq!(&*cache.get(&key(8, 800)).expect("newest entry"), &[8]);
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
