//! `uu-capture-probe` — measures the capture/gating pipeline on a live X11 desktop.
//!
//! This is the project's first working module: it proves the content-adaptive frame
//! source actually changes behavior, by reporting real numbers in both modes.
//!
//!   gating mode   : capture only when XDamage/XFixes say the screen changed
//!                   (official controlled-host behavior)
//!   baseline mode : constant-rate capture, i.e. what an `ffmpeg -f x11grab` host does
//!
//! Usage:
//!   uu-capture-probe --display :1 --fps 60 --still-fps 3 --duration 20 [--baseline] [--no-read]

mod capture;
mod gating;

use std::error::Error;
use std::time::{Duration, Instant};

use capture::x11::X11Screen;
use gating::{Action, FrameDropper, GateConfig};

struct Args {
    display: Option<String>,
    moving_fps: u32,
    still_fps: u32,
    still_after: Duration,
    baseline: bool,
    read: bool,
    duration: Duration,
}

fn parse_args() -> Args {
    let mut args = Args {
        display: None,
        moving_fps: 60,
        still_fps: 3,
        still_after: Duration::from_millis(1000),
        baseline: false,
        read: true,
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
            "--duration" => args.duration = Duration::from_secs(next().parse().unwrap_or(20)),
            "--baseline" => args.baseline = true,
            "--no-read" => args.read = false,
            "--help" | "-h" => {
                println!(
                    "usage: uu-capture-probe [--display :1] [--fps 60] [--still-fps 3] \
                     [--still-after-ms 1000] [--duration 20] [--baseline] [--no-read]"
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
    args
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args();
    let mut screen = X11Screen::open(args.display.as_deref())?;

    let cfg = GateConfig {
        moving_fps: args.moving_fps,
        still_fps: args.still_fps,
        still_after: args.still_after,
    };
    let mut dropper = FrameDropper::new(cfg);

    let full_px = screen.width() as u64 * screen.height() as u64;
    println!(
        "display {}x{} frame={} MiB mode={} moving_fps={} still_fps={} still_after_ms={} read={}",
        screen.width(),
        screen.height(),
        screen.frame_bytes() / (1024 * 1024),
        if args.baseline { "baseline" } else { "gating" },
        cfg.moving_fps,
        cfg.still_fps,
        cfg.still_after.as_millis(),
        args.read
    );

    let start = Instant::now();
    let mut window_start = start;
    let mut total_captures: u64 = 0;
    let mut total_keep: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut w_captures: u64 = 0;
    let mut w_keep: u64 = 0;
    let mut w_damage: u64 = 0;
    let mut w_cursor: u64 = 0;
    let mut w_dirty_px: u64 = 0;
    let mut w_grab_ms: f64 = 0.0;
    let mut w_bytes: u64 = 0;

    while start.elapsed() < args.duration {
        let now = Instant::now();

        let mut dirty = false;
        if args.baseline {
            dirty = true;
        } else {
            let mut dirty_px = 0u64;
            for area in screen.poll_events()? {
                dirty = true;
                dirty_px += area.pixels();
                w_damage += 1;
            }
            if !dirty {
                if let Some(area) = screen.poll_cursor()? {
                    dirty = true;
                    dirty_px += area.pixels();
                    w_cursor += 1;
                }
            }
            w_dirty_px += dirty_px;
        }
        if dirty {
            dropper.note_change(now);
        }

        match dropper.decide(now) {
            Action::Capture => {
                if args.read {
                    let (data, ms) = screen.grab()?;
                    w_bytes += data.len() as u64;
                    w_grab_ms += ms;
                }
                w_captures += 1;
            }
            Action::Keepalive => w_keep += 1,
            Action::Idle => {}
        }

        if now.duration_since(window_start) >= Duration::from_secs(2) {
            let dt = now.duration_since(window_start).as_secs_f64().max(1e-6);
            let dirty_ratio = (w_dirty_px as f64 / full_px as f64).min(9999.0);
            println!(
                "t={:5.1}s state={:<7} capture_fps={:6.1} keepalive_fps={:6.1} \
                 damage_ev_s={:6.1} cursor_ev_s={:6.1} dirty_ratio={:6.3} \
                 read_mb_s={:7.1} avg_grab_ms={:6.1}",
                now.duration_since(start).as_secs_f64(),
                if dropper.is_moving() { "moving" } else { "still" },
                w_captures as f64 / dt,
                w_keep as f64 / dt,
                w_damage as f64 / dt,
                w_cursor as f64 / dt,
                dirty_ratio,
                w_bytes as f64 / dt / (1024.0 * 1024.0),
                if w_captures > 0 { w_grab_ms / w_captures as f64 } else { 0.0 },
            );
            total_captures += w_captures;
            total_keep += w_keep;
            total_bytes += w_bytes;
            w_captures = 0;
            w_keep = 0;
            w_damage = 0;
            w_cursor = 0;
            w_dirty_px = 0;
            w_grab_ms = 0.0;
            w_bytes = 0;
            window_start = now;
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    let secs = start.elapsed().as_secs_f64().max(1e-6);
    println!(
        "SUMMARY mode={} captures={} capture_fps={:.2} keepalives={} keepalive_fps={:.2} \
         read_mb_s={:.2} still_share={:.1}%",
        if args.baseline { "baseline" } else { "gating" },
        total_captures,
        total_captures as f64 / secs,
        total_keep,
        total_keep as f64 / secs,
        total_bytes as f64 / secs / (1024.0 * 1024.0),
        100.0 * total_keep as f64 / (total_captures + total_keep).max(1) as f64,
    );
    Ok(())
}
