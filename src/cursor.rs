//! Cursor sprites and their composition into captured frames.
//!
//! The pixels we grab from the root window do **not** contain the X cursor — the server draws
//! the cursor over the composite, not into the drawable — yet the official controlled side
//! ships the cursor inside the video. So the cursor has to be fetched separately
//! (`XFixesGetCursorImage`) and painted into the frame ourselves.
//!
//! This module is pure logic: given a sprite and a frame, blend. No I/O, deterministic tests.

/// One cursor sprite: `width * height` ARGB pixels, `A` in bits 24..31.
///
/// `x/y/hotspot` are carried for convenience when a fetch result is stored; the compositing
/// functions take the hotspot position explicitly so the logic stays testable without X.
pub struct Sprite {
    /// Sprite top-left the server reported for the cursor position at fetch time. Kept for
    /// diagnostics; compositing takes the live hotspot from `QueryPointer` instead, so a
    /// cached sprite never bakes in a stale position.
    #[allow(dead_code)]
    pub x: i32,
    #[allow(dead_code)]
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub serial: u32,
    pub argb: Vec<u32>,
}

impl Sprite {
    /// Pixels the sprite actually carries; guards a truncated reply.
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.argb.len() < self.pixel_count()
    }

    /// Screen rectangle `(x, y, w, h)` the sprite occupies, clipped to `screen_w * screen_h`
    /// and to what is visible. `hotspot_x/y` is the cursor point on screen. `None` when the
    /// sprite is entirely off-screen — which is also the "cursor hidden" case.
    pub fn screen_rect(
        &self,
        hotspot_x: i32,
        hotspot_y: i32,
        screen_w: usize,
        screen_h: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        if self.is_empty() {
            return None;
        }
        let left = hotspot_x - self.hotspot_x as i32;
        let top = hotspot_y - self.hotspot_y as i32;
        let x0 = left.max(0).min(screen_w as i32) as usize;
        let y0 = top.max(0).min(screen_h as i32) as usize;
        let x1 = (left + self.width as i32).clamp(0, screen_w as i32) as usize;
        let y1 = (top + self.height as i32).clamp(0, screen_h as i32) as usize;
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some((x0, y0, x1 - x0, y1 - y0))
    }
}

/// Paint `sprite` into `frame` at the cursor point `(hotspot_x, hotspot_y)`.
///
/// `frame` is packed 32-bit pixels (`bytes_per_pixel == 4`): bytes B, G, R, pad on
/// little-endian, i.e. a native u32 of `0x00RRGGBB`. Source is ARGB, so this is exactly the
/// "source over" blend `out = src*a + dst*(1-a)` per channel; the pad byte is left alone.
///
/// Returns the painted screen rectangle `(x, y, w, h)`, or `None` when nothing was visible.
/// This is the region an encoder has to re-encode because of the cursor — with a full-screen
/// readback it is *not* part of the content-diff dirty set, since the grabbed pixels never
/// contain the cursor.
pub fn composite(
    frame: &mut [u8],
    screen_w: usize,
    bytes_per_pixel: usize,
    sprite: &Sprite,
    hotspot_x: i32,
    hotspot_y: i32,
) -> Option<(usize, usize, usize, usize)> {
    if bytes_per_pixel != 4 || screen_w == 0 {
        return None;
    }
    let screen_h = frame.len() / (screen_w * bytes_per_pixel);
    let (dx, dy, w, h) = sprite.screen_rect(hotspot_x, hotspot_y, screen_w, screen_h)?;
    // Sprite top-left in screen coordinates; screen rows map to sprite rows by this offset.
    let top = hotspot_y - sprite.hotspot_y as i32;
    let left = hotspot_x - sprite.hotspot_x as i32;
    for row in 0..h {
        let sy = (dy + row) as i32 - top;
        if sy < 0 || sy as usize >= sprite.height as usize {
            continue;
        }
        let frow = (dy + row) * screen_w;
        let srow = sy as usize * sprite.width as usize;
        for col in 0..w {
            let sx = (dx + col) as i32 - left;
            if sx < 0 || sx as usize >= sprite.width as usize {
                continue;
            }
            let argb = sprite.argb[srow + sx as usize];
            let a = (argb >> 24) as u8;
            if a == 0 {
                continue;
            }
            let sr = (argb >> 16) as u8;
            let sg = (argb >> 8) as u8;
            let sb = argb as u8;
            let off = (frow + dx + col) * 4;
            if off + 4 > frame.len() {
                continue;
            }
            let db = frame[off];
            let dg = frame[off + 1];
            let dr = frame[off + 2];
            frame[off] = blend(sb, db, a);
            frame[off + 1] = blend(sg, dg, a);
            frame[off + 2] = blend(sr, dr, a);
        }
    }
    Some((dx, dy, w, h))
}

