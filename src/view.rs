use crate::decode::{ImageRect, RenderedImage};

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
    /// How far the middle of the window is from the middle of the image, in image pixels.
    pub pan: (f64, f64),
    pub turns: u8,
}

pub struct Viewport {
    pub crop: ImageRect,
    pub w: u32,
    pub h: u32,
}

impl View {
    pub fn new() -> Self {
        Self {
            scale: ScaleMode::ShrinkToFit,
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

    pub fn pan_by(&mut self, dir: (f64, f64), nr_px: Option<usize>, win: (u32, u32)) {
        let ScaleMode::Zoom(zoom) = self.scale else {
            return;
        };
        let step = |win: u32| nr_px.map_or(f64::from(win) * PAN_FRACTION, |nr| nr as f64) / zoom;
        self.pan.0 += dir.0 * step(win.0);
        self.pan.1 += dir.1 * step(win.1);
    }

    pub fn region(&mut self, zoom: f64, img: (u32, u32), win: (u32, u32)) -> Viewport {
        let axis = |img: u32, win: u32, pan: &mut f64| {
            let img = f64::from(img);
            let nr_shown = (img * zoom).round().clamp(1.0, f64::from(win));
            let len = (nr_shown / zoom).min(img);
            let max_pan = (img - len) / 2.0;
            *pan = pan.clamp(-max_pan, max_pan);
            (img / 2.0 + *pan - len / 2.0, len, nr_shown as u32)
        };
        let (x, w, out_w) = axis(img.0, win.0, &mut self.pan.0);
        let (y, h, out_h) = axis(img.1, win.1, &mut self.pan.1);
        Viewport {
            crop: ImageRect { x, y, w, h },
            w: out_w,
            h: out_h,
        }
    }
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
    fn test_region_pan_is_clamped() {
        let mut view = view_with_pan((1e9, -1e9));
        let region = view.region(2.0, (1000, 1000), (500, 500));
        let expected = ImageRect {
            x: 750.0,
            y: 0.0,
            w: 250.0,
            h: 250.0,
        };
        assert_eq!(region.crop, expected);
        assert_eq!(view.pan, (375.0, -375.0));
    }

    #[test]
    fn test_region_small_image_is_not_panned() {
        let region = view_with_pan((50.0, 0.0)).region(1.0, (100, 80), (800, 600));
        let expected = ImageRect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 80.0,
        };
        assert_eq!(region.crop, expected);
        assert_eq!((region.w, region.h), (100, 80));
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
