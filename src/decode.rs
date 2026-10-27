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
}

pub struct RenderedImage {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

pub fn decode(data: &[u8]) -> Result<DecodedImage> {
    if data.starts_with(&[0xff, 0xd8]) {
        decode_jpeg(data)
    } else if data.starts_with(b"\x89PNG") {
        decode_png(data)
    } else {
        bail!("Unsupported format")
    }
}

fn decode_jpeg(data: &[u8]) -> Result<DecodedImage> {
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(data), opts);
    let px = dec.decode().map_err(|err| anyhow!("{err}"))?;
    let info = dec.info().context("BUG: no JPEG info after decode")?;
    Ok(DecodedImage {
        w: info.width.into(),
        h: info.height.into(),
        px,
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
    })
}

pub fn fit(img: &DecodedImage, box_w: u32, box_h: u32, upscale: bool) -> Result<RenderedImage> {
    let mut scale = f64::min(
        f64::from(box_w) / f64::from(img.w),
        f64::from(box_h) / f64::from(img.h),
    );
    if !upscale {
        scale = scale.min(1.0);
    }
    let dim = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
    scale_to(img, dim(img.w), dim(img.h))
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
