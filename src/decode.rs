use crate::tiff;
use anyhow::{anyhow, bail, Context, Result};
use fast_image_resize as fr;
use std::io::Cursor;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

pub struct DecodedImage {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
    pub turns: u8,
}

impl DecodedImage {
    pub fn upright_size(&self, turns: u8) -> (u32, u32) {
        if (self.turns + turns) % 2 == 1 {
            (self.h, self.w)
        } else {
            (self.w, self.h)
        }
    }
}

// These all look suspiciously similar, but:
//
// - ImageRect is an area in upright image coordinates
// - RenderedImage is the scaled, rotated 0RGB pixels ready to draw
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

pub struct RenderedImage {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

fn turns_for(orientation: u16) -> u8 {
    match orientation {
        6 => 1,
        3 => 2,
        8 => 3,
        _ => 0,
    }
}

pub fn decode(data: &[u8], orientation: Option<u16>) -> Result<DecodedImage> {
    if data.starts_with(&[0xff, 0xd8]) {
        decode_jpeg(data, orientation)
    } else if data.starts_with(b"\x89PNG") {
        decode_png(data)
    } else {
        bail!("Unsupported format")
    }
}

fn decode_jpeg(data: &[u8], orientation: Option<u16>) -> Result<DecodedImage> {
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(data), opts);
    let px = dec.decode().map_err(|err| anyhow!("{err}"))?;
    let info = dec.info().context("BUG: no JPEG info after decode")?;
    let orientation = orientation.or_else(|| tiff::exif_orientation(dec.exif()?));
    Ok(DecodedImage {
        w: info.width.into(),
        h: info.height.into(),
        px,
        turns: turns_for(orientation.unwrap_or(1)),
    })
}

fn decode_png(data: &[u8]) -> Result<DecodedImage> {
    let mut dec = png::Decoder::new(Cursor::new(data));
    dec.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = dec.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("PNG is too large")?];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());

    let blend = |c: u8, alpha: u8| {
        u8::try_from(u16::from(c) * u16::from(alpha) / 255).expect("BUG: blend exceeds u8")
    };
    let px = match info.color_type {
        png::ColorType::Rgba => buf
            .chunks_exact(4)
            .flat_map(|p| [blend(p[0], p[3]), blend(p[1], p[3]), blend(p[2], p[3])])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [blend(p[0], p[1]); 3])
            .collect(),
        other => bail!("Unexpected PNG colour type {other:?}"),
    };
    Ok(DecodedImage {
        w: info.width,
        h: info.height,
        px,
        turns: 0,
    })
}

pub fn fit(
    img: &DecodedImage,
    box_w: u32,
    box_h: u32,
    upscale: bool,
    turns: u8,
) -> Result<RenderedImage> {
    let (w, h) = img.upright_size(turns);
    let mut scale = f64::min(
        f64::from(box_w) / f64::from(w),
        f64::from(box_h) / f64::from(h),
    );
    if !upscale {
        scale = scale.min(1.0);
    }
    let dim = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
    let crop = ImageRect {
        x: 0.0,
        y: 0.0,
        w: f64::from(w),
        h: f64::from(h),
    };
    render(img, turns, crop, dim(w), dim(h))
}

/// Scaling down is limited by how fast memory can be read, so having more threads doesn't really
/// help here.
const NR_SCALE_THREADS: usize = 4;

#[derive(Clone, Copy)]
struct UnrotatedRect {
    left: f64,
    top: f64,
    w: f64,
    h: f64,
}

fn scale_rows(
    img: &DecodedImage,
    src: UnrotatedRect,
    (w, h): (u32, u32),
    first_row: usize,
    out: &mut [u32],
) -> Result<()> {
    let nr_rows = out.len() / w as usize;
    let src_rows_per_row = src.h / f64::from(h);
    let top = src.top + first_row as f64 * src_rows_per_row;
    let from = fr::images::ImageRef::new(img.w, img.h, &img.px, fr::PixelType::U8x3)?;
    let mut to = fr::images::Image::new(w, u32::try_from(nr_rows)?, fr::PixelType::U8x3);
    let filter = if f64::from(w) < src.w {
        fr::FilterType::Box
    } else {
        fr::FilterType::Bilinear
    };
    let opts = fr::ResizeOptions::new()
        .crop(src.left, top, src.w, nr_rows as f64 * src_rows_per_row)
        .resize_alg(fr::ResizeAlg::Convolution(filter))
        .use_alpha(false);
    fr::Resizer::new().resize(&from, &mut to, &opts)?;

    for (out, p) in out.iter_mut().zip(to.buffer().chunks_exact(3)) {
        *out = u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]);
    }
    Ok(())
}

