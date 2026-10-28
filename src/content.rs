//! Cheap "did the picture actually change?" check on grabbed frames.
//!
//! XDamage reports say the *drawable* was modified. On a compositing X11 session that is
//! true every frame regardless of whether anything visible moved: measured on this
//! machine's GNOME/mutter desktop, 85-97 damage reports per second with a screen whose only
//! change was a 64x48 rectangle, and every report's dirty box covered the whole screen.
//!
//! So damage alone cannot reproduce the official host's "static desktop = 3 fps, a few
//! hundred Kbps" behavior — it needs a second, content-based gate, which is this module.
//! It samples a coarse lattice of pixels from the frame and compares it with the previous
//! sample: a few hundred pixel reads, cheap enough to run on every captured frame.

/// Number of differing sampled pixels between two consecutive frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentDiff {
    /// Sampled pixels that differ from the previous frame.
    pub changed_pixels: usize,
    /// Total sampled pixels in this frame.
    pub total_pixels: usize,
    /// True when there was no previous frame to compare against.
    pub first: bool,
}

impl ContentDiff {
    /// Share of the sampled lattice that changed, 0.0..=1.0. The first frame reports 1.0
    /// because there is nothing to compare it to yet.
    pub fn share(&self) -> f64 {
        if self.total_pixels == 0 {
            0.0
        } else {
            self.changed_pixels as f64 / self.total_pixels as f64
        }
    }
}

/// Samples a fixed lattice of pixels from full-screen frames and diffs successive samples.
///
/// The lattice is deliberately coarse (default 32x18): it is a gate, not a metric. A change
/// smaller than one lattice step can be missed, which is the accepted trade-off — the point
/// is to stop feeding the encoder frames whose picture is identical.
pub struct ContentChange {
    grid_w: usize,
    grid_h: usize,
    step_x: usize,
    step_y: usize,
    last: Vec<u8>,
    have_last: bool,
}

impl ContentChange {
    /// Sample a `grid_w` x `grid_h` lattice spread over a `screen_w` x `screen_h` frame.
    pub fn new(screen_w: usize, screen_h: usize, grid_w: usize, grid_h: usize) -> Self {
        let grid_w = grid_w.clamp(1, screen_w.max(1));
        let grid_h = grid_h.clamp(1, screen_h.max(1));
        Self {
            grid_w,
            grid_h,
            step_x: (screen_w / grid_w).max(1),
            step_y: (screen_h / grid_h).max(1),
            last: vec![0u8; grid_w * grid_h * 3],
            have_last: false,
        }
    }

    /// Sample `frame` and report how it differs from the previous sample.
    ///
    /// `bytes_per_pixel` is the server's packed pixel size (32 here); only the low three
    /// bytes of each pixel are compared, so the padding byte cannot produce false changes.
    pub fn check(&mut self, frame: &[u8], screen_w: usize, bytes_per_pixel: usize) -> ContentDiff {
        if bytes_per_pixel < 3 {
            // A format whose pixels carry fewer than three components cannot be diffed this
            // way; report "changed" rather than silently comparing garbage.
            self.have_last = false;
            return ContentDiff {
                changed_pixels: self.grid_w * self.grid_h,
                total_pixels: self.grid_w * self.grid_h,
                first: true,
            };
        }

        let mut current = vec![0u8; self.grid_w * self.grid_h * 3];
        for gy in 0..self.grid_h {
            let py = gy * self.step_y;
            for gx in 0..self.grid_w {
                let px = gx * self.step_x;
                let off = (py * screen_w + px) * bytes_per_pixel;
                let slot = (gy * self.grid_w + gx) * 3;
                if off + 3 <= frame.len() && slot + 3 <= current.len() {
                    current[slot] = frame[off];
                    current[slot + 1] = frame[off + 1];
                    current[slot + 2] = frame[off + 2];
                }
            }
        }

        let first = !self.have_last;
        let changed_pixels = if first {
            self.grid_w * self.grid_h
        } else {
            (0..self.grid_w * self.grid_h)
                .filter(|&i| {
                    let s = i * 3;
                    self.last[s] != current[s]
                        || self.last[s + 1] != current[s + 1]
                        || self.last[s + 2] != current[s + 2]
                })
                .count()
        };

        std::mem::swap(&mut self.last, &mut current);
        self.have_last = true;

        ContentDiff {
            changed_pixels,
            total_pixels: self.grid_w * self.grid_h,
            first,
        }
    }

