//! Image processing for S3 Knob LCD display
//!
//! The S3 Knob uses a 240x240 LCD that expects RGB565 format (2 bytes per pixel).
//! This module handles:
//! - JPEG decoding
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

/// Convert any image buffer (JPEG, PNG, etc.) to RGB565 format for ESP32 LCD
///
/// Returns RGB565 data in little-endian byte order (ESP32 native).
/// Supports JPEG, PNG, GIF, BMP, ICO, TIFF, WebP, and other formats via the `image` crate.
pub fn jpeg_to_rgb565(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
) -> Result<Rgb565Image, image::ImageError> {
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
}
