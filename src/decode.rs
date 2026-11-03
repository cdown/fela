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
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
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

    let mut px: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        other => bail!("Unexpected PNG colour type {other:?}"),
    };
    for p in px.chunks_exact_mut(4) {
        let alpha = u16::from(p[3]);
        for c in &mut p[..3] {
            *c = u8::try_from(u16::from(*c) * alpha / 255).expect("BUG: blend exceeds u8");
        }
    }
    Ok(DecodedImage {
        w: info.width,
        h: info.height,
        px,
        turns: 0,
    })
}

pub fn fit(img: &DecodedImage, box_w: u32, box_h: u32, upscale: bool) -> Result<RenderedImage> {
    let (box_w, box_h) = if img.turns % 2 == 1 {
        (box_h, box_w)
    } else {
        (box_w, box_h)
    };
    let mut scale = f64::min(
        f64::from(box_w) / f64::from(img.w),
        f64::from(box_h) / f64::from(img.h),
    );
    if !upscale {
        scale = scale.min(1.0);
    }
    let dim = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
    Ok(rotate(scale_to(img, dim(img.w), dim(img.h))?, img.turns))
}

fn scale_to(img: &DecodedImage, w: u32, h: u32) -> Result<RenderedImage> {
    let src = fr::images::ImageRef::new(img.w, img.h, &img.px, fr::PixelType::U8x4)?;
    let mut dst = fr::images::Image::new(w, h, fr::PixelType::U8x4);
    let filter = if w < img.w {
        fr::FilterType::Box
    } else {
        fr::FilterType::Bilinear
    };
    let opts = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(filter))
        .use_alpha(false);
    fr::Resizer::new().resize(&src, &mut dst, &opts)?;

    let px = dst
        .buffer()
        .chunks_exact(4)
        .map(|p| u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]))
        .collect();
    Ok(RenderedImage { w, h, px })
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
