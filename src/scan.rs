//! Cheap change scan: find which screen cells changed without reading the whole screen.
//!
//! A full 2560x1440 grab costs the server 14.7 MB of pixel writes (~6 ms measured). To grab
//! only what changed we first need to know what changed *without* paying for that grab, so
//! this module samples one small tile per cell of a coarse grid — a few hundred pixels worth
//! of data instead of the whole frame — and compares each sample against the frame we already
//! hold (the reference). Cells whose sample differs are candidates; `dirty.rs`'s block merge
//! turns them into grab rectangles.
//!
//! Accepted trade-off: a change that fits entirely between two sample tiles is invisible to
//! the scan, so a resync (full grab) is needed periodically. Missing a change costs one stale
//! frame, which is why the resync interval is a policy knob rather than a guess.

use crate::diff::BlockGrid;

/// A coarse cell grid with one small sample tile per cell.
#[derive(Debug, Clone, Copy)]
pub struct SampleGrid {
    /// Cells, in the same block-index form `diff::merge_blocks` understands.
    pub cells: BlockGrid,
    /// Sample tile width in pixels.
    pub sample_w: usize,
    /// Sample tile height in pixels.
    pub sample_h: usize,
}

impl SampleGrid {
    /// Cover the screen with `cols` x `rows` cells, sampling `sample_w` x `sample_h` pixels at
    /// each cell's centre.
    pub fn new(screen_w: usize, screen_h: usize, cols: usize, rows: usize, sample_w: usize, sample_h: usize) -> Self {
        Self {
            cells: BlockGrid::for_screen(screen_w, screen_h, (screen_w / cols.max(1)).max(1), (screen_h / rows.max(1)).max(1)),
            sample_w: sample_w.max(1),
            sample_h: sample_h.max(1),
        }
    }

    /// Where cell `index` is on screen.
    pub fn cell_rect(&self, index: usize) -> Option<(usize, usize, usize, usize)> {
        let (col, row) = self.cells.col_row(index)?;
        Some(self.cells.span_rect(col, row, 1, 1))
    }

    /// The sample rectangle inside cell `index`, centred and clamped to the cell.
    ///
    /// Returns `None` for a cell too small to hold a sample. Centring matters: sampling the
    /// cell's corner would leave the middle of every cell unsampled in a systematic pattern.
    pub fn sample_rect(&self, index: usize) -> Option<(usize, usize, usize, usize)> {
        let (x, y, w, h) = self.cell_rect(index)?;
        if w < self.sample_w || h < self.sample_h {
            return None;
        }
        Some((
            x + (w - self.sample_w) / 2,
            y + (h - self.sample_h) / 2,
            self.sample_w,
            self.sample_h,
        ))
    }

    /// Number of cells, i.e. how many sample rectangles a scan costs.
    pub fn count(&self) -> usize {
        self.cells.count()
    }
}

/// Cells whose sample differs from the reference frame.
///
/// `samples[i]` is the pixels of `sample_rect(i)`, each with the rectangle's own stride. A
/// sample shorter than its rectangle, or a cell with no sample rectangle, is reported as
/// unchanged: absent data must not manufacture a dirty region.
pub fn changed_cells(
    reference: &[u8],
    samples: &[Vec<u8>],
    grid: &SampleGrid,
    screen_w: usize,
    bytes_per_pixel: usize,
) -> Vec<usize> {
    let mut out = Vec::new();
    for index in 0..grid.count() {
        let (x, y, w, h) = match grid.sample_rect(index) {
            Some(r) => r,
            None => continue,
        };
        let Some(sample) = samples.get(index) else { continue };
        if sample.len() < w * h * bytes_per_pixel {
            continue;
        }
        if sample_differ(reference, sample, screen_w, bytes_per_pixel, x, y, w, h) {
            out.push(index);
        }
    }
    out
}

