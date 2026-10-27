use crate::decode::RenderedImage;

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
