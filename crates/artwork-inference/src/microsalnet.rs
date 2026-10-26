// Derived from Imazen zensally-tract at a8888d08c6d4bd1fe0969cdce40da6c01b8032be.
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Imazen-Commercial

//! MicroSalNet: lightweight general saliency detection via tract.
//!
//! Custom MobileNetV3-style encoder-decoder (~237K params).
//! Trained on DUTS-TR with knowledge distillation from U2-Netp.
//!
//! Input: NCHW RGB float32 [0, 1], 256x256 (stretched).
//! Output: single-channel saliency map [1, 1, 128, 128] in [0, 1].
//!
//! Uses ConvTranspose2d for upsampling (no Resize ops) to avoid
//! tract's slow bilinear Resize implementation. Output is half
//! input resolution (128x128) — sufficient for smart cropping.

use tract_nnef::prelude::*;
use zensally::{ImageRef, PixelFormat, SaliencyDetector, SaliencyMap};

/// Embedded gzip-compressed MicroSalNet NNEF deployment model.
const MODEL_GZ: &[u8] = include_bytes!("../models/microsalnet.nnef.tar.gz");

const INPUT_SIZE: usize = 256;
const OUTPUT_SIZE: usize = 128;

/// MicroSalNet saliency detector using tract for pure-Rust inference.
pub struct MicroSalNet {
    model: Arc<TypedRunnableModel>,
    /// Reusable preprocessing buffer: [3, 256, 256] NCHW RGB.
    preprocess_buf: Vec<f32>,
}

impl MicroSalNet {
    /// Create a new MicroSalNet saliency detector.
    pub fn new() -> Result<Self, anyhow::Error> {
        let model = crate::load_model(MODEL_GZ)?;

        let preprocess_buf = vec![0.0f32; 3 * INPUT_SIZE * INPUT_SIZE];

        Ok(Self {
            model,
            preprocess_buf,
        })
    }

    /// Preprocess: bilinear resize to 256x256 (stretch), RGB float32 [0, 1], NCHW layout.
    fn preprocess(&mut self, pixels: &[u8], src_w: u32, src_h: u32, format: PixelFormat) {
        let bpp = format.bytes_per_pixel();

        let (r_idx, g_idx, b_idx) = match format {
            PixelFormat::Bgra => (2, 1, 0),
            PixelFormat::Rgba | PixelFormat::Rgb => (0, 1, 2),
        };

        let x_ratio = if INPUT_SIZE > 1 {
            (src_w as f32 - 1.0) / (INPUT_SIZE as f32 - 1.0)
        } else {
            0.0
        };
        let y_ratio = if INPUT_SIZE > 1 {
            (src_h as f32 - 1.0) / (INPUT_SIZE as f32 - 1.0)
        } else {
            0.0
        };
        let src_stride = src_w as usize * bpp;
        let plane_size = INPUT_SIZE * INPUT_SIZE;

        for dst_y in 0..INPUT_SIZE {
            let src_yf = dst_y as f32 * y_ratio;
            let y0 = src_yf as usize;
            let y1 = (y0 + 1).min(src_h as usize - 1);
            let fy = src_yf - y0 as f32;
            let fy_inv = 1.0 - fy;

            for dst_x in 0..INPUT_SIZE {
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

                let pixel_idx = dst_y * INPUT_SIZE + dst_x;

                // NCHW RGB, scale to [0,1] only (matching training preprocessing)
                self.preprocess_buf[pixel_idx] = r / 255.0;
                self.preprocess_buf[plane_size + pixel_idx] = g / 255.0;
                self.preprocess_buf[2 * plane_size + pixel_idx] = b / 255.0;
            }
        }
    }
}

impl SaliencyDetector for MicroSalNet {
    fn saliency_map(&mut self, image: &ImageRef<'_>) -> SaliencyMap {
        self.preprocess(image.pixels, image.width, image.height, image.format);

        let input = match Tensor::from_shape(
            &[1, 3, INPUT_SIZE, INPUT_SIZE],
            &self.preprocess_buf[..3 * INPUT_SIZE * INPUT_SIZE],
        ) {
            Ok(t) => t,
            Err(_) => {
                return SaliencyMap {
                    data: vec![0.0; OUTPUT_SIZE * OUTPUT_SIZE],
                    width: OUTPUT_SIZE as u32,
                    height: OUTPUT_SIZE as u32,
                };
            }
        };

        let outputs = match self.model.run(tvec!(input.into())) {
            Ok(o) => o,
            Err(_) => {
                return SaliencyMap {
                    data: vec![0.0; OUTPUT_SIZE * OUTPUT_SIZE],
                    width: OUTPUT_SIZE as u32,
                    height: OUTPUT_SIZE as u32,
                };
            }
        };

        let raw = match crate::output::plain_f32(&outputs[0]) {
            Ok(s) => s,
            Err(_) => {
                return SaliencyMap {
                    data: vec![0.0; OUTPUT_SIZE * OUTPUT_SIZE],
                    width: OUTPUT_SIZE as u32,
                    height: OUTPUT_SIZE as u32,
                };
            }
        };

        // Output is already sigmoided [0, 1] — just clamp
        let data: Vec<f32> = raw.iter().map(|&v| v.clamp(0.0, 1.0)).collect();

        SaliencyMap {
            data,
            width: OUTPUT_SIZE as u32,
            height: OUTPUT_SIZE as u32,
        }
    }
}