/// Whether a sampled rectangle differs from the reference frame at its screen position.
fn sample_differ(
    reference: &[u8],
    sample: &[u8],
    screen_w: usize,
    bytes_per_pixel: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> bool {
    let row_len = w * bytes_per_pixel;
    for row in 0..h {
        let py = y + row;
        let off = (py * screen_w + x) * bytes_per_pixel;
        if off + row_len > reference.len() {
            return false;
        }
        let s = row * row_len;
        if s + row_len > sample.len() {
            return false;
        }
        if reference[off..off + row_len] != sample[s..s + row_len] {
            return true;
        }
    }
    false
}

/// Splice grabbed rectangles back into the reference frame.
///
/// `rects[i]` is the screen position of `tiles[i]`; the tile has the rectangle's own stride,
/// so it is copied row by row rather than as one contiguous block. Returns how many bytes were
/// actually written, so a caller can check that nothing was silently dropped.
pub fn splice_reference(
    reference: &mut [u8],
    screen_w: usize,
    bytes_per_pixel: usize,
    rects: &[(usize, usize, usize, usize)],
    tiles: &[Vec<u8>],
) -> usize {
    let mut written = 0usize;
    for (rect, tile) in rects.iter().zip(tiles.iter()) {
        let (x, y, w, h) = *rect;
        let row_len = w * bytes_per_pixel;
        for row in 0..h {
            let py = y + row;
            let off = (py * screen_w + x) * bytes_per_pixel;
            let s = row * row_len;
            if off + row_len > reference.len() || s + row_len > tile.len() {
                break;
            }
            reference[off..off + row_len].copy_from_slice(&tile[s..s + row_len]);
            written += row_len;
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 32x16 screen, 4-byte pixels, one painted rectangle.
    fn frame(w: usize, h: usize, paint: Option<(usize, usize, usize, usize)>) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for (i, chunk) in v.chunks_exact_mut(4).enumerate() {
            let px = i % w;
            let py = i / w;
            let on = paint
                .map(|(x, y, rw, rh)| px >= x && px < x + rw && py >= y && py < y + rh)
                .unwrap_or(false);
            chunk[2] = if on { 255 } else { 0 };
        }
        v
    }

    /// Cut sample rectangles out of a frame the way `grab_tiles` would return them.
    fn samples(frame: &[u8], grid: &SampleGrid, screen_w: usize, bpp: usize) -> Vec<Vec<u8>> {
        (0..grid.count())
            .map(|i| {
                let (x, y, w, h) = grid.sample_rect(i).unwrap();
                let mut out = Vec::with_capacity(w * h * bpp);
                for row in 0..h {
                    let off = ((y + row) * screen_w + x) * bpp;
                    out.extend_from_slice(&frame[off..off + w * bpp]);
                }
                out
            })
            .collect()
    }

    fn grid() -> SampleGrid {
        // 4x2 cells over a 32x16 screen, 4x4 sample tile centred in each cell.
        SampleGrid::new(32, 16, 4, 2, 4, 4)
    }

    #[test]
    fn sample_tiles_are_centred_in_their_cells() {
        let g = grid();
        assert_eq!(g.cell_rect(0), Some((0, 0, 8, 8)));
        assert_eq!(g.sample_rect(0), Some((2, 2, 4, 4)));
        assert_eq!(g.sample_rect(5), Some((10, 10, 4, 4)));
    }

    #[test]
    fn unchanged_screen_reports_no_cells() {
        let g = grid();
        let f = frame(32, 16, None);
        assert_eq!(changed_cells(&f, &samples(&f, &g, 32, 4), &g, 32, 4), Vec::<usize>::new());
    }

    #[test]
    fn a_change_inside_a_cell_flags_that_cell() {
        let g = grid();
        let base = frame(32, 16, None);
        // Paint exactly the sample tile of cell 6 (x 16..20, y 8..12).
        let changed = frame(32, 16, Some((16, 8, 4, 4)));
        assert_eq!(changed_cells(&base, &samples(&changed, &g, 32, 4), &g, 32, 4), vec![6]);
    }

    #[test]
    fn a_change_spanning_cells_flags_all_of_them() {
        let g = grid();
        let base = frame(32, 16, None);
        // A 12x2 block at (4,4) spans cells 0 and 1 (row 0 only: row 1's samples sit at y 10..14).
        let changed = frame(32, 16, Some((4, 4, 12, 2)));
        let cells = changed_cells(&base, &samples(&changed, &g, 32, 4), &g, 32, 4);
        assert_eq!(cells, vec![0, 1]);
    }

    #[test]
    fn a_change_between_sample_tiles_is_missed_by_design() {
        let g = grid();
        let base = frame(32, 16, None);
        // One pixel at (5,5): the sample of cell 0 spans 2..6, so this is inside it; pick a
        // spot outside every sample instead — (7,7) sits in cell 0's corner but outside 2..6.
        let mut changed = base.clone();
        let off = (7 * 32 + 7) * 4;
        changed[off] = 255;
        assert_eq!(
            changed_cells(&base, &samples(&changed, &g, 32, 4), &g, 32, 4),
            Vec::<usize>::new(),
            "unsampled space cannot be noticed"
        );
    }

    #[test]
    fn a_truncated_sample_is_not_counted_as_changed() {
        let g = grid();
        let base = frame(32, 16, None);
        let mut s = samples(&base, &g, 32, 4);
        s[3].truncate(4);
        assert_eq!(changed_cells(&base, &s, &g, 32, 4), Vec::<usize>::new());
    }

    #[test]
    fn splicing_tiles_updates_only_the_touched_rectangles() {
        let g = grid();
        let base = frame(32, 16, None);
        let changed = frame(32, 16, Some((16, 8, 4, 4)));
        let mut reference = base.clone();
        let rects = vec![(16usize, 8usize, 4usize, 4usize)];
        let tile = {
            let mut t = Vec::new();
            for row in 0..4usize {
                let off = ((8 + row) * 32 + 16) * 4;
                t.extend_from_slice(&changed[off..off + 16]);
            }
            t
        };
        let written = splice_reference(&mut reference, 32, 4, &rects, &[tile]);
        assert_eq!(written, 64);
        assert_eq!(reference, changed, "the reference now matches the new frame");
    }

    #[test]
    fn splicing_out_of_range_rows_is_bounded() {
        let mut reference = frame(16, 8, None);
        let rects = vec![(0usize, 7usize, 16usize, 4usize)]; // runs past the screen
        let tile = vec![9u8; 16 * 4 * 4];
        let written = splice_reference(&mut reference, 16, 4, &rects, &[tile]);
        assert_eq!(written, 16 * 4, "only the row that fits is written");
    }
}
