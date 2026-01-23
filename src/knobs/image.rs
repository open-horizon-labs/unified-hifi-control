//! Image processing for S3 Knob LCD display
//!
//! The S3 Knob uses a 240x240 LCD that expects RGB565 format (2 bytes per pixel).
//! This module handles:
//! - JPEG, PNG, GIF, BMP, WebP decoding
//! - SVG rasterization (via resvg)
//! - Image resizing (bilinear)
//! - RGB565 conversion (little-endian for ESP32)

use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, DynamicImage, ImageFormat};
use std::io::Cursor;

/// RGB565 image data for LCD display
pub struct Rgb565Image {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Convert any image buffer (JPEG, PNG, SVG, etc.) to RGB565 format for ESP32 LCD
///
/// Returns RGB565 data in little-endian byte order (ESP32 native).
/// Supports JPEG, PNG, GIF, BMP, ICO, TIFF, WebP via the `image` crate,
/// and SVG via `resvg`.
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

    // Resize if needed
    let img = if img.width() != target_width || img.height() != target_height {
        img.resize_exact(target_width, target_height, FilterType::Triangle)
    } else {
        img
    };

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
