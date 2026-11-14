use crate::decode::{self, DecodedImage, FitParams, ImageRect, ImageSize, RenderedImage};
use anyhow::Result;

const ZOOM_FACTOR: f64 = 1.5;
const MIN_ZOOM: f64 = 0.125;
const MAX_ZOOM: f64 = 8.0;

/// A pan moves by this fraction of the window, as in nsxiv.
const PAN_FRACTION: f64 = 0.2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScaleMode {
    ShrinkToFit,
    Fit,
    Zoom(f64),
}

pub enum ZoomStep {
    In,
    Out,
}

pub struct View {
    pub scale: ScaleMode,
    pub is_sharpness_shown: bool,
    /// How far the middle of the window is from the middle of the image, in image pixels.
    pub pan: (f64, f64),
    pub turns: u8,
}

pub struct Viewport {
    pub crop: ImageRect,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameParams {
    pub area: ImageSize,
    pub scale: ScaleMode,
    pan: (f64, f64),
    turns: u8,
}

impl FrameParams {
    pub fn is_zoom(self) -> bool {
        matches!(self.scale, ScaleMode::Zoom(_))
    }

    pub fn upright_size(self, img: ImageSize) -> ImageSize {
        img.rotated(self.turns)
    }

    fn fit_params(self) -> FitParams {
        FitParams {
            w: self.area.w,
            h: self.area.h,
            upscale: self.scale == ScaleMode::Fit,
            turns: self.turns,
        }
    }

    pub fn size(self, img: ImageSize) -> ImageSize {
        let img = self.upright_size(img);
        match self.scale {
            ScaleMode::Zoom(zoom) => {
                let region = region(zoom, self.pan, img, self.area);
                ImageSize {
                    w: region.w,
                    h: region.h,
                }
            }
            _ => decode::fit_size(img, self.fit_params()),
        }
    }

    pub fn render(self, img: &DecodedImage) -> Result<RenderedImage> {
        let ScaleMode::Zoom(zoom) = self.scale else {
            return decode::fit(img, self.fit_params());
        };
        let region = region(zoom, self.pan, img.upright_size(self.turns), self.area);
        decode::render(img, self.turns, region.crop, region.w, region.h)
    }

    pub fn render_draft(self, img: &DecodedImage) -> Result<RenderedImage> {
        decode::fit(
            img,
            FitParams {
                upscale: true,
                ..self.fit_params()
            },
        )
    }
}

impl View {
    pub fn new() -> Self {
        Self {
            scale: ScaleMode::ShrinkToFit,
            is_sharpness_shown: false,
            pan: (0.0, 0.0),
            turns: 0,
        }
    }

    pub fn step_zoom(&mut self, zoom: f64, step: ZoomStep) {
        let next = match step {
            ZoomStep::In if zoom < MAX_ZOOM => (zoom * ZOOM_FACTOR).clamp(MIN_ZOOM, MAX_ZOOM),
            ZoomStep::Out if zoom > MIN_ZOOM => (zoom / ZOOM_FACTOR).clamp(MIN_ZOOM, MAX_ZOOM),
            _ => return,
        };
        self.scale = ScaleMode::Zoom(next);
    }

    pub fn pan_by(
        &mut self,
        dir: (f64, f64),
        nr_px: Option<usize>,
        img: ImageSize,
        win: ImageSize,
    ) {
        let ScaleMode::Zoom(zoom) = self.scale else {
            return;
        };
        let pan_axis = |pan: &mut f64, dir: f64, img: u32, win: u32| {
            if dir == 0.0 {
                return;
            }
            let (_, _, max_pan) = axis_geometry(zoom, img, win);
            let step = nr_px.map_or(f64::from(win) * PAN_FRACTION, |nr| nr as f64) / zoom;
            *pan = (pan.clamp(-max_pan, max_pan) + dir * step).clamp(-max_pan, max_pan);
        };
        pan_axis(&mut self.pan.0, dir.0, img.w, win.w);
        pan_axis(&mut self.pan.1, dir.1, img.h, win.h);
    }

    pub fn frame_params(&self, area: ImageSize, is_current: bool) -> FrameParams {
        self.frame_params_at_scale(area, is_current, self.scale)
    }

    pub fn frame_params_at_scale(
        &self,
        area: ImageSize,
        is_current: bool,
        scale: ScaleMode,
    ) -> FrameParams {
        FrameParams {
            area,
            scale,
            pan: if matches!(scale, ScaleMode::Zoom(_)) {
                self.pan
            } else {
                (0.0, 0.0)
            },
            turns: if is_current { self.turns } else { 0 },
        }
    }

