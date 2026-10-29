// Bench: encode 2560x1440 desktop frames — openh264 thread sweep, and the real
// NVENC pipeline (XShm grab → ffmpeg stdin → drain access units).
use linux_uuremote::encoder::OpenH264;
use std::time::{Duration, Instant};

fn software() {
    let (w, h) = (2560usize, 1440usize);
    let mut frame = vec![0u8; w * h * 4];
    for (i, px) in frame.iter_mut().enumerate() {
        *px = ((i / 4) % 251) as u8;
    }
    for threads in [4usize, 8, 12, 16] {
        let enc = OpenH264::new(w, h, 30, 4000, 60, threads).unwrap();
        let mut e = enc;
        // warm-up IDR
        e.encode_bgra(&frame, true).unwrap();
        let t0 = Instant::now();
        let n = 12;
        for i in 0..n {
            // churn pixels so the encoder does real P-frame work
            for (j, px) in frame.iter_mut().enumerate().take(w * 40 * 4) {
                *px = ((i * 7 + j / 4) % 253) as u8;
            }
            e.encode_bgra(&frame, false).unwrap();
        }
        let ms = t0.elapsed().as_millis() as f64 / n as f64;
        println!("threads={threads}: {ms:.1} ms/frame → {:.1} fps", 1000.0 / ms);
    }
}

/// 真实管线吞吐：XShm 抓屏 + NVENC 喂帧 + 取成品帧 —— 就是宿主 daemon 跑的东西，
/// 只是没有 WebRTC 发送端，量出来即编码侧的上限。
fn nvenc(display: Option<&str>, fps: u32, frames: u32) {
    use linux_uuremote::capture::x11::{DamageLevel, X11Screen};
    use linux_uuremote::nvenc::{self, NvencH264};
    let mut screen = match X11Screen::open(display, DamageLevel::BoundingBox) {
        Ok(screen) => screen,
        Err(e) => {
            eprintln!("采集起不来：{}", e.0);
            return;
        }
    };
    let w = u32::from(screen.width()) & !1;
    let h = u32::from(screen.height()) & !1;
    let mut hw = match NvencH264::open(w, h, fps, 20_000, nvenc::preset_cq(5), false, None) {
        Ok(pipe) => pipe,
        Err(e) => {
            eprintln!("NVENC 起不来：{e}");
            return;
        }
    };
    let started = Instant::now();
    let mut fed = 0u32;
    let mut got = 0u64;
    let mut bytes = 0usize;
    let mut acc_grab = Duration::ZERO;
    let mut acc_feed = Duration::ZERO;
    let pace = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
    let mut next = Instant::now();
    while fed < frames {
        if Instant::now() < next {
            // 取走已编完的帧再等配速窗口，别让成品帧攒在管道里。
            while let Some(unit) = hw.next_unit(Duration::ZERO) {
                bytes += unit.len();
                got += 1;
            }
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        next += pace;
        let g0 = Instant::now();
        let (frame, _) = match screen.grab_rect(None) {
            Ok(grab) => grab,
            Err(e) => {
                eprintln!("抓取失败：{}", e.0);
                return;
            }
        };
        acc_grab += g0.elapsed();
        let f0 = Instant::now();
        if let Err(e) = hw.feed(&frame) {
            eprintln!("喂帧失败：{e}");
            break;
        }
        acc_feed += f0.elapsed();
        fed += 1;
        while let Some(unit) = hw.next_unit(Duration::ZERO) {
            bytes += unit.len();
            got += 1;
        }
    }
    // 收尾：编码器还有没吐完的帧。
    while let Some(unit) = hw.next_unit(Duration::from_millis(200)) {
        bytes += unit.len();
        got += 1;
    }
    let secs = started.elapsed().as_secs_f64().max(0.001);
    println!(
        "NVENC {w}x{h}@{fps}：喂 {fed} 帧 / 取 {got} 帧 / {bytes} 字节，{:.1} fps；每帧 抓取 {:.1} + 喂帧 {:.1} ms",
        fed as f64 / secs,
        acc_grab.as_millis() as f64 / fed.max(1) as f64,
        acc_feed.as_millis() as f64 / fed.max(1) as f64
    );
}

fn number(name: &str, fallback: u32) -> u32 {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(fallback)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let display = args
        .iter()
        .position(|a| a == "--display")
        .and_then(|i| args.get(i + 1))
        .map(String::from);
    if args.iter().any(|a| a == "--nvenc") {
        nvenc(display.as_deref(), number("--fps", 60), number("--frames", 120));
        return;
    }
    software();
}
