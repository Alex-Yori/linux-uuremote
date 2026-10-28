//! Block diff between two full-screen frames: where did the picture actually differ?
//!
//! XDamage says the drawable was modified, which on a compositing server is true every frame
//! (measured: dirty box covers 100% of the screen while only a 64x48 rectangle moved). So the
//! dirty region an incremental grab needs has to come from the pixels themselves, which is
//! what this module computes: compare `prev` and `cur` block by block, then merge the blocks
//! that differ into a small number of screen rectangles.
//!
//! Pure logic, no I/O — deterministic tests, no display required.

/// Block grid over a screen, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockGrid {
    /// Screen width this grid was built for, so partial edge blocks can be clamped exactly.
    pub screen_w: usize,
    /// Screen height this grid was built for.
    pub screen_h: usize,
    /// Pixels per block horizontally.
    pub block_w: usize,
    /// Pixels per block vertically.
    pub block_h: usize,
    /// Blocks across.
    pub cols: usize,
    /// Blocks down.
    pub rows: usize,
}

impl BlockGrid {
    /// Cover a `screen_w` x `screen_h` screen with `block_w` x `block_h` blocks.
    ///
    /// The last column/row may be narrower than a full block; `rect` clamps to the screen.
    pub fn for_screen(screen_w: usize, screen_h: usize, block_w: usize, block_h: usize) -> Self {
        let block_w = block_w.max(1);
        let block_h = block_h.max(1);
        Self {
            screen_w,
            screen_h,
            block_w,
            block_h,
            cols: screen_w.div_ceil(block_w),
            rows: screen_h.div_ceil(block_h),
        }
    }

    pub fn count(&self) -> usize {
        self.cols * self.rows
    }

    /// Pixel rectangle of block `index`, clamped to the screen, or `None` if out of range.
    pub fn rect(&self, index: usize) -> Option<(usize, usize, usize, usize)> {
        if index >= self.count() {
            return None;
        }
        let col = index % self.cols;
        let row = index / self.cols;
        Some(self.block_rect(col, row))
    }

    /// Pixel rectangle of block `(col, row)`, clamped to the screen it was built for.
    pub fn block_rect(&self, col: usize, row: usize) -> (usize, usize, usize, usize) {
        let x = col * self.block_w;
        let y = row * self.block_h;
        if x >= self.screen_w || y >= self.screen_h {
            return (x, y, 0, 0);
        }
        self.span_rect(col, row, 1, 1)
    }

    /// Pixel rectangle spanning `cols` x `rows` blocks from `(col, row)`, clamped to the screen.
    pub fn span_rect(
        &self,
        col: usize,
        row: usize,
        cols: usize,
        rows: usize,
    ) -> (usize, usize, usize, usize) {
        let x = col * self.block_w;
        let y = row * self.block_h;
        if x >= self.screen_w || y >= self.screen_h {
            return (x, y, 0, 0);
        }
        (
            x,
            y,
            (cols * self.block_w).min(self.screen_w - x),
            (rows * self.block_h).min(self.screen_h - y),
        )
    }

    pub fn col_row(&self, index: usize) -> Option<(usize, usize)> {
        if index >= self.count() {
            return None;
        }
        Some((index % self.cols, index / self.cols))
    }
}

/// Indices of blocks whose pixels differ between `prev` and `cur`.
///
/// `prev` is the frame we already sent; `cur` is the freshly grabbed one. Blocks that fall
/// outside either buffer are skipped rather than counted as changed, so a short buffer cannot
/// manufacture a dirty region out of missing data.
pub fn dirty_blocks(
    prev: &[u8],
    cur: &[u8],
    screen_w: usize,
    bytes_per_pixel: usize,
    grid: &BlockGrid,
) -> Vec<usize> {
    let mut out = Vec::new();
    for index in 0..grid.count() {
        let (x, y, w, h) = match grid.rect(index) {
            Some(r) => r,
            None => break,
        };
        if blocks_differ(prev, cur, screen_w, bytes_per_pixel, x, y, w, h) {
            out.push(index);
        }
    }
    out
}

/// Whether any pixel of the rectangle differs between the two frames.
///
/// Compares row slices rather than pixel colors: the padding byte is part of the slice, so a
/// format that varies padding would report a change. That is acceptable here because the same
/// server produced both buffers; a padding-only difference would mean the grab is not stable
/// and treating it as dirty is the safe direction.
pub fn blocks_differ(
    prev: &[u8],
    cur: &[u8],
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
        if off + row_len > prev.len() || off + row_len > cur.len() {
            return false;
        }
        if prev[off..off + row_len] != cur[off..off + row_len] {
            return true;
        }
    }
    false
}

