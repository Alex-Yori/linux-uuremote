//! `uu-capture-probe` — measures the capture/gating pipeline on a live X11 desktop.
//!
//! This is the project's first working module: it proves the content-adaptive frame
//! source actually changes behavior, by reporting real numbers in both modes, and it
//! verifies that the pixel readback really returns screen content.
//!
//!   gating mode   : capture only when XDamage/XFixes say the screen changed
//!                   (official controlled-host behavior)
//!   baseline mode : constant-rate capture, i.e. what an `ffmpeg -f x11grab` host does
//!
//! Readback runs over MIT-SHM when the server allows it and falls back to `GetImage`;
//! the report shows which path was used and splits its cost into the server-side write
//! and the copy out of the shared mapping.
//!
//! `visual_change_fps` comes from a second, content-based check (`content.rs`): on a
//! compositing server XDamage reports every frame even when the picture is unchanged, so
//! damage alone cannot reproduce the official host's low static-desktop output rate.
//!
//! Usage:
//!   uu-capture-probe --display :1 --fps 60 --still-fps 3 --duration 20 \
//!     [--baseline] [--no-read] [--verify] [--dirty]

mod capture;
mod content;
mod cursor;
mod diff;
mod gating;
mod scan;

use std::error::Error;
use std::time::{Duration, Instant};

use capture::DirtyArea;
use capture::x11::{DamageLevel, GrabTiming, X11Screen};
use content::ContentChange;
use diff::{BlockGrid, dirty_blocks, merge_blocks};
use gating::{Action, FrameDropper, GateConfig};
use scan::{SampleGrid, changed_cells, splice_reference};

