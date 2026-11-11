use crate::decode::RenderedImage;

const RADIUS: usize = 2;
const DETAIL_GAIN: i32 = 12;
const SAMPLE_SIZE: usize = 192;
const DETAIL_SCALE: i32 = 4 * 16;
const BORDER: usize = RADIUS + 1;

fn value(rgb: u32) -> u16 {
    let (r, g, b) = (rgb >> 16 & 255, rgb >> 8 & 255, rgb & 255);
    r.max(g).max(b) as u16
}

fn blur(src: &[u16], w: usize, h: usize) -> Vec<u16> {
    let mut blurred = vec![0; src.len()];
    let sum = |p: &[u16]| p[0] + 2 * p[1] + p[2];
    for (y, out) in blurred.chunks_exact_mut(w).enumerate().take(h - 1).skip(1) {
        let above = &src[(y - 1) * w..][..w];
        let row = &src[y * w..][..w];
        let below = &src[(y + 1) * w..][..w];
        for (x, out) in out[1..w - 1].iter_mut().enumerate() {
            *out = sum(&above[x..]) + 2 * sum(&row[x..]) + sum(&below[x..]);
        }
    }
    blurred
}

fn detail(smooth: &[u16], i: usize, dy: usize) -> i32 {
    4 * i32::from(smooth[i])
        - i32::from(smooth[i - RADIUS] + smooth[i + RADIUS] + smooth[i - dy] + smooth[i + dy])
}

fn centre_score(px: &mut [u32], smooth: &[u16], w: usize, h: usize) -> Option<f64> {
    if w < SAMPLE_SIZE + 2 * BORDER || h < SAMPLE_SIZE + 2 * BORDER {
        return None;
    }
    let (left, top) = ((w - SAMPLE_SIZE) / 2, (h - SAMPLE_SIZE) / 2);
    let (right, bottom) = (left + SAMPLE_SIZE, top + SAMPLE_SIZE);
    let mut sum = 0u64;
    for y in top..bottom {
        for x in left..right {
            sum += u64::from(detail(smooth, y * w + x, RADIUS * w).unsigned_abs());
        }
    }
    for y in top - 1..=bottom {
        let row = &mut px[y * w..][..w];
        let colour = 0x00ffff;
        if y == top - 1 || y == bottom {
            row[left - 1..=right].fill(colour);
        } else {
            row[left - 1] = colour;
            row[right] = colour;
        }
    }
    Some(sum as f64 / (SAMPLE_SIZE * SAMPLE_SIZE * DETAIL_SCALE as usize) as f64)
}

pub fn render(frame: &RenderedImage) -> (RenderedImage, Option<f64>) {
    let (w, h) = (frame.w as usize, frame.h as usize);
    // Luma gives saturated red and blue feathers little weight.
    let src: Vec<_> = frame.px.iter().copied().map(value).collect();
    let grey = |v| (64 + u32::from(v) / 2) * 0x010101;
    let mut px: Vec<_> = src.iter().copied().map(grey).collect();
    let mut score = None;
    if w > 2 * BORDER && h > 2 * BORDER {
        let smooth = blur(&src, w, h);
        let dy = RADIUS * w;
        for y in BORDER..h - BORDER {
            for x in BORDER..w - BORDER {
                let i = y * w + x;
                let level =
                    (px[i] & 255) as i32 + DETAIL_GAIN * detail(&smooth, i, dy) / DETAIL_SCALE;
                px[i] = level.clamp(0, 255) as u32 * 0x010101;
            }
        }
        score = centre_score(&mut px, &smooth, w, h);
    }
    (
        RenderedImage {
            w: frame.w,
            h: frame.h,
            px,
        },
        score,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_low_contrast_red_texture_is_amplified() {
        let w = 200;
        let frame = |amplitude: u32| RenderedImage {
            w,
            h: w,
            px: (0..w * w)
                .map(|i| {
                    let red = if i % 4 < 2 {
                        102 - amplitude
                    } else {
                        102 + amplitude
                    };
                    red << 16 | 0x1010
                })
                .collect(),
        };
        let (result, sharp) = render(&frame(2));
        let (_, soft) = render(&frame(1));
        let i = (w * 4 + 4) as usize;
        assert!((result.px[i + 2] & 255) - (result.px[i] & 255) > 4);
        assert!(sharp.unwrap() > soft.unwrap());
    }
}