    /// Forget the previous sample, so the next frame is treated as a fresh reference.
    pub fn reset(&mut self) {
        self.have_last = false;
    }

    /// Lattice size, for reporting.
    pub fn grid(&self) -> (usize, usize) {
        (self.grid_w, self.grid_h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `w` x `h` frame of 4-byte pixels with a constant color.
    fn frame(w: usize, h: usize, rgb: [u8; 3]) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for (i, chunk) in v.chunks_exact_mut(4).enumerate() {
            let _ = i;
            chunk[0] = rgb[0];
            chunk[1] = rgb[1];
            chunk[2] = rgb[2];
            chunk[3] = 0xff;
        }
        v
    }

    #[test]
    fn first_frame_reports_all_samples_new() {
        let mut c = ContentChange::new(64, 64, 8, 8);
        let d = c.check(&frame(64, 64, [10, 20, 30]), 64, 4);
        assert!(d.first);
        assert_eq!(d.changed_pixels, 64);
        assert_eq!(d.total_pixels, 64);
        assert_eq!(d.share(), 1.0);
    }

    #[test]
    fn identical_frame_reports_no_change() {
        let mut c = ContentChange::new(64, 64, 8, 8);
        let f = frame(64, 64, [10, 20, 30]);
        c.check(&f, 64, 4);
        let d = c.check(&f, 64, 4);
        assert!(!d.first);
        assert_eq!(d.changed_pixels, 0);
        assert_eq!(d.share(), 0.0);
    }

    #[test]
    fn padding_byte_changes_are_ignored() {
        let mut c = ContentChange::new(64, 64, 8, 8);
        c.check(&frame(64, 64, [10, 20, 30]), 64, 4);
        // Same colors, different padding byte everywhere: must not look like a change.
        let mut other = frame(64, 64, [10, 20, 30]);
        for chunk in other.chunks_exact_mut(4) {
            chunk[3] = 0x00;
        }
        let d = c.check(&other, 64, 4);
        assert_eq!(d.changed_pixels, 0);
    }

    #[test]
    fn a_visible_change_is_detected() {
        let mut c = ContentChange::new(64, 64, 8, 8);
        c.check(&frame(64, 64, [10, 20, 30]), 64, 4);
        let d = c.check(&frame(64, 64, [11, 20, 30]), 64, 4);
        assert_eq!(d.changed_pixels, 64);
        assert!(d.share() > 0.0);
    }

    #[test]
    fn only_samples_covering_the_change_flag_it() {
        // 8x8 lattice over a 64x64 frame, step 8: repainting a 16x16 block at the origin
        // touches exactly the 2x2 corner samples.
        let mut c = ContentChange::new(64, 64, 8, 8);
        c.check(&frame(64, 64, [10, 20, 30]), 64, 4);
        let mut changed = frame(64, 64, [10, 20, 30]);
        for py in 0..16usize {
            for px in 0..16usize {
                let off = (py * 64 + px) * 4;
                changed[off] = 200;
            }
        }
        let d = c.check(&changed, 64, 4);
        assert_eq!(d.changed_pixels, 4);
    }

    #[test]
    fn a_change_between_sample_points_is_missed_by_design() {
        // Documents the accepted trade-off: a change that lands between lattice points is
        // invisible to this gate.
        let mut c = ContentChange::new(64, 64, 8, 8);
        c.check(&frame(64, 64, [10, 20, 30]), 64, 4);
        let mut changed = frame(64, 64, [10, 20, 30]);
        let off = (3 * 64 + 3) * 4; // inside a cell, but not on a lattice point
        changed[off] = 200;
        let d = c.check(&changed, 64, 4);
        assert_eq!(d.changed_pixels, 0);
    }

    #[test]
    fn reset_forces_the_next_frame_to_be_treated_as_new() {
        let mut c = ContentChange::new(64, 64, 8, 8);
        let f = frame(64, 64, [10, 20, 30]);
        c.check(&f, 64, 4);
        c.reset();
        let d = c.check(&f, 64, 4);
        assert!(d.first);
        assert_eq!(d.changed_pixels, 64);
    }
}
