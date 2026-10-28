//! `uu-damage-load` — a controlled test load for the capture side.
//!
//! Maps its own window and sweeps a small filled rectangle inside it, so the capture
//! pipeline has a dirty area of known size to measure against. Without this, the only
//! dirty-area numbers come from whatever the desktop happens to be doing, which makes a
//! dirty-rect vs full-screen comparison unreproducible.
//!
//! Usage:
//!   uu-damage-load [--display :1] [--secs 20] [--window 320x240] [--rect 64x48]

use std::error::Error;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeGCAux, ConnectionExt, CreateGCAux, CreateWindowAux, Rectangle, WindowClass,
};

fn parse_size(text: &str, default: (u16, u16)) -> (u16, u16) {
    let Some((w, h)) = text.split_once('x') else {
        return default;
    };
    match (w.parse::<u16>(), h.parse::<u16>()) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
        _ => default,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let argv: Vec<String> = std::env::args().collect();
    let mut display: Option<String> = None;
    let mut secs = 20u64;
    let mut window = (320u16, 240u16);
    let mut rect = (64u16, 48u16);

    let mut i = 1usize;
    while i < argv.len() {
        // Bind the flag before the value closure borrows `i`.
        let flag = argv[i].as_str();
        let mut next = || -> String {
            i += 1;
            argv.get(i).cloned().unwrap_or_default()
        };
        match flag {
            "--display" => display = Some(next()),
            "--secs" => secs = next().parse().unwrap_or(20),
            "--window" => window = parse_size(&next(), window),
            "--rect" => rect = parse_size(&next(), rect),
            "--help" | "-h" => {
                println!(
                    "usage: uu-damage-load [--display :1] [--secs 20] [--window 320x240] [--rect 64x48]"
                );
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    if rect.0 >= window.0 || rect.1 >= window.1 {
        eprintln!("the painted rectangle has to be smaller than the window");
        std::process::exit(2);
    }

    let (conn, screen_num) = x11rb::connect(display.as_deref())?;
    let screen = &conn.setup().roots[screen_num];

    let wid = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        wid,
        screen.root,
        120,
        120,
        window.0,
        window.1,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        // True: the window manager must not move the window, so the dirty area it produces
        // stays where this program put it.
        &CreateWindowAux::new().background_pixel(0x0020_2020).override_redirect(1u32),
    )?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, wid, &CreateGCAux::new())?;
    conn.map_window(wid)?;
    conn.flush()?;

    // Sweep horizontally and step down every few frames, so successive damage reports land
    // in different places rather than redrawing one fixed spot.
    let step_x = 8u16;
    let columns = ((window.0 - rect.0) / step_x).max(1) as u64;
    let period = Duration::from_millis(30);
    let started = Instant::now();
    let mut frame = 0u64;

    while started.elapsed() < Duration::from_secs(secs) {
        let phase = frame % (columns * 2);
        let col = if phase < columns {
            phase
        } else {
            columns * 2 - phase
        } as u16 * step_x;
        let row = ((frame / columns) % ((window.1 - rect.1) / 12).max(1) as u64) as u16 * 12;

        // Vary the fill color so the pixels actually change, not just the damage report.
        let hue = (frame % 64) as u32;
        conn.change_gc(gc, &ChangeGCAux::new().foreground(0x00FF_0000 | (hue << 8) | hue))?;
        conn.poly_fill_rectangle(
            wid,
            gc,
            &[Rectangle {
                x: col as i16,
                y: row as i16,
                width: rect.0,
                height: rect.1,
            }],
        )?;
        conn.flush()?;

        frame += 1;
        std::thread::sleep(period);
    }

    println!(
        "painted {frame} rectangles of {}x{} inside a {}x{} window over {:.1}s",
        rect.0,
        rect.1,
        window.0,
        window.1,
        started.elapsed().as_secs_f64()
    );
    conn.destroy_window(wid)?;
    conn.flush()?;
    Ok(())
}