/// `src*a + dst*(1-a)` on one channel with ordinary round-half-up.
fn blend(src: u8, dst: u8, a: u8) -> u8 {
    if a == 255 {
        return src;
    }
    let v = src as u32 * a as u32 + dst as u32 * (255 - a as u32) + 127;
    (v / 255).min(255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 8;
    const H: usize = 4;

    /// A frame of one solid color, packed as B, G, R, pad.
    fn frame(b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut f = vec![0u8; W * H * 4];
        for px in f.chunks_mut(4) {
            px.copy_from_slice(&[b, g, r, 0]);
        }
        f
    }

    fn px(f: &[u8], x: usize, y: usize) -> [u8; 4] {
        let o = (y * W + x) * 4;
        [f[o], f[o + 1], f[o + 2], f[o + 3]]
    }

    fn sprite(argb: Vec<u32>, w: u16, h: u16, hot: (u16, u16)) -> Sprite {
        Sprite {
            x: 0,
            y: 0,
            width: w,
            height: h,
            hotspot_x: hot.0,
            hotspot_y: hot.1,
            serial: 1,
            argb,
        }
    }

    #[test]
    fn an_opaque_sprite_replaces_the_frame() {
        let s = sprite(vec![0xFFFF_0000; 4], 2, 2, (1, 1));
        let mut f = frame(0x40, 0x40, 0x40);
        // Hotspot at (3,2) with a 2x2 sprite centred on it: covers (2..4, 1..3).
        assert_eq!(composite(&mut f, W, 4, &s, 3, 2), Some((2, 1, 2, 2)));
        assert_eq!(px(&f, 3, 2), [0x00, 0x00, 0xFF, 0]);
        assert_eq!(px(&f, 2, 1), [0x00, 0x00, 0xFF, 0]);
        // Untouched neighbours.
        assert_eq!(px(&f, 1, 1), [0x40, 0x40, 0x40, 0]);
        assert_eq!(px(&f, 4, 3), [0x40, 0x40, 0x40, 0]);
    }

    #[test]
    fn a_fully_transparent_sprite_changes_nothing() {
        let s = sprite(vec![0x00FF_0000; 4], 2, 2, (1, 1));
        let mut f = frame(0x11, 0x22, 0x33);
        // The rectangle is reported (the sprite occupies it) but no pixel changes.
        assert_eq!(composite(&mut f, W, 4, &s, 3, 2), Some((2, 1, 2, 2)));
        assert_eq!(px(&f, 3, 2), [0x11, 0x22, 0x33, 0]);
    }

    #[test]
    fn a_half_alpha_sprite_blends() {
        // a=128 over white (0xFF): out = src*128 + 255*127 + 127 = 32769 → 128 for src=0.
        let s = sprite(vec![0x8000_0000], 1, 1, (0, 0));
        let mut f = frame(0xFF, 0xFF, 0xFF);
        assert_eq!(composite(&mut f, W, 4, &s, 3, 2), Some((3, 2, 1, 1)));
        assert_eq!(px(&f, 3, 2), [127, 127, 127, 0]);
    }

    #[test]
    fn a_sprite_clipped_at_the_top_left_corner_is_trimmed() {
        let s = sprite(vec![0xFFFF_0000; 16], 4, 4, (2, 2));
        let mut f = frame(0, 0, 0);
        // Hotspot at (1,1): sprite spans (-1..3, -1..3), visible part is (0..3, 0..3).
        assert_eq!(composite(&mut f, W, 4, &s, 1, 1), Some((0, 0, 3, 3)));
        assert_eq!(px(&f, 0, 0), [0x00, 0x00, 0xFF, 0]);
        assert_eq!(px(&f, 2, 2), [0x00, 0x00, 0xFF, 0]);
        assert_eq!(px(&f, 3, 3), [0, 0, 0, 0]);
    }

    #[test]
    fn a_sprite_offscreen_reports_nothing() {
        let s = sprite(vec![0xFFFF_0000; 16], 4, 4, (2, 2));
        let mut f = frame(0, 0, 0);
        assert_eq!(composite(&mut f, W, 4, &s, -10, -10), None);
        assert_eq!(composite(&mut f, W, 4, &s, W as i32 + 10, 0), None);
    }

    #[test]
    fn an_empty_sprite_is_nothing() {
        let s = sprite(vec![], 0, 0, (0, 0));
        let mut f = frame(0, 0, 0);
        assert_eq!(composite(&mut f, W, 4, &s, 3, 2), None);
    }

    #[test]
    fn a_truncated_sprite_is_treated_as_empty() {
        let s = sprite(vec![0xFFFF_0000; 3], 2, 2, (1, 1));
        let mut f = frame(0, 0, 0);
        assert_eq!(composite(&mut f, W, 4, &s, 3, 2), None);
    }
}
