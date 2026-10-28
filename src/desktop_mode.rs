//! X11 桌面分辨率跟着客户端的"分辨率"档位走（照参照实现与官方 Windows 宿主的做法）：
//! 改的是 **显示模式本身**（xrandr），不是只缩编码流；断开最后一路桌面流时恢复原始模式。
//! `UU_DESKTOP_RESIZE=0` 关掉（客户端档位则只记进设置、不改显示）。

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Native,
    Size(u32, u32),
}

/// Clone 是为了 restore() 能在锁外跑 xrandr。
#[derive(Clone)]
struct Original {
    display: Option<String>,
    output: String,
    mode: String,
}

static ORIGINAL: Mutex<Option<Original>> = Mutex::new(None);
static STREAMS: AtomicUsize = AtomicUsize::new(0);

/// 单边上限（H.264 编码器上限）与可用桌面下限，照参照实现。
const MAX_SIDE: u32 = 4096;
const MIN_SIZE: (u32, u32) = (640, 480);

pub fn enabled() -> bool {
    std::env::var("UU_DESKTOP_RESIZE").map_or(true, |v| v != "0")
}

/// 有桌面流在跑就保持改过的模式；最后一路结束才恢复。
pub struct StreamGuard;

impl StreamGuard {
    pub fn new() -> Self {
        STREAMS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if STREAMS.fetch_sub(1, Ordering::SeqCst) == 1 && ORIGINAL.lock().unwrap().is_some() {
            // xrandr 会阻塞，别在异步运行时线程上跑它。
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if STREAMS.load(Ordering::SeqCst) == 0 {
                    restore();
                }
            });
        }
    }
}

/// 把客户端的分辨率请求应用到 X11 显示；返回此刻桌面真实尺寸，改不动时 None。
pub fn apply(display: Option<&str>, request: Request) -> Option<(u32, u32)> {
    match request {
        Request::Native => {
            restore();
            current_size(display)
        }
        Request::Size(w, h) => {
            let target = fit(w, h)?;
            match switch(display, target) {
                Ok(()) => current_size(display),
                Err(error) => {
                    println!("   桌面分辨率切到 {w}x{h} 失败：{error}（保持现模式推流）");
                    None
                }
            }
        }
    }
}

/// 偶数边、MIN..=MAX 之间，等比缩小保持宽高比。
fn fit(w: u32, h: u32) -> Option<(u32, u32)> {
    if w == 0 || h == 0 {
        return None;
    }
    let scale = (f64::from(MAX_SIDE) / f64::from(w.max(h))).min(1.0);
    let (w, h) = (
        (f64::from(w) * scale).round() as u32,
        (f64::from(h) * scale).round() as u32,
    );
    Some((w.max(MIN_SIZE.0) & !1, h.max(MIN_SIZE.1) & !1))
}

struct State {
    output: String,
    current: Option<(u32, u32)>,
    current_mode: Option<String>,
    modes: Vec<(String, (u32, u32))>,
}

/// 解析 `xrandr` 文本输出：输出行 `<name> connected [primary] WxH+X+Y`，
/// 其下缩进的 mode 行 `<name> <refresh>* ...`（名字形如 `2560x1440` 或 `3840x2160_60.00`）。
fn parse_xrandr(text: &str) -> Option<State> {
    let mut state: Option<State> = None;
    for line in text.lines() {
        if let Some(idx) = line.find(" connected ") {
            let name = &line[..idx];
            let rest = &line[idx + " connected ".len()..];
            // 尺寸可能跟在 `primary` 后面（也可能带 `disconnected` 输出行没有尺寸）。
            let size = rest.split_whitespace().find_map(parse_size);
            state = Some(State {
                output: name.to_owned(),
                current: size,
                current_mode: None,
                modes: Vec::new(),
            });
        } else if let Some(s) = state.as_mut() {
            if line.starts_with("  ") {
                let mut it = line.split_whitespace();
                let mode = it.next()?;
                let size = parse_mode_size(mode)?;
                // 星号=当前模式；线上真形态是 `240.00*+`，星不一定在末尾。
                let starred = it.next().is_some_and(|r| r.contains('*'));
                s.modes.push((mode.to_owned(), size));
                if starred {
                    s.current_mode = Some(mode.to_owned());
                }
            }
        }
    }
    state
}

fn parse_size(token: &str) -> Option<(u32, u32)> {
    let core = token.split('+').next()?;
    let (w, h) = core.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// 模式名容忍官方后缀（`3840x2160_60.00`）：只取前导数字。
fn parse_mode_size(token: &str) -> Option<(u32, u32)> {
    let (w, h) = token.split_once('x')?;
    let digits = |s: &str| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>().ok();
    Some((digits(w)?, digits(h)?))
}

fn query(display: Option<&str>) -> Result<State, String> {
    let mut command = Command::new("xrandr");
    if let Some(d) = display {
        command.env("DISPLAY", d);
    }
    let out = command.output().map_err(|e| format!("xrandr 起不来：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "xrandr 退出 {}",
            out.status.code().unwrap_or(-1)
        ));
    }
    parse_xrandr(&String::from_utf8_lossy(&out.stdout)).ok_or("xrandr 输出里没有已连接的输出".into())
}

