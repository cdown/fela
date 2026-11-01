use font8x8::legacy::BASIC_LEGACY;

const GLYPH_SIZE: usize = 8;
const PADDING: usize = 2;
const TEXT_COLOUR: u32 = 0x00c0_c0c0;

pub const HEIGHT: usize = GLYPH_SIZE + 2 * PADDING;

fn text_width(text: &str) -> usize {
    text.chars().count() * GLYPH_SIZE
}

fn draw_text(buf: &mut [u32], buf_w: usize, x: usize, y: usize, text: &str) {
    for (nr, c) in text.chars().enumerate() {
        let glyph = BASIC_LEGACY
            .get(c as usize)
            .unwrap_or(&BASIC_LEGACY[usize::from(b'?')]);
        let glyph_x = x + nr * GLYPH_SIZE;
        if glyph_x + GLYPH_SIZE > buf_w {
            return;
        }
        for (row, bits) in glyph.iter().enumerate() {
            for col in (0..GLYPH_SIZE).filter(|col| bits >> col & 1 == 1) {
                let idx = (y + row) * buf_w + glyph_x + col;
                if let Some(px) = buf.get_mut(idx) {
                    *px = TEXT_COLOUR;
                }
            }
        }
    }
}

pub fn draw(buf: &mut [u32], buf_w: usize, left: &str, right: &str) {
    buf.fill(0);
    let right_x = buf_w.saturating_sub(text_width(right) + PADDING);
    draw_text(buf, buf_w, right_x, PADDING, right);

    let nr_fitting = right_x.saturating_sub(PADDING + GLYPH_SIZE) / GLYPH_SIZE;
    let nr_skipped = left.chars().count().saturating_sub(nr_fitting);
    let left: String = left.chars().skip(nr_skipped).collect();
    draw_text(buf, buf_w, PADDING, PADDING, &left);
}

pub fn draw_message(buf: &mut [u32], buf_w: usize, text: &str) {
    let buf_h = buf.len() / buf_w;
    let x = buf_w.saturating_sub(text_width(text)) / 2;
    let y = buf_h.saturating_sub(GLYPH_SIZE) / 2;
    draw_text(buf, buf_w, x, y, text);
}
