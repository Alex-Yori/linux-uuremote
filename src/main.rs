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
mod gating;

use std::error::Error;
use std::time::{Duration, Instant};

use capture::DirtyArea;
use capture::x11::{DamageLevel, GrabTiming, X11Screen};
use content::ContentChange;
use gating::{Action, FrameDropper, GateConfig};

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
                     [--dirty] [--content-gate] [--damage-level bbox|nonempty|raw]"
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
            && v.painted_changed > 0.5;
        println!(
            "VERIFY bytes={} painted_getimage={:.3} painted_shm={:.3} painted_tile={:.3} \
             tile_matches_full={} rect_identical={:.3} painted_changed={:.3} frame_identical={} \
             rect=({},{},{},{}) full_ms={:.1} tile_ms={:.1} tile_bytes={} -> {}",
            v.bytes,
            v.painted_getimage,
            v.painted_shm,
            v.painted_tile,
            v.tile_matches_full,
            v.rect_identical,
            v.painted_changed,
            v.frame_identical,
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
