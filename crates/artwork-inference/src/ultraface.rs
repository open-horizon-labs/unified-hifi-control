// Derived from Imazen zensally-tract at a8888d08c6d4bd1fe0969cdce40da6c01b8032be.
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Imazen-Commercial

//! UltraFace RFB-320 face detector via tract.
//!
//! Lightweight SSD-style detector. Proven to work with tract in multiple projects.
//! Input: NCHW RGB float32, normalized (pixel - 127) / 128.
//! Outputs: scores [1, 4420, 2], boxes [1, 4420, 4] (already decoded, normalized 0-1).

use tract_nnef::prelude::*;
use zensally::{FaceDetector, FaceRect, ImageRef, PixelFormat};

/// Embedded gzip-compressed UltraFace RFB-320 NNEF deployment model.
const MODEL_GZ: &[u8] = include_bytes!("../models/ultraface.nnef.tar.gz");

/// Input dimensions: 320 wide x 240 tall.
const INPUT_W: usize = 320;
const INPUT_H: usize = 240;

/// Configuration for the UltraFace detector.
#[derive(Debug, Clone)]
pub struct UltraFaceConfig {
    /// Minimum face confidence to keep a detection.
    pub score_threshold: f32,
    /// IoU threshold for non-maximum suppression.
    pub nms_iou_threshold: f32,
}

impl Default for UltraFaceConfig {
    fn default() -> Self {
        Self {
            score_threshold: 0.7,
            nms_iou_threshold: 0.3,
        }
    }
}

/// A raw detection in pixel coordinates of the original image.
#[derive(Debug, Clone)]
struct RawDetection {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    confidence: f32,
}

fn iou(a: &RawDetection, b: &RawDetection) -> f32 {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);

    let intersection = (right - left).max(0.0) * (bottom - top).max(0.0);
    let area_a = a.width * a.height;
    let area_b = b.width * b.height;
    let union = area_a + area_b - intersection;

    if union <= 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn nms(mut detections: Vec<RawDetection>, iou_threshold: f32) -> Vec<RawDetection> {
    detections.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut keep = Vec::new();
    let mut suppressed = vec![false; detections.len()];

    for i in 0..detections.len() {
        if suppressed[i] {
            continue;
        }
        keep.push(detections[i].clone());
        for j in (i + 1)..detections.len() {
            if !suppressed[j] && iou(&detections[i], &detections[j]) >= iou_threshold {
                suppressed[j] = true;
            }
        }
    }

    keep
}

/// UltraFace RFB-320 face detector using tract for pure-Rust inference.
///
/// Proven tract compatibility. Input 320x240, ~1MB model.
pub struct UltraFaceDetector {
    model: Arc<TypedRunnableModel>,
    config: UltraFaceConfig,
    /// Reusable preprocessing buffer: [3, INPUT_H, INPUT_W] NCHW RGB.
    preprocess_buf: Vec<f32>,
}

impl UltraFaceDetector {
    /// Create a new detector with default configuration.
    pub fn new() -> Result<Self, anyhow::Error> {
        Self::with_config(UltraFaceConfig::default())
    }

    /// Create a new detector with custom configuration.
    pub fn with_config(config: UltraFaceConfig) -> Result<Self, anyhow::Error> {
        let model = crate::load_model(MODEL_GZ)?;

        let preprocess_buf = vec![0.0f32; 3 * INPUT_H * INPUT_W];

        Ok(Self {
            model,
            config,
            preprocess_buf,
        })
    }