/// Merge dirty block indices into at most `max_rects` pixel rectangles `(x, y, w, h)`.
///
/// Strategy, in order:
///   1. each row's contiguous runs of dirty blocks become one rectangle;
///   2. vertically adjacent rectangles with the same horizontal span are joined;
///   3. if that is still more than `max_rects`, the pair whose union wastes the least area is
///      merged repeatedly until the cap is met.
///
/// Step 3 deliberately trades precision for a bounded number of grabs: one rectangle covering
/// two far-apart changes grabs some clean pixels, which beats paying for another round trip.
pub fn merge_blocks(
    indices: &[usize],
    grid: &BlockGrid,
    max_rects: usize,
) -> Vec<(usize, usize, usize, usize)> {
    if indices.is_empty() || max_rects == 0 {
        return Vec::new();
    }

    let mut dirty = vec![false; grid.count()];
    for &i in indices {
        if i < dirty.len() {
            dirty[i] = true;
        }
    }

    // 1. Horizontal runs per row.
    let mut rects: Vec<Rect> = Vec::new();
    for row in 0..grid.rows {
        let mut col = 0usize;
        while col < grid.cols {
            if !dirty[row * grid.cols + col] {
                col += 1;
                continue;
            }
            let start = col;
            while col < grid.cols && dirty[row * grid.cols + col] {
                col += 1;
            }
            rects.push(Rect {
                col: start,
                row,
                cols: col - start,
                rows: 1,
            });
        }
    }

    // 2. Join vertically adjacent rectangles that share a horizontal span. Repeat until a
    //    full pass merges nothing, so chains longer than two rows collapse too.
    loop {
        let mut merged_any = false;
        let mut next: Vec<Rect> = Vec::with_capacity(rects.len());
        let mut used = vec![false; rects.len()];
        for i in 0..rects.len() {
            if used[i] {
                continue;
            }
            let mut cur = rects[i];
            for j in i + 1..rects.len() {
                if used[j] {
                    continue;
                }
                let o = rects[j];
                if o.col == cur.col
                    && o.cols == cur.cols
                    && o.row == cur.row + cur.rows
                {
                    cur.rows += o.rows;
                    used[j] = true;
                    merged_any = true;
                }
            }
            next.push(cur);
        }
        rects = next;
        if !merged_any {
            break;
        }
    }

    // 3. Enforce the cap by merging the cheapest pair, where "cheapest" is the smallest area
    //    added by taking the union.
    while rects.len() > max_rects {
        let mut best: Option<(usize, usize, usize)> = None; // (i, j, added area)
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                let added = union_cost(rects[i], rects[j]);
                match best {
                    Some((_, _, cur)) if added >= cur => {}
                    _ => best = Some((i, j, added)),
                }
            }
        }
        let (i, j, _) = match best {
            Some(b) => b,
            None => break,
        };
        let union = union_rect(rects[i], rects[j]);
        rects[i] = union;
        rects.remove(j);
    }

    rects
        .into_iter()
        .map(|r| grid.span_rect(r.col, r.row, r.cols, r.rows))
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
}

fn area(r: Rect) -> usize {
    r.cols * r.rows
}