fn scale(
    img: &DecodedImage,
    src: UnrotatedRect,
    (w, h): (u32, u32),
    nr_threads: usize,
) -> Result<Vec<u32>> {
    let mut px = vec![0; w as usize * h as usize];
    let rows_per_thread = (h as usize).div_ceil(nr_threads);
    std::thread::scope(|scope| {
        let threads: Vec<_> = px
            .chunks_mut(rows_per_thread * w as usize)
            .enumerate()
            .map(|(nr, out)| {
                scope.spawn(move || scale_rows(img, src, (w, h), nr * rows_per_thread, out))
            })
            .collect();
        threads
            .into_iter()
            .try_for_each(|thread| thread.join().expect("BUG: scaling thread panicked"))
    })?;
    Ok(px)
}

pub fn render(
    img: &DecodedImage,
    turns: u8,
    crop: ImageRect,
    w: u32,
    h: u32,
) -> Result<RenderedImage> {
    let turns = (img.turns + turns) % 4;
    let (img_w, img_h) = (f64::from(img.w), f64::from(img.h));
    // The pixels we have are not rotated, so this needs translation
    let (left, top, src_w, src_h) = match turns {
        1 => (crop.y, img_h - crop.x - crop.w, crop.h, crop.w),
        2 => (
            img_w - crop.x - crop.w,
            img_h - crop.y - crop.h,
            crop.w,
            crop.h,
        ),
        3 => (img_w - crop.y - crop.h, crop.x, crop.h, crop.w),
        _ => (crop.x, crop.y, crop.w, crop.h),
    };
    let (left, top) = (left.clamp(0.0, img_w - 1.0), top.clamp(0.0, img_h - 1.0));
    let src = UnrotatedRect {
        left,
        top,
        w: src_w.min(img_w - left),
        h: src_h.min(img_h - top),
    };
    let (w, h) = if turns % 2 == 1 { (h, w) } else { (w, h) };

    // Rows can only be scaled independently of each other without leaving seams if the filter
    // looks at nothing beyond what it is shrinking. That's the case for box, but not for bilinear.
    let nr_threads = if f64::from(w) < src.w {
        NR_SCALE_THREADS
    } else {
        1
    };
    let px = scale(img, src, (w, h), nr_threads)?;
    Ok(rotate(RenderedImage { w, h, px }, turns))
}

fn rotate(frame: RenderedImage, turns: u8) -> RenderedImage {
    let (w, h) = (frame.w as usize, frame.h as usize);
    let turns = turns % 4;
    if turns == 0 {
        return frame;
    }
    if turns == 2 {
        let mut px = frame.px;
        px.reverse();
        return RenderedImage { px, ..frame };
    }
    let mut px = Vec::with_capacity(w * h);
    for y in 0..w {
        for x in 0..h {
            let (src_x, src_y) = if turns == 1 {
                (y, h - 1 - x)
            } else {
                (w - 1 - y, x)
            };
            px.push(frame.px[src_y * w + src_x]);
        }
    }
    RenderedImage {
        w: frame.h,
        h: frame.w,
        px,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_in_bands_matches_scaling_at_once() {
        let (w, h) = (403u32, 301u32);
        let mut seed = 1u32;
        let px = std::iter::repeat_with(|| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed.to_le_bytes()[3]
        })
        .take((w * h * 3) as usize)
        .collect();
        let img = DecodedImage { w, h, px, turns: 0 };
        let src = UnrotatedRect {
            left: 10.5,
            top: 7.25,
            w: 380.0,
            h: 290.0,
        };
        let at_once = scale(&img, src, (97, 73), 1).unwrap();
        for nr_threads in [2, 4, 7] {
            assert_eq!(scale(&img, src, (97, 73), nr_threads).unwrap(), at_once);
        }
    }

    fn frame_2x3() -> RenderedImage {
        RenderedImage {
            w: 2,
            h: 3,
            px: vec![1, 2, 3, 4, 5, 6],
        }
    }

    #[test]
    fn test_rotate_quarter() {
        let rotated = rotate(frame_2x3(), 1);
        assert_eq!((rotated.w, rotated.h), (3, 2));
        assert_eq!(rotated.px, [5, 3, 1, 6, 4, 2]);
    }

    #[test]
    fn test_rotate_half() {
        let rotated = rotate(frame_2x3(), 2);
        assert_eq!((rotated.w, rotated.h), (2, 3));
        assert_eq!(rotated.px, [6, 5, 4, 3, 2, 1]);
    }

    #[test]
    fn test_rotate_three_quarters() {
        let rotated = rotate(frame_2x3(), 3);
        assert_eq!((rotated.w, rotated.h), (3, 2));
        assert_eq!(rotated.px, [2, 4, 6, 1, 3, 5]);
    }
}