fn current_size(display: Option<&str>) -> Option<(u32, u32)> {
    query(display).ok()?.current
}

fn switch(display: Option<&str>, target: (u32, u32)) -> Result<(), String> {
    let state = query(display)?;
    if state.current == Some(target) {
        return Ok(());
    }
    {
        let mut original = ORIGINAL.lock().unwrap();
        if original.is_none() {
            let mode = state.current_mode.clone().ok_or("当前模式名未知，没法恢复")?;
            println!(
                "   桌面分辨率：记住原始模式 {}（{}），断开流时恢复",
                mode, state.output
            );
            *original = Some(Original {
                display: display.map(str::to_owned),
                output: state.output.clone(),
                mode,
            });
        }
    }
    let mode = match state.modes.iter().find(|(_, size)| *size == target) {
        Some((name, _)) => name.clone(),
        None => {
            let (name, timings) = cvt_reduced(target.0, target.1)?;
            // 同名模式已存在（上次会话建的）时 --newmode 失败无害，继续。
            let mut args = vec!["--newmode", name.as_str()];
            args.extend(timings.iter().map(String::as_str));
            run_xrandr(display, &args).ok();
            run_xrandr(
                display,
                &["--addmode", state.output.as_str(), name.as_str()],
            )?;
            name
        }
    };
    run_xrandr(
        display,
        &["--output", state.output.as_str(), "--mode", mode.as_str()],
    )?;
    println!("   桌面分辨率已切到 {target:?}（模式 {mode}）");
    Ok(())
}

fn restore() {
    let Some(original) = ORIGINAL.lock().unwrap().clone() else {
        return;
    };
    match run_xrandr(
        original.display.as_deref(),
        &[
            "--output",
            original.output.as_str(),
            "--mode",
            original.mode.as_str(),
        ],
    ) {
        Ok(()) => println!("   桌面分辨率已恢复原始模式 {}", original.mode),
        Err(error) => println!("   恢复原始分辨率失败：{error}"),
    }
}

fn run_xrandr(display: Option<&str>, args: &[&str]) -> Result<(), String> {
    let mut command = Command::new("xrandr");
    if let Some(d) = display {
        command.env("DISPLAY", d);
    }
    let out = command
        .args(args)
        .output()
        .map_err(|e| format!("xrandr {args:?} 起不来：{e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "xrandr {args:?} 失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// `cvt -r w h`（减同步时序，eDP 友好）→ (模式名, timings)。
fn cvt_reduced(w: u32, h: u32) -> Result<(String, Vec<String>), String> {
    let out = Command::new("cvt")
        .args(["-r", &w.to_string(), &h.to_string()])
        .output()
        .map_err(|e| format!("cvt 起不来：{e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let name = text
        .lines()
        .find(|l| l.starts_with("# ModeLine"))
        .and_then(|l| l.split_whitespace().nth(2))
        .ok_or("cvt 没给出 ModeLine")?
        .trim_matches('"')
        .to_owned();
    let timings = text
        .lines()
        .find(|l| l.starts_with("Modeline"))
        .and_then(|l| {
            let open = l.find('(')? + 1;
            let close = l.find(')')?;
            Some(l[open..close].split_whitespace().map(str::to_owned).collect())
        })
        .ok_or("cvt 没给出 Modeline timings")?;
    Ok((name, timings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_clamps_and_keeps_aspect() {
        assert_eq!(fit(3840, 2160), Some((3840, 2160)));
        assert_eq!(fit(4200, 4200), Some((4096, 4096)));
        assert_eq!(fit(1919, 1079), Some((1918, 1078)));
        assert_eq!(fit(0, 1080), None);
    }

    #[test]
    fn parse_xrandr_finds_current_mode_and_sizes() {
        let text = "Screen 0: minimum 320 x 200, current 2560 x 1440, maximum 16384 x 16384\neDP-1 connected primary 2560x1440+0+0 (normal left inverted right x axis y axis) 597mm x 336mm\n   2560x1440     240.00* 165.00  60.00\n   1920x1080     60.00  50.00\n   3840x2160_60.00  60.00\nHDMI-2 disconnected (normal left inverted right x axis y axis)\n";
        let state = parse_xrandr(text).unwrap();
        assert_eq!(state.output, "eDP-1");
        assert_eq!(state.current, Some((2560, 1440)));
        assert_eq!(state.current_mode.as_deref(), Some("2560x1440"));
        assert!(state.modes.iter().any(|(n, s)| n == "3840x2160_60.00" && *s == (3840, 2160)));
        assert!(parse_xrandr("eDP-1 disconnected\n").is_none());
        assert_eq!(parse_mode_size("3840x2160_60.00"), Some((3840, 2160)));
    }

    #[test]
    fn parse_size_splits_before_plus() {
        assert_eq!(parse_size("2560x1440+0+0"), Some((2560, 1440)));
        assert_eq!(parse_size("3840x2160_60.00"), None); // 模式名带后缀不是尺寸
    }
}