/// Extra blocks covered by merging two rectangles, i.e. union area minus both areas.
fn union_cost(a: Rect, b: Rect) -> usize {
    let u = union_rect(a, b);
    u.cols * u.rows - area(a) - area(b)
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let col = a.col.min(b.col);
    let row = a.row.min(b.row);
    let right = (a.col + a.cols).max(b.col + b.cols);
    let bottom = (a.row + a.rows).max(b.row + b.rows);
    Rect {
        col,
        row,
        cols: right - col,
        rows: bottom - row,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 32x16 screen of 4-byte pixels, constant color, except a rectangle painted differently.
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

    #[test]
    fn grid_covers_the_screen_in_full_blocks() {
        let g = BlockGrid::for_screen(32, 16, 8, 8);
        assert_eq!((g.cols, g.rows), (4, 2));
        assert_eq!(g.count(), 8);
        assert_eq!(g.rect(5), Some((8, 8, 8, 8)));
        assert_eq!(g.rect(8), None);
    }

    #[test]
    fn grid_clamps_a_partial_last_block() {
        // 30 is not a multiple of 8: the last column must be 6 wide, not 8.
        let g = BlockGrid::for_screen(30, 16, 8, 8);
        assert_eq!((g.cols, g.rows), (4, 2));
        assert_eq!(g.rect(3), Some((24, 0, 6, 8)));
        assert_eq!(g.rect(7), Some((24, 8, 6, 8)));
    }

    #[test]
    fn merged_rectangles_are_clamped_to_the_screen() {
        let g = BlockGrid::for_screen(30, 16, 8, 8);
        let rects = merge_blocks(&[3, 7], &g, 4);
        assert_eq!(rects, vec![(24, 0, 6, 16)]);
    }

    #[test]
    fn identical_frames_have_no_dirty_blocks() {
        let g = BlockGrid::for_screen(32, 16, 8, 8);
        let a = frame(32, 16, None);
        let b = frame(32, 16, None);
        assert_eq!(dirty_blocks(&a, &b, 32, 4, &g), Vec::<usize>::new());
    }

    #[test]
    fn a_painted_rectangle_marks_exactly_the_blocks_it_touches() {
        let g = BlockGrid::for_screen(32, 16, 8, 8);
        let a = frame(32, 16, None);
        // Paint 64x48 -> at 8x8 blocks, columns 0..1 and row 0: blocks 0 and 1.
        let b = frame(32, 16, Some((0, 0, 12, 8)));
        assert_eq!(dirty_blocks(&a, &b, 32, 4, &g), vec![0, 1]);
    }

    #[test]
    fn a_short_buffer_is_not_counted_as_dirty() {
        let g = BlockGrid::for_screen(32, 16, 8, 8);
        let full = frame(32, 16, None);
        let short = vec![0u8; 8];
        assert_eq!(dirty_blocks(&full, &short, 32, 4, &g), Vec::<usize>::new());
    }

    #[test]
    fn adjacent_blocks_merge_into_one_rectangle() {
        let g = BlockGrid::for_screen(32, 16, 8, 8);
        let rects = merge_blocks(&[0, 1], &g, 8);
        assert_eq!(rects, vec![(0, 0, 16, 8)]);
    }

    #[test]
    fn vertically_stacked_blocks_merge_into_one_rectangle() {
        let g = BlockGrid::for_screen(32, 16, 8, 8);
        // Blocks 0 and 4 are the same column in rows 0 and 1.
        let rects = merge_blocks(&[0, 4], &g, 8);
        assert_eq!(rects, vec![(0, 0, 8, 16)]);
    }

    #[test]
    fn far_apart_blocks_stay_separate_when_the_cap_allows_it() {
        let g = BlockGrid::for_screen(32, 16, 8, 8);
        // Block 0 (top-left) and block 7 (bottom-right).
        let rects = merge_blocks(&[0, 7], &g, 4);
        assert_eq!(rects.len(), 2);
        assert!(rects.contains(&(0, 0, 8, 8)));
        assert!(rects.contains(&(24, 8, 8, 8)));
    }

    #[test]
    fn the_cap_is_enforced_by_merging_the_cheapest_pair() {
        // 3x2 grid of 8x8 blocks. Dirty: two separate blocks in row 0 (blocks 0 and 2) and one
        // in row 1 (block 4). Three rectangles, cap 2. Merging the row-0 pair costs one extra
        // block; merging either with the row-1 block costs two. So the row-0 pair must merge.
        let g = BlockGrid::for_screen(24, 16, 8, 8);
        let rects = merge_blocks(&[0, 2, 4], &g, 2);
        assert_eq!(rects.len(), 2);
        assert!(rects.contains(&(0, 0, 24, 8)), "row-0 pair merged: {rects:?}");
        assert!(rects.contains(&(8, 8, 8, 8)), "row-1 block kept separate: {rects:?}");
    }

    #[test]
    fn vertical_runs_merge_before_the_cap_is_consulted() {
        // Blocks 0 and 3 are the same column in rows 0 and 1: step 2 joins them, so the cap is
        // already satisfied and nothing else gets merged.
        let g = BlockGrid::for_screen(24, 16, 8, 8);
        let rects = merge_blocks(&[0, 2, 3], &g, 2);
        assert_eq!(rects.len(), 2);
        assert!(rects.contains(&(0, 0, 8, 16)), "vertical run joined: {rects:?}");
        assert!(rects.contains(&(16, 0, 8, 8)), "row-0 block stays: {rects:?}");
    }

    #[test]
    fn cap_one_becomes_a_single_union() {
        let g = BlockGrid::for_screen(24, 16, 8, 8);
        let rects = merge_blocks(&[0, 2, 4], &g, 1);
        assert_eq!(rects, vec![(0, 0, 24, 16)]);
    }

    #[test]
    fn empty_input_and_zero_cap_yield_nothing() {
        let g = BlockGrid::for_screen(32, 16, 8, 8);
        assert!(merge_blocks(&[], &g, 4).is_empty());
        assert!(merge_blocks(&[0], &g, 0).is_empty());
    }

    #[test]
    fn out_of_range_indices_are_ignored() {
        let g = BlockGrid::for_screen(32, 16, 8, 8);
        let rects = merge_blocks(&[0, 999], &g, 4);
        assert_eq!(rects, vec![(0, 0, 8, 8)]);
    }
}