    /// Preprocess: bilinear resize to 320x240 with letterboxing,
    /// RGB float32 normalized (pixel - 127) / 128, NCHW layout.
    /// Returns (pad_left, pad_top, ratio).
    fn preprocess(
        &mut self,
        pixels: &[u8],
        src_w: u32,
        src_h: u32,
        format: PixelFormat,
    ) -> (f32, f32, f32) {
        let bpp = format.bytes_per_pixel();

        let ratio = (INPUT_W as f32 / src_w as f32).min(INPUT_H as f32 / src_h as f32);
        let resized_w = (src_w as f32 * ratio).round() as u32;
        let resized_h = (src_h as f32 * ratio).round() as u32;
        let pad_left = (INPUT_W as u32 - resized_w) / 2;
        let pad_top = (INPUT_H as u32 - resized_h) / 2;

        // Fill with (127 - 127) / 128 = 0 (neutral padding)
        self.preprocess_buf.fill(0.0);

        let (r_idx, g_idx, b_idx) = match format {
            PixelFormat::Bgra => (2, 1, 0),
            PixelFormat::Rgba | PixelFormat::Rgb => (0, 1, 2),
        };

        let x_ratio = if resized_w > 1 {
            (src_w as f32 - 1.0) / (resized_w as f32 - 1.0)
        } else {
            0.0
        };
        let y_ratio = if resized_h > 1 {
            (src_h as f32 - 1.0) / (resized_h as f32 - 1.0)
        } else {
            0.0
        };
        let src_stride = src_w as usize * bpp;
        let plane_size = INPUT_H * INPUT_W;

        for dst_y in 0..resized_h as usize {
            let src_yf = dst_y as f32 * y_ratio;
            let y0 = src_yf as usize;
            let y1 = (y0 + 1).min(src_h as usize - 1);
            let fy = src_yf - y0 as f32;
            let fy_inv = 1.0 - fy;
            let out_y = dst_y + pad_top as usize;

            for dst_x in 0..resized_w as usize {
                let src_xf = dst_x as f32 * x_ratio;
                let x0 = src_xf as usize;
                let x1 = (x0 + 1).min(src_w as usize - 1);
                let fx = src_xf - x0 as f32;
                let fx_inv = 1.0 - fx;

                let off00 = y0 * src_stride + x0 * bpp;
                let off10 = y0 * src_stride + x1 * bpp;
                let off01 = y1 * src_stride + x0 * bpp;
                let off11 = y1 * src_stride + x1 * bpp;

                let w00 = fx_inv * fy_inv;
                let w10 = fx * fy_inv;
                let w01 = fx_inv * fy;
                let w11 = fx * fy;

                let r = pixels[off00 + r_idx] as f32 * w00
                    + pixels[off10 + r_idx] as f32 * w10
                    + pixels[off01 + r_idx] as f32 * w01
                    + pixels[off11 + r_idx] as f32 * w11;
                let g = pixels[off00 + g_idx] as f32 * w00
                    + pixels[off10 + g_idx] as f32 * w10
                    + pixels[off01 + g_idx] as f32 * w01
                    + pixels[off11 + g_idx] as f32 * w11;
                let b = pixels[off00 + b_idx] as f32 * w00
                    + pixels[off10 + b_idx] as f32 * w10
                    + pixels[off01 + b_idx] as f32 * w01
                    + pixels[off11 + b_idx] as f32 * w11;

                let out_x = dst_x + pad_left as usize;
                let pixel_idx = out_y * INPUT_W + out_x;

                // NCHW RGB layout, normalized: (pixel - 127) / 128
                self.preprocess_buf[pixel_idx] = (r - 127.0) / 128.0;
                self.preprocess_buf[plane_size + pixel_idx] = (g - 127.0) / 128.0;
                self.preprocess_buf[2 * plane_size + pixel_idx] = (b - 127.0) / 128.0;
            }
        }

        (pad_left as f32, pad_top as f32, ratio)
    }
}

impl FaceDetector for UltraFaceDetector {
    fn detect(&mut self, image: &ImageRef<'_>) -> Vec<FaceRect> {
        let (pad_left, pad_top, ratio) =
            self.preprocess(image.pixels, image.width, image.height, image.format);

        let input = match Tensor::from_shape(
            &[1, 3, INPUT_H, INPUT_W],
            &self.preprocess_buf[..3 * INPUT_H * INPUT_W],
        ) {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };

        let outputs = match self.model.run(tvec!(input.into())) {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };

        // outputs[0]: scores [1, 4420, 2] — [background, face]
        // outputs[1]: boxes [1, 4420, 4] — [xmin, ymin, xmax, ymax] normalized 0-1
        let scores = match crate::output::plain_f32(&outputs[0]) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let boxes = match crate::output::plain_f32(&outputs[1]) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let n_anchors = scores.len() / 2;
        let threshold = self.config.score_threshold;

        let input_w = INPUT_W as f32;
        let input_h = INPUT_H as f32;

        let mut detections = Vec::new();

        for i in 0..n_anchors {
            let face_score = scores[i * 2 + 1];
            if face_score < threshold {
                continue;
            }

            let bi = i * 4;
            // Boxes are normalized [0, 1] relative to 320x240 input
            let xmin = boxes[bi] * input_w;
            let ymin = boxes[bi + 1] * input_h;
            let xmax = boxes[bi + 2] * input_w;
            let ymax = boxes[bi + 3] * input_h;

            // Convert from padded input space to original image space
            let x = (xmin - pad_left) / ratio;
            let y = (ymin - pad_top) / ratio;
            let w = (xmax - xmin) / ratio;
            let h = (ymax - ymin) / ratio;

            detections.push(RawDetection {
                x,
                y,
                width: w,
                height: h,
                confidence: face_score,
            });
        }

        let detections = nms(detections, self.config.nms_iou_threshold);

        let img_w = image.width as f32;
        let img_h = image.height as f32;

        detections
            .into_iter()
            .map(|d| FaceRect {
                x1: (d.x / img_w * 100.0).clamp(0.0, 100.0),
                y1: (d.y / img_h * 100.0).clamp(0.0, 100.0),
                x2: ((d.x + d.width) / img_w * 100.0).clamp(0.0, 100.0),
                y2: ((d.y + d.height) / img_h * 100.0).clamp(0.0, 100.0),
                confidence: d.confidence,
            })
            .collect()
    }
}
