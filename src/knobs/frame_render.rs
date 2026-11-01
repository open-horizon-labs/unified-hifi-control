//! HiPhi Frame stock-panel rendering. Recipe physically preferred in #906:
//! panel-range luminance compression, measured RGB matching, floating-point
//! Floyd–Steinberg error diffusion. Hardware nibbles remain unchanged.
//!
//! Independently implemented from the reviewed behavior of
//! aitjcize/esp32-photoframe 04993f686c541a0afed030313e227d50928842ae
//! (main/image_processor.c, main/color_palette.c). Geometry stays in image.rs.

const PALETTE: [[f64; 3]; 6] = [
    [2., 2., 2.],
    [190., 200., 200.],
    [205., 202., 0.],
    [135., 19., 0.],
    [5., 64., 158.],
    [39., 102., 60.],
];
const PANEL: [u8; 6] = [0, 1, 2, 3, 5, 6];
fn linear(v: f64) -> f64 {
    let s = v / 255.;
    if s > 0.04045 {
        ((s + 0.055) / 1.055).powf(2.4)
    } else {
        s / 12.92
    }
}
fn luminance(rgb: [f64; 3]) -> f64 {
    0.2126729 * rgb[0] + 0.7151522 * rgb[1] + 0.0721750 * rgb[2]
}

pub(super) fn render(rgb: &[u8], width: usize, height: usize) -> Vec<u8> {
    assert_eq!(rgb.len(), width * height * 3);
    let to_linear: [f64; 256] = std::array::from_fn(|i| linear(i as f64));
    let to_srgb: [f64; 4096] = std::array::from_fn(|i| {
        let l = i as f64 / 4095.;
        let s = if l > 0.0031308 {
            1.055 * l.powf(1. / 2.4) - 0.055
        } else {
            12.92 * l
        };
        (s * 255.).round().clamp(0., 255.)
    });
    let black = luminance(PALETTE[0].map(linear));
    let range = luminance(PALETTE[1].map(linear)) - black;
    let stride = width.div_ceil(2);
    // White padding nibble for odd widths is never a reserved hardware color.
    let mut packed = vec![0x11; stride * height];
    let mut current = vec![[0.; 3]; width];
    let mut next = vec![[0.; 3]; width];
    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 3;
            let source = std::array::from_fn(|c| to_linear[rgb[offset + c] as usize]);
            let source_y = luminance(source);
            let target_y = black + source_y * range;
            let value: [f64; 3] = std::array::from_fn(|c| {
                let l = if source_y > 1e-6 {
                    source[c] * target_y / source_y
                } else {
                    black
                };
                let index = (l.clamp(0., 1.) * 4095.).round() as usize;
                (to_srgb[index] + current[x][c]).clamp(0., 255.)
            });
            let nearest = (0..6)
                .min_by(|&a, &b| {
                    let distance = |i: usize| -> f64 {
                        (0..3)
                            .map(|c| (value[c].round() - PALETTE[i][c]).powi(2))
                            .sum()
                    };
                    distance(a).total_cmp(&distance(b))
                })
                .unwrap_or(0);
            let cell = &mut packed[y * stride + x / 2];
            if x % 2 == 0 {
                *cell = (*cell & 15) | (PANEL[nearest] << 4);
            } else {
                *cell = (*cell & 0xf0) | PANEL[nearest];
            }
            for (dx, dy, numerator) in [(1, 0, 7.), (-1, 1, 3.), (0, 1, 5.), (1, 1, 1.)] {
                let nx = x as isize + dx;
                if nx < 0 || nx >= width as isize || y + dy >= height {
                    continue;
                }
                let target = if dy == 0 {
                    &mut current[nx as usize]
                } else {
                    &mut next[nx as usize]
                };
                for c in 0..3 {
                    target[c] += (value[c] - PALETTE[nearest][c]) * numerator / 16.;
                }
            }
        }
        std::mem::swap(&mut current, &mut next);
        next.fill([0.; 3]);
    }
    packed
}

#[cfg(test)]
mod tests {
    use super::render;
    #[test]
    fn odd_rows_keep_white_padding_and_never_emit_reserved_codes() {
        let packed = render(&[0; 18], 3, 2);
        assert_eq!(packed, [0x00, 0x01, 0x00, 0x01]);
        let rgb: Vec<u8> = (0..1539).map(|i| ((i * 73) % 256) as u8).collect();
        let packed = render(&rgb, 171, 3);
        assert_eq!(packed.len(), 86 * 3);
        assert!(packed
            .iter()
            .all(|b| [0, 1, 2, 3, 5, 6].contains(&(b >> 4))
                && [0, 1, 2, 3, 5, 6].contains(&(b & 15))));
        assert!((0..3).all(|y| packed[y * 86 + 85] & 15 == 1));
    }
}
