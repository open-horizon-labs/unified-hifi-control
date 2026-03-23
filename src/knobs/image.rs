//! Image processing for S3 Knob LCD display
//!
//! The S3 Knob uses a 240x240 LCD that expects RGB565 format (2 bytes per pixel).
//! This module handles:
//! - JPEG, PNG, GIF, BMP, WebP decoding
//! - SVG rasterization (via resvg)
//! - Image resizing (bilinear)
//! - RGB565 conversion (little-endian for ESP32)

use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, DynamicImage, ImageFormat};
use once_cell::sync::Lazy;
use std::io::Cursor;

/// RGB565 image data for LCD display
pub struct Rgb565Image {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub edge_color: [u8; 3],
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
    let img = if img.width() != target_width || img.height() != target_height {
        img.resize_to_fill(target_width, target_height, FilterType::Triangle)
    } else {
        img
    };

    // Extract edge color before RGB565 conversion
    let edge_color = extract_edge_color(&img);
    // Convert to RGB565
    let rgb565_data = rgba_to_rgb565(&img);
    Ok(Rgb565Image {
        data: rgb565_data,
        width: target_width,
        height: target_height,
        edge_color,
    })
}

/// Extract the average color from the outer ~15% border strip of an image.
/// This gives the color at the art's edge — the natural transition into the background.
pub fn extract_edge_color(img: &DynamicImage) -> [u8; 3] {
    let (w, h) = (img.width(), img.height());
    let border = (w.min(h) / 7).max(1); // ~15%
    let rgba = img.to_rgba8();
    let (mut r_sum, mut g_sum, mut b_sum, mut count) = (0u64, 0u64, 0u64, 0u64);
    for y in 0..h {
        for x in 0..w {
            if x < border || x >= w - border || y < border || y >= h - border {
                let p = rgba.get_pixel(x, y);
                r_sum += p[0] as u64;
                g_sum += p[1] as u64;
                b_sum += p[2] as u64;
                count += 1;
            }
        }
    }
    if count == 0 {
        return [0, 0, 0];
    }
    [
        (r_sum / count) as u8,
        (g_sum / count) as u8,
        (b_sum / count) as u8,
    ]
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
        edge_color: [0, 0, 0],
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
        resized = img.resize_to_fill(target_width, target_height, FilterType::Triangle);
        &resized
    } else {
        img
    };

    let edge_color = extract_edge_color(img_ref);
    let rgb565_data = rgba_to_rgb565(img_ref);
    Rgb565Image {
        data: rgb565_data,
        width: target_width,
        height: target_height,
        edge_color,
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
        img.resize_to_fill(target_width, target_height, FilterType::Triangle)
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

// ============================================================================
// E-ink ACeP 6-color dithering (Stucki + serpentine + CIELAB)
// ============================================================================

/// E-ink image data (4-bit packed palette indices)
pub struct EinkImage {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// ACeP 6-color palette entry
struct AcepColor {
    hw_index: u8,
    lab: [f32; 3],
}

/// sRGB-to-linear lookup table, computed once (avoids pow() per pixel)
static SRGB_LUT: Lazy<[f32; 256]> = Lazy::new(|| {
    let mut lut = [0.0f32; 256];
    for (i, entry) in lut.iter_mut().enumerate() {
        let c = i as f32 / 255.0;
        *entry = if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        };
    }
    lut
});

/// Convert sRGB to CIELAB (D65 illuminant)
fn rgb_to_lab(r: u8, g: u8, b: u8, lut: &[f32; 256]) -> [f32; 3] {
    let rl = lut[r as usize];
    let gl = lut[g as usize];
    let bl = lut[b as usize];

    // Linear RGB to XYZ (sRGB primaries, D65 white)
    let x = rl * 0.412_456_4 + gl * 0.357_576_1 + bl * 0.180_437_5;
    let y = rl * 0.212_672_9 + gl * 0.715_152_2 + bl * 0.072_175_0;
    let z = rl * 0.019_333_9 + gl * 0.119_192 + bl * 0.950_304_1;

    // XYZ to CIELAB
    const XN: f32 = 0.950_47;
    const YN: f32 = 1.0;
    const ZN: f32 = 1.088_83;

    let f = |t: f32| -> f32 {
        if t > 0.008_856 {
            t.cbrt()
        } else {
            t * 7.787_037 + 16.0 / 116.0
        }
    };

    let fx = f(x / XN);
    let fy = f(y / YN);
    let fz = f(z / ZN);

    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// Build the ACeP 6-color palette with pre-computed CIELAB values.
///
/// Hardware indices match the panel controller specification (Issue #263).
/// Display RGB values are calibrated to approximate actual e-ink panel output
/// rather than ideal sRGB primaries. Sources:
/// - PhotoPainter Spectra 6 converter (red, blue calibration)
/// - Common e-ink colorimetry measurements (yellow, green)
///
/// Tune these values per-panel for optimal results.
fn acep6_palette(lut: &[f32; 256]) -> [AcepColor; 6] {
    // (hardware_index, display_R, display_G, display_B)
    // These are calibrated to what the panel actually renders, not ideal sRGB.
    let entries: [(u8, u8, u8, u8); 6] = [
        (0, 0, 0, 0),       // Black — e-ink black is near-perfect
        (1, 255, 255, 255), // White — e-ink white is close to paper-white
        (2, 236, 213, 44),  // Yellow — warm, less saturated than sRGB yellow
        (3, 160, 32, 32),   // Red — calibrated from PhotoPainter (#a02020)
        (5, 80, 128, 184),  // Blue — calibrated from PhotoPainter (#5080b8)
        (6, 60, 160, 60),   // Green — muted compared to sRGB green
    ];

    entries.map(|(idx, r, g, b)| AcepColor {
        hw_index: idx,
        lab: rgb_to_lab(r, g, b, lut),
    })
}

/// Find nearest palette color by CIELAB Euclidean distance.
/// Returns (hardware_index, palette_lab).
fn nearest_acep_color(lab: &[f32; 3], palette: &[AcepColor; 6]) -> (u8, [f32; 3]) {
    let mut best = 0;
    let mut best_dist = f32::MAX;

    for (i, c) in palette.iter().enumerate() {
        let dl = lab[0] - c.lab[0];
        let da = lab[1] - c.lab[1];
        let db = lab[2] - c.lab[2];
        let dist = dl * dl + da * da + db * db;
        if dist < best_dist {
            best_dist = dist;
            best = i;
        }
    }

    (palette[best].hw_index, palette[best].lab)
}

/// Stucki error-diffusion kernel: (dx, row_offset, weight)
///
/// ```text
///             *   8   4
///     2   4   8   4   2
///     1   2   4   2   1
/// ```
/// Divisor: 42
const STUCKI_KERNEL: [(i32, usize, i32); 12] = [
    // Current row (ahead of pixel)
    (1, 0, 8),
    (2, 0, 4),
    // Next row
    (-2, 1, 2),
    (-1, 1, 4),
    (0, 1, 8),
    (1, 1, 4),
    (2, 1, 2),
    // Row after next
    (-2, 2, 1),
    (-1, 2, 2),
    (0, 2, 4),
    (1, 2, 2),
    (2, 2, 1),
];
const STUCKI_DIVISOR: f32 = 42.0;

/// Dither a decoded image to ACeP 6-color e-ink format.
///
/// Uses Stucki error-diffusion with serpentine scanning and CIELAB color matching
/// for best visual quality on limited-palette e-ink panels.
///
/// Returns 4-bit packed data (2 pixels per byte, high nibble first).
pub fn image_to_eink_acep6(img: &DynamicImage, target_width: u32, target_height: u32) -> EinkImage {
    let resized;
    let img_ref = if img.width() != target_width || img.height() != target_height {
        resized = img.resize_to_fill(target_width, target_height, FilterType::Triangle);
        &resized
    } else {
        img
    };

    let rgb = img_ref.to_rgb8();
    let width = target_width as usize;
    let height = target_height as usize;

    let lut = &*SRGB_LUT;
    let palette = acep6_palette(lut);

    // Error accumulation buffers: 3 rows in CIELAB space
    let mut err: [Vec<[f32; 3]>; 3] = [
        vec![[0.0; 3]; width],
        vec![[0.0; 3]; width],
        vec![[0.0; 3]; width],
    ];

    // Output: one palette index per pixel
    let mut indices = vec![0u8; width * height];

    for y in 0..height {
        let serpentine = y % 2 == 1;

        for xi in 0..width {
            let x = if serpentine { width - 1 - xi } else { xi };
            let pixel = rgb.get_pixel(x as u32, y as u32);
            let mut lab = rgb_to_lab(pixel[0], pixel[1], pixel[2], lut);

            // Add accumulated diffusion error
            lab[0] += err[0][x][0];
            lab[1] += err[0][x][1];
            lab[2] += err[0][x][2];

            // Find nearest palette color in CIELAB space
            let (hw_index, chosen_lab) = nearest_acep_color(&lab, &palette);
            indices[y * width + x] = hw_index;

            // Quantization error in CIELAB
            let qerr = [
                lab[0] - chosen_lab[0],
                lab[1] - chosen_lab[1],
                lab[2] - chosen_lab[2],
            ];

            // Distribute error via Stucki kernel (mirror dx for serpentine)
            for &(dx, dy, weight) in &STUCKI_KERNEL {
                let nx = if serpentine {
                    x as i32 - dx
                } else {
                    x as i32 + dx
                };

                if nx >= 0 && (nx as usize) < width && y + dy < height {
                    let nx = nx as usize;
                    let w = weight as f32 / STUCKI_DIVISOR;
                    err[dy][nx][0] += qerr[0] * w;
                    err[dy][nx][1] += qerr[1] * w;
                    err[dy][nx][2] += qerr[2] * w;
                }
            }
        }

        // Shift error buffers: recycle current row for row+2
        let mut recycled = std::mem::take(&mut err[0]);
        err[0] = std::mem::take(&mut err[1]);
        err[1] = std::mem::take(&mut err[2]);
        for v in recycled.iter_mut() {
            *v = [0.0; 3];
        }
        err[2] = recycled;
    }

    // Pack into 4-bit (2 pixels per byte, high nibble = first pixel)
    let bytes_per_row = width.div_ceil(2);
    let mut packed = vec![0u8; bytes_per_row * height];

    for y in 0..height {
        for x in (0..width).step_by(2) {
            let high = indices[y * width + x];
            let low = if x + 1 < width {
                indices[y * width + x + 1]
            } else {
                0
            };
            packed[y * bytes_per_row + x / 2] = (high << 4) | low;
        }
    }

    EinkImage {
        data: packed,
        width: target_width,
        height: target_height,
    }
}

/// Decode any image and dither to ACeP 6-color e-ink format.
///
/// Accepts JPEG, PNG, GIF, BMP, WebP. SVG placeholders are handled
/// separately in the route layer.
pub fn dither_to_eink_acep6(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
) -> Result<EinkImage, image::ImageError> {
    let img = image::load_from_memory(image_data)?;
    Ok(image_to_eink_acep6(&img, target_width, target_height))
}

/// Generate a placeholder e-ink image (solid black).
pub fn placeholder_eink(width: u32, height: u32) -> EinkImage {
    let bytes_per_row = (width as usize).div_ceil(2);
    EinkImage {
        data: vec![0u8; bytes_per_row * height as usize],
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

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

    // ========== E-ink dithering tests ==========

    #[test]
    fn test_rgb_to_lab_black() {
        let lut = &*SRGB_LUT;
        let lab = rgb_to_lab(0, 0, 0, lut);
        assert!(
            (lab[0]).abs() < 0.01,
            "Black L* should be ~0, got {}",
            lab[0]
        );
        assert!(
            (lab[1]).abs() < 0.5,
            "Black a* should be ~0, got {}",
            lab[1]
        );
        assert!(
            (lab[2]).abs() < 0.5,
            "Black b* should be ~0, got {}",
            lab[2]
        );
    }

    #[test]
    fn test_rgb_to_lab_white() {
        let lut = &*SRGB_LUT;
        let lab = rgb_to_lab(255, 255, 255, lut);
        assert!(
            (lab[0] - 100.0).abs() < 0.1,
            "White L* should be ~100, got {}",
            lab[0]
        );
        assert!(
            (lab[1]).abs() < 0.5,
            "White a* should be ~0, got {}",
            lab[1]
        );
        assert!(
            (lab[2]).abs() < 0.5,
            "White b* should be ~0, got {}",
            lab[2]
        );
    }

    #[test]
    fn test_nearest_acep_color_exact_match() {
        let lut = &*SRGB_LUT;
        let palette = acep6_palette(lut);

        // Calibrated palette RGB values should map to their own indices
        let test_cases: [(u8, u8, u8, u8); 6] = [
            (0, 0, 0, 0),       // Black → index 0
            (255, 255, 255, 1), // White → index 1
            (236, 213, 44, 2),  // Calibrated yellow → index 2
            (160, 32, 32, 3),   // Calibrated red → index 3
            (80, 128, 184, 5),  // Calibrated blue → index 5
            (60, 160, 60, 6),   // Calibrated green → index 6
        ];

        for (r, g, b, expected_idx) in test_cases {
            let lab = rgb_to_lab(r, g, b, lut);
            let (idx, _) = nearest_acep_color(&lab, &palette);
            assert_eq!(
                idx, expected_idx,
                "({},{},{}) should map to index {}",
                r, g, b, expected_idx
            );
        }
    }

    #[test]
    fn test_nearest_acep_color_ideal_primaries() {
        let lut = &*SRGB_LUT;
        let palette = acep6_palette(lut);

        // Ideal sRGB primaries should still map to the correct palette entries
        let test_cases: [(u8, u8, u8, u8); 4] = [
            (255, 255, 0, 2), // sRGB yellow → calibrated yellow
            (255, 0, 0, 3),   // sRGB red → calibrated red
            (0, 0, 255, 5),   // sRGB blue → calibrated blue
            (0, 255, 0, 6),   // sRGB green → calibrated green
        ];

        for (r, g, b, expected_idx) in test_cases {
            let lab = rgb_to_lab(r, g, b, lut);
            let (idx, _) = nearest_acep_color(&lab, &palette);
            assert_eq!(
                idx, expected_idx,
                "ideal ({},{},{}) should map to index {}",
                r, g, b, expected_idx
            );
        }
    }

    #[test]
    fn test_dither_solid_black() {
        let mut img = image::RgbaImage::new(4, 4);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgba([0, 0, 0, 255]);
        }
        let dynamic = DynamicImage::ImageRgba8(img);
        let result = image_to_eink_acep6(&dynamic, 4, 4);

        assert_eq!(result.width, 4);
        assert_eq!(result.height, 4);
        // 4 pixels wide → 2 bytes per row, 4 rows → 8 bytes
        assert_eq!(result.data.len(), 8);
        for byte in &result.data {
            assert_eq!(*byte, 0x00, "Black image should be all index 0");
        }
    }

    #[test]
    fn test_dither_solid_white() {
        let mut img = image::RgbaImage::new(4, 4);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgba([255, 255, 255, 255]);
        }
        let dynamic = DynamicImage::ImageRgba8(img);
        let result = image_to_eink_acep6(&dynamic, 4, 4);

        // All should be index 1: high nibble = 1, low nibble = 1 → 0x11
        for byte in &result.data {
            assert_eq!(*byte, 0x11, "White image should be all index 1");
        }
    }

    #[test]
    fn test_eink_output_size() {
        // Verify 4-bit packing with odd width
        let mut img = image::RgbaImage::new(5, 3);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgba([128, 128, 128, 255]);
        }
        let dynamic = DynamicImage::ImageRgba8(img);
        let result = image_to_eink_acep6(&dynamic, 5, 3);

        assert_eq!(result.width, 5);
        assert_eq!(result.height, 3);
        // ceil(5/2) = 3 bytes per row, 3 rows → 9 bytes
        assert_eq!(result.data.len(), 9);
    }

    #[test]
    fn test_placeholder_eink() {
        let result = placeholder_eink(800, 480);
        assert_eq!(result.width, 800);
        assert_eq!(result.height, 480);
        assert_eq!(result.data.len(), 400 * 480);
        assert!(result.data.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_dither_from_png() {
        // End-to-end: create red PNG, dither it
        let mut img = image::RgbaImage::new(10, 10);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgba([255, 0, 0, 255]);
        }

        let mut png_data = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_data);
        encoder
            .write_image(&img, 10, 10, image::ExtendedColorType::Rgba8)
            .expect("PNG encoding should work");

        let result = dither_to_eink_acep6(&png_data, 10, 10);
        assert!(result.is_ok(), "Should dither PNG: {:?}", result.err());

        let eink = result.unwrap();
        assert_eq!(eink.width, 10);
        assert_eq!(eink.height, 10);
        // 10/2 = 5 bytes per row, 10 rows = 50 bytes
        assert_eq!(eink.data.len(), 50);

        // Solid sRGB red dithered with calibrated palette — overwhelmingly index 3,
        // but error diffusion may produce a few non-red pixels at edges.
        let mut counts = [0usize; 8];
        for byte in &eink.data {
            counts[(byte >> 4) as usize] += 1;
            counts[(byte & 0x0F) as usize] += 1;
        }
        let total = (10 * 10) as usize;
        let red_count = counts[3];
        assert!(
            red_count > total * 90 / 100,
            "Solid red should be >90% index 3, got {}/{} ({:.0}%)",
            red_count,
            total,
            red_count as f64 / total as f64 * 100.0
        );
    }

    #[test]
    fn test_dither_gradient_exercises_diffusion() {
        // A gradient image forces error diffusion across many colors.
        // Verify output is well-formed and uses multiple palette indices.
        let width = 64u32;
        let height = 32u32;
        let mut img = image::RgbImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let r = ((x as f32 / width as f32) * 255.0) as u8;
                let g = ((y as f32 / height as f32) * 255.0) as u8;
                let b = 128u8;
                img.put_pixel(x, y, image::Rgb([r, g, b]));
            }
        }

        let dynamic = DynamicImage::ImageRgb8(img);
        let result = image_to_eink_acep6(&dynamic, width, height);

        let bytes_per_row = (width as usize).div_ceil(2);
        assert_eq!(result.data.len(), bytes_per_row * height as usize);

        // Collect unique palette indices used
        let mut seen = std::collections::HashSet::new();
        for byte in &result.data {
            seen.insert(byte >> 4);
            seen.insert(byte & 0x0F);
        }
        // A gradient should produce at least 3 distinct palette colors
        assert!(
            seen.len() >= 3,
            "Gradient should use ≥3 palette colors, got {:?}",
            seen
        );
    }
}