struct Args {
    display: Option<String>,
    moving_fps: u32,
    still_fps: u32,
    still_after: Duration,
    baseline: bool,
    read: bool,
    verify: bool,
    /// Grab only the dirty bounding box instead of the whole screen.
    dirty: bool,
    /// Drive the gate from the content check instead of XDamage reports.
    content_gate: bool,
    /// Incremental capture: cheap sample scan, then grab only the changed rectangles.
    incremental: bool,
    /// Full-frame read + exact CPU diff: measure the read+diff cost and the dirty share the
    /// encoder would actually receive.
    cpu_diff: bool,
    /// Fetch the cursor sprite and blend it into emitted frames (the root grab never contains
    /// the cursor itself).
    cursor: bool,
    /// Diff block size for `--cpu-diff`.
    diff_block: (usize, usize),
    /// Sample grid: cells across x down.
    sample_grid: (usize, usize),
    /// Pixels sampled per cell.
    sample_tile: (usize, usize),
    /// Full-frame resync interval, which bounds what a missed sample can cost.
    resync: Duration,
    /// Upper bound on rectangles grabbed per frame.
    max_dirty_rects: usize,
    damage_level: DamageLevel,
    duration: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        display: None,
        moving_fps: 60,
        still_fps: 3,
        still_after: Duration::from_millis(1000),
        baseline: false,
        read: true,
        verify: false,
        dirty: false,
        content_gate: false,
        incremental: false,
        cpu_diff: false,
        cursor: false,
        diff_block: (64, 64),
        sample_grid: (32, 18),
        sample_tile: (8, 8),
        resync: Duration::from_millis(1000),
        max_dirty_rects: 8,
        // Bounding-box reports are what a dirty-rect grab needs; the reference implementation
        // uses NonEmpty, so `--damage-level nonempty` compares against that.
        damage_level: DamageLevel::BoundingBox,
        duration: Duration::from_secs(20),
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1usize;
    while i < argv.len() {
        // Bind the flag before the value closure borrows `i`.
        let flag = argv[i].as_str();
        // Consume the next argument as this flag's value; missing values fall back below.
        let mut next = || -> String {
            i += 1;
            argv.get(i).cloned().unwrap_or_default()
        };
        match flag {
            "--display" => args.display = Some(next()),
            "--fps" => args.moving_fps = next().parse().unwrap_or(60),
            "--still-fps" => args.still_fps = next().parse().unwrap_or(3),
            "--still-after-ms" => {
                args.still_after = Duration::from_millis(next().parse().unwrap_or(1000))
            }
            "--duration" | "--secs" => {
                args.duration = Duration::from_secs(next().parse().unwrap_or(20))
            }
            "--baseline" | "--no-gate" => args.baseline = true,
            "--no-read" => args.read = false,
            "--verify" => args.verify = true,
            "--dirty" => args.dirty = true,
            "--content-gate" => args.content_gate = true,
            "--incremental" => args.incremental = true,
            "--cpu-diff" => args.cpu_diff = true,
            "--cursor" => args.cursor = true,
            "--diff-block" => args.diff_block = parse_size(&next(), args.diff_block),
            "--sample-grid" => args.sample_grid = parse_size(&next(), args.sample_grid),
            "--sample-tile" => args.sample_tile = parse_size(&next(), args.sample_tile),
            "--resync-ms" => args.resync = Duration::from_millis(next().parse().unwrap_or(1000)),
            "--max-dirty-rects" => args.max_dirty_rects = next().parse().unwrap_or(8),
            "--damage-level" => match next().as_str() {
                "nonempty" => args.damage_level = DamageLevel::NonEmpty,
                "bbox" => args.damage_level = DamageLevel::BoundingBox,
                "raw" => args.damage_level = DamageLevel::RawRectangles,
                other => return Err(format!("unknown --damage-level: {other}")),
            },
            "--help" | "-h" => {
                println!(
                    "usage: uu-capture-probe [--display :1] [--fps 60] [--still-fps 3] \
                     [--still-after-ms 1000] [--secs 20] [--baseline] [--no-read] [--verify] \
                     [--dirty] [--content-gate] [--incremental] [--cursor] [--cpu-diff] [--diff-block 64x64] \
                     [--sample-grid 32x18] \
                     [--sample-tile 8x8] [--resync-ms 1000] [--max-dirty-rects 8] \
                     [--damage-level bbox|nonempty|raw]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    Ok(args)
}

/// Parse `WxH` into a pair, keeping `default` when the text is malformed.
fn parse_size(text: &str, default: (usize, usize)) -> (usize, usize) {
    let Some((w, h)) = text.split_once('x') else {
        return default;
    };
    match (w.trim().parse::<usize>(), h.trim().parse::<usize>()) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
        _ => default,
    }
}

/// Union bounding box of the dirty reports seen in one round — the `dirty_bbox` the
/// official host logs. Reports are merged in 32-bit arithmetic because the union of two
/// far-apart rectangles can overflow `i16` on a large screen.
fn merge_bbox(current: Option<DirtyArea>, area: DirtyArea) -> DirtyArea {
    let Some(c) = current else { return area };
    let x = c.x.min(area.x) as i32;
    let y = c.y.min(area.y) as i32;
    let right = (c.x as i32 + c.width as i32).max(area.x as i32 + area.width as i32);
    let bottom = (c.y as i32 + c.height as i32).max(area.y as i32 + area.height as i32);
    DirtyArea {
        x: x as i16,
        y: y as i16,
        width: (right - x) as u16,
        height: (bottom - y) as u16,
    }
}

/// Incremental capture: a cheap sample scan decides which rectangles to grab.
///
/// A full grab makes the X server write the whole screen (14.7 MB / ~6 ms here). Instead this
/// samples a few hundred small tiles — a fraction of that — compares them against the frame we
/// already hold, and grabs only the merged rectangles around cells that differ, then splices
/// them back into the reference.
///
/// A periodic full resync does two jobs: it refreshes the reference so drift cannot
/// accumulate, and its block diff against the spliced reference measures what the sparse
/// samples missed. That missed share is the number to watch when tuning `--sample-grid`: it is
/// the cost of sampling cheaply, expressed in stale pixels per second.
fn run_incremental(args: &Args, screen: &mut X11Screen, cfg: GateConfig) -> Result<(), Box<dyn Error>> {
    let (w, h) = (screen.width() as usize, screen.height() as usize);
    let bpp = screen.bytes_per_pixel();
    let full_px = (w * h) as u64;

    let grid = SampleGrid::new(w, h, args.sample_grid.0, args.sample_grid.1, args.sample_tile.0, args.sample_tile.1);
    let sample_rects: Vec<DirtyArea> = (0..grid.count())
        .filter_map(|i| grid.sample_rect(i))
        .map(|(x, y, rw, rh)| DirtyArea {
            x: x as i16,
            y: y as i16,
            width: rw as u16,
            height: rh as u16,
        })
        .collect();
    let sample_px: u64 = sample_rects.iter().map(|r| r.pixels()).sum();
    // The resync diff uses the same cell size, so misses are counted in cells too.
    let cell_grid = BlockGrid::for_screen(w, h, grid.cells.block_w, grid.cells.block_h);

    println!(
        "incremental cells={}x{} sample_tile={}x{} scan={} px ({:.2}% of a frame) resync_ms={} \
         max_rects={}",
        grid.cells.cols,
        grid.cells.rows,
        grid.sample_w,
        grid.sample_h,
        sample_px,
        100.0 * sample_px as f64 / full_px as f64,
        args.resync.as_millis(),
        args.max_dirty_rects,
    );

    // The reference frame is what samples and tiles are compared against; the first frame has
    // to be complete, so it is a full grab.
    let (mut reference, first) = screen.grab_rect(None)?;
    println!(
        "first full frame: {} px in {:.1} ms ({})",
        reference.len() / bpp,
        first.total_ms(),
        if args.read { "read" } else { "skipped" },
    );

    let mut dropper = FrameDropper::new(cfg);
    let start = Instant::now();
    let mut window_start = start;
    let mut last_resync = start;

    let mut total_emits: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_dirty_px: u64 = 0;
    let mut total_resync: u64 = 0;
    let mut total_missed_px: u64 = 0;
    let mut total_scans: u64 = 0;
    let mut total_scan_px: u64 = 0;
    let mut total_keep: u64 = 0;
    let mut w_scans: u64 = 0;
    let mut w_scan_ms: f64 = 0.0;
    let mut w_scan_px: u64 = 0;
    let mut w_emits: u64 = 0;
    let mut w_rects: u64 = 0;
    let mut w_dirty_px: u64 = 0;
    let mut w_tiles_ms: f64 = 0.0;
    let mut w_bytes: u64 = 0;
    let mut w_keep: u64 = 0;
    let mut w_damage: u64 = 0;
    let mut w_cursor: u64 = 0;
    let mut w_resync: u64 = 0;
    let mut w_resync_ms: f64 = 0.0;
    let mut w_missed_px: u64 = 0;
    let mut scan_via_shm = false;

    while start.elapsed() < args.duration {
        let now = Instant::now();

        // Damage and cursor reports are still drained: damage is a hint we no longer trust for
        // geometry, and cursor motion has to keep the gate in its moving regime even though our
        // grabbed pixels do not contain the cursor.
        for _ in screen.poll_events()? {
            w_damage += 1;
        }
        let cursor_moved = screen.poll_cursor()?.is_some();
        if cursor_moved {
            w_cursor += 1;
        }

        let mut changed: Vec<usize> = Vec::new();
        if args.read {
            let scan = screen.grab_tiles(&sample_rects)?;
            scan_via_shm = scan.via_shm;
            w_scans += 1;
            w_scan_ms += scan.ms;
            w_scan_px += scan.bytes as u64 / bpp as u64;
            changed = changed_cells(&reference, &scan.data, &grid, w, bpp);
        }
        if !changed.is_empty() || cursor_moved || args.baseline {
            dropper.note_change(now);
        }

        match dropper.decide(now) {
            Action::Capture => {
                let rects = merge_blocks(&changed, &grid.cells, args.max_dirty_rects);
                if rects.is_empty() {
                    // Nothing visible changed; a cursor-only motion produces no pixel work here
                    // because the cursor is drawn by the server, not part of the grabbed image.
                    continue;
                }
                let areas: Vec<DirtyArea> = rects
                    .iter()
                    .map(|&(x, y, rw, rh)| DirtyArea {
                        x: x as i16,
                        y: y as i16,
                        width: rw as u16,
                        height: rh as u16,
                    })
                    .collect();
                let grab = screen.grab_tiles(&areas)?;
                let written = splice_reference(&mut reference, w, bpp, &rects, &grab.data);
                if written != grab.bytes {
                    return Err(format!(
                        "spliced {written} of {} tile bytes into the reference",
                        grab.bytes
                    )
                    .into());
                }
                w_emits += 1;
                w_rects += rects.len() as u64;
                w_dirty_px += rects.iter().map(|(_, _, rw, rh)| (rw * rh) as u64).sum::<u64>();
                w_tiles_ms += grab.ms;
                w_bytes += grab.bytes as u64;
            }
            Action::Keepalive => w_keep += 1,
            Action::Idle => {}
        }

        // Periodic full frame: refreshes the reference and measures what the samples missed.
        if args.read && now.duration_since(last_resync) >= args.resync {
            let (frame, timing) = screen.grab_rect(None)?;
            let missed = dirty_blocks(&reference, &frame, w, bpp, &cell_grid);
            w_resync += 1;
            w_resync_ms += timing.total_ms();
            w_missed_px += missed
                .iter()
                .filter_map(|i| cell_grid.rect(*i))
                .map(|(_, _, rw, rh)| (rw * rh) as u64)
                .sum::<u64>();
            w_bytes += frame.len() as u64;
            reference = frame;
            last_resync = now;
        }

        if now.duration_since(window_start) >= Duration::from_secs(2) {
            let dt = now.duration_since(window_start).as_secs_f64().max(1e-6);
            let scans = w_scans.max(1) as f64;
            let emits = w_emits.max(1) as f64;
            let resyncs = w_resync.max(1) as f64;
            println!(
                "t={:5.1}s state={:<7} scan_shm={} scan_fps={:6.1} scan_ms={:6.2} scan_share={:6.4} \
                 emit_fps={:6.1} rects_per_frame={:5.2} dirty_share={:6.4} tiles_ms={:6.2} \
                 read_mb_s={:7.1} resync_fps={:5.2} resync_ms={:6.1} missed_share={:6.4} \
                 damage_ev_s={:6.1} cursor_ev_s={:6.1} keepalive_fps={:6.1}",
                now.duration_since(start).as_secs_f64(),
                if dropper.is_moving() { "moving" } else { "still" },
                scan_via_shm,
                w_scans as f64 / dt,
                w_scan_ms / scans,
                w_scan_px as f64 / scans / full_px as f64,
                w_emits as f64 / dt,
                w_rects as f64 / emits,
                w_dirty_px as f64 / emits / full_px as f64,
                w_tiles_ms / emits,
                w_bytes as f64 / dt / (1024.0 * 1024.0),
                w_resync as f64 / dt,
                w_resync_ms / resyncs,
                w_missed_px as f64 / resyncs / full_px as f64,
                w_damage as f64 / dt,
                w_cursor as f64 / dt,
                w_keep as f64 / dt,
            );
            total_emits += w_emits;
            total_bytes += w_bytes;
            total_dirty_px += w_dirty_px;
            total_resync += w_resync;
            total_missed_px += w_missed_px;
            total_scans += w_scans;
            total_scan_px += w_scan_px;
            total_keep += w_keep;
            for c in [
                &mut w_scans,
                &mut w_scan_px,
                &mut w_emits,
                &mut w_rects,
                &mut w_dirty_px,
                &mut w_bytes,
                &mut w_keep,
                &mut w_damage,
                &mut w_cursor,
                &mut w_resync,
                &mut w_missed_px,
            ] {
                *c = 0;
            }
            w_scan_ms = 0.0;
            w_tiles_ms = 0.0;
            w_resync_ms = 0.0;
            window_start = now;
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    let secs = start.elapsed().as_secs_f64().max(1e-6);
    println!(
        "SUMMARY mode=incremental emits={} emit_fps={:.2} dirty_share={:.4} scans={} \
         scan_fps={:.2} scan_share={:.4} resyncs={} resync_fps={:.2} missed_share={:.4} \
         keepalives={} read_mb_s={:.2} (full-screen capture measured ~840 MB/s at 60 fps)",
        total_emits,
        total_emits as f64 / secs,
        total_dirty_px as f64 / total_emits.max(1) as f64 / full_px as f64,
        total_scans,
        total_scans as f64 / secs,
        total_scan_px as f64 / total_scans.max(1) as f64 / full_px as f64,
        total_resync,
        total_resync as f64 / secs,
        total_missed_px as f64 / total_resync.max(1) as f64 / full_px as f64,
        total_keep,
        total_bytes as f64 / secs / (1024.0 * 1024.0),
    );
    Ok(())
}

/// Full-frame read plus an exact CPU diff, to decide where the work actually belongs.
///
/// A 2560x1440 SHM grab costs ~7 ms and a whole-frame byte diff costs a few more, so reading
/// the entire screen fits a 60 fps budget without any hunting for dirty areas. What dirty
/// rectangles buy is not the read but the *encoder* input, so this mode reports the pixel share
/// the encoder would receive. If that share is small, dirty rects are worth their complexity;
/// if it is large, they are not.
fn run_cpu_diff(args: &Args, screen: &mut X11Screen) -> Result<(), Box<dyn Error>> {
    let (w, h) = (screen.width() as usize, screen.height() as usize);
    let bpp = screen.bytes_per_pixel();
    let full_px = (w * h) as u64;
    let grid = BlockGrid::for_screen(w, h, args.diff_block.0, args.diff_block.1);
    println!(
        "cpu-diff blocks={}x{} cells={}x{}={} cap_rects={}",
        args.diff_block.0,
        args.diff_block.1,
        grid.cols,
        grid.rows,
        grid.count(),
        args.max_dirty_rects,
    );

    let (mut reference, first) = screen.grab_rect(None)?;
    println!(
        "first frame {} px in {:.1} ms ({})",
        reference.len() / bpp,
        first.total_ms(),
        if args.read { "read" } else { "skipped" },
    );

    // Cursor state: the sprite only changes when the server reports a new cursor, so it is
    // fetched lazily and dropped on any cursor-notify; position comes from QueryPointer.
    let mut sprite: Option<cursor::Sprite> = None;
    let mut w_cursor_fetch: u64 = 0;
    let mut w_cursor_fetch_ms: f64 = 0.0;
    let mut w_cursor_comp_ms: f64 = 0.0;
    let mut w_cursor_px: u64 = 0;
    let mut t_cursor_px: u64 = 0;

    let start = Instant::now();
    let mut window_start = start;

    let mut w_grabs: u64 = 0;
    let mut w_grab_ms: f64 = 0.0;
    let mut w_diff_ms: f64 = 0.0;
    let mut w_rects: u64 = 0;
    let mut w_dirty_px: u64 = 0;
    let mut w_bytes: u64 = 0;
    let mut w_damage: u64 = 0;
    let mut t_grabs: u64 = 0;
    let mut t_rects: u64 = 0;
    let mut t_dirty_px: u64 = 0;
    let mut t_bytes: u64 = 0;

    while start.elapsed() < args.duration {
        for _ in screen.poll_events()? {
            w_damage += 1;
            if args.cursor {
                // `poll_events` only reports a cursor-notify after deduping `cursor_serial`,
                // so this invalidates the sprite only when the server really swapped it. On
                // this mutter desktop the serial still churns ~60/s (it rebuilds the cursor
                // object per repaint), which costs the measured ~0.05 ms per frame — cheap
                // enough to accept rather than guess a slower polling cadence.
                sprite = None;
            }
        }
        let mut hotspot: Option<(i32, i32)> = None;
        if args.cursor {
            // Motion marks the gate dirty; the sprite still has to ride on every frame, so
            // the position is kept even when nothing moved. `poll_cursor` stores the position
            // before returning, so polling then reading covers both branches.
            screen.poll_cursor()?;
            hotspot = screen.cursor_position();
            if sprite.is_none() {
            let t0 = Instant::now();
            sprite = Some(screen.fetch_cursor()?);
            w_cursor_fetch += 1;
            w_cursor_fetch_ms += t0.elapsed().as_secs_f64() * 1000.0;
            }
        }

        let (frame, timing) = screen.grab_rect(None)?;
        let t0 = Instant::now();
        let blocks = dirty_blocks(&reference, &frame, w, bpp, &grid);
        let rects = merge_blocks(&blocks, &grid, args.max_dirty_rects);
        let diff_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // The cursor lives on top of everything: while it shows, its sprite rectangle rides
        // on top of the content-diff rectangles and is re-encoded each frame it is visible
        // (cheap: a sprite is a few hundred pixels, not a screen). It is blended into the
        // emitted copy only — the diff reference stays cursor-free, otherwise cursor pixels
        // would register as content dirty forever.
        if let (Some(s), Some(pos)) = (sprite.as_ref(), hotspot) {
            let mut out = frame.clone();
            let t0 = Instant::now();
            let painted = screen.composite_cursor(&mut out, s, pos);
            w_cursor_comp_ms += t0.elapsed().as_secs_f64() * 1000.0;
            let px = painted.map(|(_, _, w, h)| (w * h) as u64).unwrap_or(0);
            w_cursor_px += px;
            w_bytes += px * bpp as u64;
        }

        w_bytes += frame.len() as u64;
        reference = frame;

        w_grabs += 1;
        w_grab_ms += timing.total_ms();
        w_diff_ms += diff_ms;
        w_rects += rects.len() as u64;
        w_dirty_px += rects.iter().map(|(_, _, rw, rh)| (rw * rh) as u64).sum::<u64>();

        if window_start.elapsed() >= Duration::from_secs(2) {
            let dt = window_start.elapsed().as_secs_f64().max(1e-6);
            let grabs = w_grabs.max(1) as f64;
            println!(
                "t={:5.1}s grab_fps={:6.1} grab_ms={:6.2} diff_ms={:6.2} loop_ms={:6.2} \
                 rects_per_frame={:5.2} dirty_share={:6.4} read_mb_s={:7.1} encode_mb_s={:7.1} \
                 cursor_fetch_fps={:5.2} cursor_fetch_ms={:6.3} cursor_comp_ms={:6.3} \
                 cursor_share={:6.4} damage_ev_s={:6.1}",
                start.elapsed().as_secs_f64(),
                w_grabs as f64 / dt,
                w_grab_ms / grabs,
                w_diff_ms / grabs,
                (w_grab_ms + w_diff_ms) / grabs,
                w_rects as f64 / grabs,
                w_dirty_px as f64 / grabs / full_px as f64,
                w_bytes as f64 / dt / (1024.0 * 1024.0),
                (w_dirty_px + w_cursor_px) as f64 / dt * bpp as f64 / (1024.0 * 1024.0),
                w_cursor_fetch as f64 / dt,
                w_cursor_fetch_ms / w_cursor_fetch.max(1) as f64,
                w_cursor_comp_ms / grabs,
                w_cursor_px as f64 / grabs / full_px as f64,
                w_damage as f64 / dt,
            );
            t_grabs += w_grabs;
            t_rects += w_rects;
            t_dirty_px += w_dirty_px;
            t_bytes += w_bytes;
            t_cursor_px += w_cursor_px;
            w_grabs = 0;
            w_rects = 0;
            w_dirty_px = 0;
            w_bytes = 0;
            w_damage = 0;
            w_cursor_px = 0;
            w_cursor_fetch = 0;
            w_cursor_fetch_ms = 0.0;
            w_cursor_comp_ms = 0.0;
            w_grab_ms = 0.0;
            w_diff_ms = 0.0;
            window_start = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    let secs = start.elapsed().as_secs_f64().max(1e-6);
    let grabs = t_grabs.max(1) as f64;
    println!(
        "SUMMARY mode=cpu-diff grabs={} grab_fps={:.2} dirty_share={:.4} rects_total={} \
         rects_per_frame={:.2} encode_mb_s={:.2} cursor_mb_s={:.2} read_mb_s={:.2} \
         (full-frame encoder input would be {:.0} MB/s)",
        t_grabs,
        t_grabs as f64 / secs,
        t_dirty_px as f64 / grabs / full_px as f64,
        t_rects,
        t_rects as f64 / grabs,
        (t_dirty_px + t_cursor_px) as f64 / secs * bpp as f64 / (1024.0 * 1024.0),
        t_cursor_px as f64 / secs * bpp as f64 / (1024.0 * 1024.0),
        t_bytes as f64 / secs / (1024.0 * 1024.0),
        full_px as f64 * bpp as f64 / (1024.0 * 1024.0) * (t_grabs as f64 / secs),
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    let mut screen = X11Screen::open(args.display.as_deref(), args.damage_level)?;

    let (readback, reason) = screen.readback();
    let cfg = GateConfig {
        moving_fps: args.moving_fps,
        still_fps: args.still_fps,
        still_after: args.still_after,
    };
    let mut dropper = FrameDropper::new(cfg);

    // A coarse lattice (576 samples over a 2560x1440 screen) used to tell "the drawable was
    // modified" apart from "the picture changed".
    let mut content = ContentChange::new(screen.width() as usize, screen.height() as usize, 32, 18);
    let (gw, gh) = content.grid();

    let full_px = screen.width() as u64 * screen.height() as u64;
    println!(
        "display {}x{} frame={} MiB mode={} gate={} readback={} grab={} damage={} content_grid={}x{} \
         moving_fps={} still_fps={} still_after_ms={} read={}",
        screen.width(),
        screen.height(),
        screen.frame_bytes() / (1024 * 1024),
        if args.baseline { "baseline" } else { "gating" },
        if args.content_gate { "content" } else { "damage" },
        readback,
        if args.dirty { "dirty-rect" } else { "full-screen" },
        screen.damage_level().label(),
        gw,
        gh,
        cfg.moving_fps,
        cfg.still_fps,
        cfg.still_after.as_millis(),
        args.read
    );
    if let Some(reason) = reason {
        println!("readback fallback reason: {reason}");
    }

    // Prove the readback really returns screen content before trusting any timing numbers.
    let mut verify_ok = true;
    if args.verify {
        let v = screen.verify_readback()?;
        // The gate is about the painted rectangle, not whole-frame equality: a live desktop
        // keeps redrawing, so `frame_identical` and `tile_matches_full` are reported but
        // never gate — they can legitimately differ when the desktop repaints in between.
        verify_ok = v.painted_shm > 0.9
            && v.painted_tile > 0.9
            && v.rect_identical > 0.9
            && v.painted_changed > 0.5
            && v.tiles_batch_painted
            // Cursor composition: when the server reports a sprite, blending it onto a black
            // canvas must change at least one pixel and never more than its alpha mask allows.
            && (v.cursor_sprite == (0, 0)
                || (v.cursor_composite_ratio > 0.0
                    && v.cursor_composite_ratio <= v.cursor_expected_ratio + 0.05));
        println!(
            "VERIFY bytes={} painted_getimage={:.3} painted_shm={:.3} painted_tile={:.3} \
             tile_matches_full={} rect_identical={:.3} painted_changed={:.3} frame_identical={} \
             batch_painted={} batch_matches_full={} batch_ms={:.2} batch_bytes={} \
             cursor={}x{} serial={} fetch_ms={:.3} comp={} px comp_ms={:.3} \
             comp_ratio={:.3}/{:.3} rect=({},{},{},{}) full_ms={:.1} tile_ms={:.1} tile_bytes={} -> {}",
            v.bytes,
            v.painted_getimage,
            v.painted_shm,
            v.painted_tile,
            v.tile_matches_full,
            v.rect_identical,
            v.painted_changed,
            v.frame_identical,
            v.tiles_batch_painted,
            v.tiles_batch_matches,
            v.tiles_batch_ms,
            v.tiles_batch_bytes,
            v.cursor_sprite.0,
            v.cursor_sprite.1,
            v.cursor_serial,
            v.cursor_ms,
            v.cursor_composite_px,
            v.cursor_composite_ms,
            v.cursor_composite_ratio,
            v.cursor_expected_ratio,
            v.rect.0,
            v.rect.1,
            v.rect.2,
            v.rect.3,
            v.shm_ms,
            v.tile_ms,
            v.tile_bytes,
            if verify_ok { "PASS" } else { "FAIL" },
        );
    }

    if args.cpu_diff && args.read {
        return run_cpu_diff(&args, &mut screen);
    }

    // Incremental mode replaces the capture loop entirely: it needs its own reference frame,
    // sample scan and resync accounting, so it is kept separate rather than bolted onto this
    // loop's branches. It runs after verification so the batch path is proven first.
    if args.incremental && args.read {
        return run_incremental(&args, &mut screen, cfg);
    }

    let start = Instant::now();
    let mut window_start = start;
    let mut total_captures: u64 = 0;
    let mut total_keep: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_grabbed_px: u64 = 0;
    let mut w_captures: u64 = 0;
    let mut w_keep: u64 = 0;
    let mut w_damage: u64 = 0;
    let mut w_cursor: u64 = 0;
    let mut w_dirty_px: u64 = 0;
    let mut w_total_ms: f64 = 0.0;
    let mut w_server_ms: f64 = 0.0;
    let mut w_copy_ms: f64 = 0.0;
    let mut w_shm: u64 = 0;
    let mut w_image: u64 = 0;
    let mut w_bytes: u64 = 0;
    // Pixels actually grabbed, so `--dirty` can report how much of the screen it skipped.
    let mut w_grabbed_px: u64 = 0;
    // Content-based change counters: frames that looked different from the previous one.
    let mut w_visual_changed: u64 = 0;
    let mut w_visual_same: u64 = 0;
    let mut w_diff_share_sum: f64 = 0.0;
    let mut total_visual_changed: u64 = 0;
    let mut total_visual_same: u64 = 0;
    // Largest dirty bounding box seen in this window, so a low average ratio can be told
    // apart from "the damage reports never covered anything".
    let mut w_max_bbox_px: u64 = 0;
    let mut bbox_now: Option<DirtyArea>;
    // What the last sampled frame looked like, used by `--content-gate`.
    let mut last_content_changed = false;
    let mut path_now = readback;

    while start.elapsed() < args.duration {
        let now = Instant::now();

        // Damage reports are always drained, even in baseline mode: the bounding box is what
        // `--dirty` grabs, and the event counters make both modes comparable.
        let mut bbox: Option<DirtyArea> = None;
        let mut damage_changed = false;
        let mut cursor_moved = false;
        let mut dirty_px = 0u64;
        for area in screen.poll_events()? {
            damage_changed = true;
            dirty_px += area.pixels();
            bbox = Some(merge_bbox(bbox, area));
            w_damage += 1;
        }
        if let Some(area) = screen.poll_cursor()? {
            cursor_moved = true;
            dirty_px += area.pixels();
            bbox = Some(merge_bbox(bbox, area));
            w_cursor += 1;
        }
        w_dirty_px += dirty_px;
        bbox_now = bbox;
        if let Some(b) = bbox {
            w_max_bbox_px = w_max_bbox_px.max(b.pixels());
        }

        // Two different notions of "changed", and this is the point of the content check:
        // on a compositing server damage fires every frame whether or not the picture moved,
        // so `--content-gate` drives the gate from what the pixels actually look like instead.
        // Cursor movement always counts, because XDamage never reports the cursor.
        let gate_changed = if args.content_gate {
            cursor_moved || last_content_changed
        } else {
            damage_changed || cursor_moved
        };
        // Baseline mode pretends the screen changed every tick, which is exactly what a
        // constant-rate host does: it keeps the gate in its moving regime permanently.
        if gate_changed || args.baseline {
            dropper.note_change(now);
        }

        match dropper.decide(now) {
            Action::Capture => {
                if args.read {
                    let rect = if args.dirty { bbox } else { None };
                    let (data, timing) = screen.grab_rect(rect)?;
                    w_bytes += data.len() as u64;
                    w_grabbed_px += (data.len() / screen.bytes_per_pixel()) as u64;
                    w_total_ms += timing.total_ms();
                    // A full-screen grab can be diffed against the previous one; a tile has
                    // no stable reference, so it is left out of the content counters.
                    if data.len() == screen.frame_bytes() {
                        let diff =
                            content.check(&data, screen.width() as usize, screen.bytes_per_pixel());
                        last_content_changed = diff.changed_pixels > 0;
                        w_diff_share_sum += diff.share();
                        if diff.changed_pixels > 0 {
                            w_visual_changed += 1;
                        } else {
                            w_visual_same += 1;
                        }
                        // A readback path switch changes where pixels come from; the next
                        // frame has no meaningful predecessor, so drop the reference sample.
                        if timing.label() != path_now {
                            content.reset();
                        }
                    }
                    match timing {
                        GrabTiming::Shm { server_ms, copy_ms } => {
                            w_server_ms += server_ms;
                            w_copy_ms += copy_ms;
                            w_shm += 1;
                        }
                        GrabTiming::GetImage { .. } => w_image += 1,
                    }
                    path_now = timing.label();
                }
                w_captures += 1;
            }
            Action::Keepalive => {
                w_keep += 1;
                // With the content gate on, keepalive ticks are also sampling ticks: without
                // this, a screen that went still could never be noticed changing again.
                if args.content_gate && args.read {
                    let (data, timing) = screen.grab_rect(None)?;
                    w_bytes += data.len() as u64;
                    w_grabbed_px += (data.len() / screen.bytes_per_pixel()) as u64;
                    w_total_ms += timing.total_ms();
                    if data.len() == screen.frame_bytes() {
                        let diff =
                            content.check(&data, screen.width() as usize, screen.bytes_per_pixel());
                        last_content_changed = diff.changed_pixels > 0;
                        w_diff_share_sum += diff.share();
                        if diff.changed_pixels > 0 {
                            w_visual_changed += 1;
                        } else {
                            w_visual_same += 1;
                        }
                    }
                    match timing {
                        GrabTiming::Shm { server_ms, copy_ms } => {
                            w_server_ms += server_ms;
                            w_copy_ms += copy_ms;
                            w_shm += 1;
                        }
                        GrabTiming::GetImage { .. } => w_image += 1,
                    }
                    path_now = timing.label();
                    if last_content_changed {
                        dropper.note_change(now);
                    }
                }
            }
            Action::Idle => {}
        }

        if now.duration_since(window_start) >= Duration::from_secs(2) {
            let dt = now.duration_since(window_start).as_secs_f64().max(1e-6);
            let dirty_ratio = (w_dirty_px as f64 / full_px as f64).min(9999.0);
            let grabs = (w_shm + w_image).max(1) as f64;
            let bbox_txt = bbox_now
                .map(|b| format!("{}x{}", b.width, b.height))
                .unwrap_or_else(|| "none".into());
            // Mean share of the sampled lattice that changed, over the diffed frames.
            let diffed = w_visual_changed + w_visual_same;
            let diff_share = if diffed == 0 {
                0.0
            } else {
                w_diff_share_sum / diffed as f64
            };
            println!(
                "t={:5.1}s state={:<7} capture_fps={:6.1} keepalive_fps={:6.1} \
                 damage_ev_s={:6.1} cursor_ev_s={:6.1} visual_change_fps={:6.1} diff_share={:6.4} \
                 dirty_ratio={:6.3} grab_area_ratio={:6.3} bbox={:<11} bbox_max_share={:6.3} \
                 read_mb_s={:7.1} path={:<8} avg_total_ms={:6.1} avg_server_ms={:6.1} \
                 avg_copy_ms={:6.1}",
                now.duration_since(start).as_secs_f64(),
                if dropper.is_moving() { "moving" } else { "still" },
                w_captures as f64 / dt,
                w_keep as f64 / dt,
                w_damage as f64 / dt,
                w_cursor as f64 / dt,
                w_visual_changed as f64 / dt,
                diff_share,
                dirty_ratio,
                (w_grabbed_px as f64 / full_px as f64 / grabs).min(9999.0),
                bbox_txt,
                (w_max_bbox_px as f64 / full_px as f64).min(1.0),
                w_bytes as f64 / dt / (1024.0 * 1024.0),
                path_now,
                w_total_ms / grabs,
                w_server_ms / grabs,
                w_copy_ms / grabs,
            );
            total_captures += w_captures;
            total_keep += w_keep;
            total_bytes += w_bytes;
            total_grabbed_px += w_grabbed_px;
            total_visual_changed += w_visual_changed;
            total_visual_same += w_visual_same;
            w_visual_changed = 0;
            w_visual_same = 0;
            w_diff_share_sum = 0.0;
            w_captures = 0;
            w_keep = 0;
            w_damage = 0;
            w_cursor = 0;
            w_dirty_px = 0;
            w_total_ms = 0.0;
            w_server_ms = 0.0;
            w_copy_ms = 0.0;
            w_shm = 0;
            w_image = 0;
            w_bytes = 0;
            w_grabbed_px = 0;
            w_max_bbox_px = 0;
            window_start = now;
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    let secs = start.elapsed().as_secs_f64().max(1e-6);
    println!(
        "SUMMARY mode={} gate={} readback={} grab={} damage={} captures={} capture_fps={:.2} \
         visual_change_fps={:.2} identical_share={:.1}% keepalives={} keepalive_fps={:.2} \
         read_mb_s={:.2} grab_area_share={:.3} still_share={:.1}%",
        if args.baseline { "baseline" } else { "gating" },
        if args.content_gate { "content" } else { "damage" },
        path_now,
        if args.dirty { "dirty-rect" } else { "full-screen" },
        screen.damage_level().label(),
        total_captures,
        total_captures as f64 / secs,
        total_visual_changed as f64 / secs,
        100.0 * total_visual_same as f64 / (total_visual_changed + total_visual_same).max(1) as f64,
        total_keep,
        total_keep as f64 / secs,
        total_bytes as f64 / secs / (1024.0 * 1024.0),
        if total_captures == 0 {
            0.0
        } else {
            (total_grabbed_px as f64 / full_px as f64 / total_captures as f64).min(9999.0)
        },
        100.0 * total_keep as f64 / (total_captures + total_keep).max(1) as f64,
    );

    if args.verify && !verify_ok {
        std::process::exit(1);
    }
    Ok(())
}
