use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};

const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const FONT_HEIGHT: RasterHeight = RasterHeight::Size16;
const GLYPH_W: usize = get_raster_width(FONT_WEIGHT, FONT_HEIGHT);
const PADDING: usize = 2;
const TEXT_LEVEL: u32 = 0xc0;

pub const HEIGHT: usize = FONT_HEIGHT.val() + 2 * PADDING;

fn text_width(text: &str) -> usize {
    text.chars().count() * GLYPH_W
}

fn draw_text(buf: &mut [u32], buf_w: usize, x: usize, y: usize, text: &str) {
    for (nr, c) in text.chars().enumerate() {
        let glyph_x = x + nr * GLYPH_W;
        if glyph_x + GLYPH_W > buf_w {
            return;
        }
        let glyph = get_raster(c, FONT_WEIGHT, FONT_HEIGHT)
            .or_else(|| get_raster('?', FONT_WEIGHT, FONT_HEIGHT))
            .expect("BUG: missing fallback glyph");
        for (row, pixels) in glyph.rows().enumerate() {
            for (col, &alpha) in pixels.iter().enumerate().filter(|&(_, &alpha)| alpha != 0) {
                let idx = (y + row) * buf_w + glyph_x + col;
                if let Some(px) = buf.get_mut(idx) {
                    let alpha = u32::from(alpha);
                    let blend = |shift: u32| -> u32 {
                        let background = (*px >> shift) & 0xff;
                        (TEXT_LEVEL * alpha + background * (255 - alpha)) / 255
                    };
                    *px = (blend(16) << 16) | (blend(8) << 8) | blend(0);
                }
            }
        }
    }
}

pub fn draw(buf: &mut [u32], buf_w: usize, left: &str, right: &str) {
    buf.fill(0);
    let right_x = buf_w.saturating_sub(text_width(right) + PADDING);
    draw_text(buf, buf_w, right_x, PADDING, right);

    let nr_fitting = right_x.saturating_sub(PADDING + GLYPH_W) / GLYPH_W;
    let nr_skipped = left.chars().count().saturating_sub(nr_fitting);
    let left: String = left.chars().skip(nr_skipped).collect();
    draw_text(buf, buf_w, PADDING, PADDING, &left);
}

pub fn draw_message(buf: &mut [u32], buf_w: usize, text: &str) {
    let buf_h = buf.len() / buf_w;
    let x = buf_w.saturating_sub(text_width(text)) / 2;
    let y = buf_h.saturating_sub(FONT_HEIGHT.val()) / 2;
    draw_text(buf, buf_w, x, y, text);
}