    #[cfg(test)]
    fn region(&self, zoom: f64, img: (u32, u32), win: (u32, u32)) -> Viewport {
        region(
            zoom,
            self.pan,
            ImageSize { w: img.0, h: img.1 },
            ImageSize { w: win.0, h: win.1 },
        )
    }
}

fn region(zoom: f64, pan: (f64, f64), img: ImageSize, win: ImageSize) -> Viewport {
    let axis = |img, win, pan: f64| {
        let (len, nr_shown, max_pan) = axis_geometry(zoom, img, win);
        let pan = pan.clamp(-max_pan, max_pan);
        let start = (f64::from(img) - len) / 2.0 + pan;
        let start = if zoom == 1.0 { start.round() } else { start };
        (start, len, nr_shown)
    };
    let (x, w, out_w) = axis(img.w, win.w, pan.0);
    let (y, h, out_h) = axis(img.h, win.h, pan.1);
    Viewport {
        crop: ImageRect { x, y, w, h },
        w: out_w,
        h: out_h,
    }
}

fn axis_geometry(zoom: f64, img: u32, win: u32) -> (f64, u32, f64) {
    let img = f64::from(img);
    let nr_shown = (img * zoom).round().clamp(1.0, f64::from(win));
    let len = (nr_shown / zoom).min(img);
    (len, nr_shown as u32, (img - len) / 2.0)
}

pub fn draw(buf: &mut [u32], buf_w: usize, buf_h: usize, frame: &RenderedImage) {
    let (frame_w, frame_h) = (frame.w as usize, frame.h as usize);
    let (w, h) = (frame_w.min(buf_w), frame_h.min(buf_h));
    let (src_x, src_y) = ((frame_w - w) / 2, (frame_h - h) / 2);
    let (dst_x, dst_y) = ((buf_w - w) / 2, (buf_h - h) / 2);

    buf[..dst_y * buf_w].fill(0);
    buf[(dst_y + h) * buf_w..].fill(0);
    for row in 0..h {
        let src = &frame.px[(src_y + row) * frame_w + src_x..][..w];
        let line = &mut buf[(dst_y + row) * buf_w..][..buf_w];
        line[..dst_x].fill(0);
        line[dst_x..dst_x + w].copy_from_slice(src);
        line[dst_x + w..].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_with_pan(pan: (f64, f64)) -> View {
        View { pan, ..View::new() }
    }

    #[test]
    fn test_region_centred() {
        let region = view_with_pan((0.0, 0.0)).region(1.0, (8640, 5760), (2560, 1600));
        let expected = ImageRect {
            x: 3040.0,
            y: 2080.0,
            w: 2560.0,
            h: 1600.0,
        };
        assert_eq!(region.crop, expected);
        assert_eq!((region.w, region.h), (2560, 1600));
    }

    #[test]
    fn test_region_at_one_to_one_does_not_interpolate() {
        let view = view_with_pan((0.0, 0.25));
        let crop = view.region(1.0, (601, 403), (300, 200)).crop;
        assert_eq!((crop.x, crop.y), (151.0, 102.0));
    }

    #[test]
    fn test_region_pan_is_clamped() {
        let view = view_with_pan((1e9, -1e9));
        let region = view.region(2.0, (1000, 1000), (500, 500));
        let expected = ImageRect {
            x: 750.0,
            y: 0.0,
            w: 250.0,
            h: 250.0,
        };
        assert_eq!(region.crop, expected);
        assert_eq!(view.pan, (1e9, -1e9));
    }

    #[test]
    fn test_region_small_image_is_not_panned() {
        let view = view_with_pan((50.0, 0.0));
        let region = view.region(1.0, (100, 80), (800, 600));
        let expected = ImageRect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 80.0,
        };
        assert_eq!(region.crop, expected);
        assert_eq!((region.w, region.h), (100, 80));
        assert_eq!(view.region(1.0, (1000, 800), (800, 600)).crop.x, 150.0);
    }

    #[test]
    fn test_pan_moves_inward_from_clamped_edge() {
        let mut view = view_with_pan((400.0, 400.0));
        view.scale = ScaleMode::Zoom(1.0);
        view.pan_by(
            (-1.0, 0.0),
            Some(10),
            ImageSize { w: 300, h: 300 },
            ImageSize { w: 200, h: 200 },
        );
        assert_eq!(view.pan, (40.0, 400.0));
        assert_eq!(view.region(1.0, (300, 300), (200, 200)).crop.x, 90.0);
    }

    #[test]
    fn test_step_zoom() {
        let mut view = View::new();
        view.step_zoom(0.375, ZoomStep::In);
        assert_eq!(view.scale, ScaleMode::Zoom(0.5625));
        view.step_zoom(0.5625, ZoomStep::Out);
        assert_eq!(view.scale, ScaleMode::Zoom(0.375));
        view.step_zoom(1.0, ZoomStep::In);
        assert_eq!(view.scale, ScaleMode::Zoom(1.5));
        view.step_zoom(7.0, ZoomStep::In);
        assert_eq!(view.scale, ScaleMode::Zoom(MAX_ZOOM));
        view.step_zoom(MAX_ZOOM, ZoomStep::In);
        assert_eq!(view.scale, ScaleMode::Zoom(MAX_ZOOM));
        view.step_zoom(0.15, ZoomStep::Out);
        assert_eq!(view.scale, ScaleMode::Zoom(MIN_ZOOM));
    }
}
