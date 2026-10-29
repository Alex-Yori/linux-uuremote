//! NVENC 硬件编码：FFmpeg 子进程；采集仍由本端做（XShm 读回），整帧 BGR0 从 stdin 喂进去。
//!
//! 两种采集方案的实测（本机 2560x1440）：
//! - `-f x11grab`（ffmpeg 自己抓屏，参照实现的路子）：300 帧 5.5 s = **54 fps**。
//!   x11grab 走 XGetImage，实测整屏读回 15–17 ms，比我们的 XShm（5.6 ms）慢一倍。
//! - XShm 读回 + stdin 喂帧：纯编码吞吐实测 **132 fps**（含 14.7 MB/帧 的管道写入），
//!   加抓取约 13 ms/帧 → **77 fps**。
//! 所以本机走喂帧；x11grab 只在共享内存读不到时才有意义。
//!
//! 码率策略 = **质量优先**（`-rc vbr -cq`，码率只当上限）。曾经用 `-rc cbr`：
//! 文本密集的 1440p 画面被恒定码率逼着抬 QP，用户读作"整个画面发糊"。
//! 缓冲区单位也踩过：`-bufsize` 的单位是**字节**，写 `bitrate*2` 当裸数字时
//! 30 Mbps 只有 60 KB < 一帧，VBV 把 IDR 量化到很狠（参照实现有同款教训）。
//!
//! 输出是 Annex-B 访问单元流，按 VCL NAL 边界切分（同 AU 里第二个 slice 起就是下一帧）。

use std::io::{self, BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// H.264 VCL NAL 类型：slice / data-partition / IDR。出现第二个即 AU 边界。
const H264_VCL: [u8; 3] = [1, 2, 5];

/// ffmpeg 是否带 h264_nvenc（进程内缓存，探测一次）。
pub fn available() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).contains("h264_nvenc"))
            .unwrap_or(false)
    })
}

/// `-fps_mode vfr -enc_time_base` 只在两个选项都可查时才用（ffmpeg 5.x+）；
/// 老版本退回 `-vsync 0`。x11grab 卡住后会连打补帧，vfr 把这些同槽帧丢掉，
/// 否则接收端会看到一小串 burst（参照实现实测 200 ms 补 69 帧）。
fn timing_args(fps: u32) -> Vec<String> {
    static MODERN: OnceLock<bool> = OnceLock::new();
    let modern = MODERN.get_or_init(|| {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-h", "full"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|output| {
                let help = String::from_utf8_lossy(&output.stdout);
                help.contains("enc_time_base") && help.contains("-fps_mode")
            })
            .unwrap_or(false)
    });
    if *modern {
        vec!["-fps_mode".into(), "vfr".into(), "-enc_time_base".into(), format!("1/{fps}")]
    } else {
        vec!["-vsync".into(), "0".into()]
    }
}

/// 画质档 → NVENC 常量质量 CQP（照参照实现：原画 17 / 超清 21 / 高清 26 / 流畅 30，
/// 自动与自定义 18）。质量优先时这才是"画质可调"的正解——码率只是天花板。
pub fn preset_cq(quality: u64) -> u32 {
    match quality {
        4 => 17,
        3 => 21,
        2 => 26,
        1 => 30,
        _ => 18,
    }
}

/// VBV 缓冲（kbit）= 码率上限 × 毫秒 / 1000。默认 250 ms：缓冲太小会把整屏 IDR
/// 量化得很狠，读起来就是"画面发糊、每个 GOP 慢慢变清晰"。`UU_NVENC_BUFFER_MS` 可改。
pub fn buffer_kbits(maxrate_kbps: u32) -> u32 {
    let ms = std::env::var("UU_NVENC_BUFFER_MS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(250)
        .clamp(10, 1000);
    (maxrate_kbps * ms / 1000).max(1_000)
}

/// 一路常驻的 NVENC H.264 编码管道。Drop（或换档位重启）即杀进程。
///
/// `closed` 表示 ffmpeg 的 stdout 已到 EOF——它可能已经退出（NVENC 会话被拒、
/// x11grab 连不上显示）。取帧前必须查它：无脑重建会每秒 spawn 上百个 ffmpeg，
/// 把 NVENC 会话槽和显存（每个 ~346 MB）吃光，帧率直接归零（2026-10-29 实测事故）。
pub struct NvencH264 {
    child: Child,
    stdin: std::process::ChildStdin,
    units: mpsc::Receiver<Vec<u8>>,
    closed: Arc<AtomicBool>,
}

impl Drop for NvencH264 {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl NvencH264 {
    /// `chroma_444` = 真彩：NVENC 实测能编 yuv444p（ffprobe 解出 High 4:4:4 Predictive），
    /// 客户端请求 4:4:4 时真切 pix_fmt + profile，不是口头拒绝。
    ///
    /// `-draw_mouse 0`：官方画面不含指针，客户端自己画光标（形状靠 Message 15 上报）。
    ///
    /// `capture_display = Some(":1")` 时让 ffmpeg 自己 x11grab 抓屏：实测 2K@90 管线
    /// 76 fps，而 XShm 抓屏 + stdin 喂帧只有 47 fps（每帧两次 14.7 MB 大拷贝 = 20 ms，
    /// 那是 90 帧档永远跑不满的真瓶颈）。None = rawvideo 从 stdin 喂帧（软编兜底后
    /// 的硬编重试也走同一构造）。
    pub fn open(
        width: u32,
        height: u32,
        fps: u32,
        maxrate_kbps: u32,
        cq: u32,
        chroma_444: bool,
        capture_display: Option<&str>,
    ) -> Result<Self, String> {
        let maxrate = maxrate_kbps.max(1_000);
        let mut command = Command::new("ffmpeg");
        command
            .args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        match capture_display {
            Some(dpy) => command.args([
                "-f", "x11grab", "-draw_mouse", "0",
                "-framerate", &fps.to_string(),
                "-video_size", &format!("{width}x{height}"),
                "-i", dpy,
            ]),
            None => command.args([
                "-f", "rawvideo", "-pix_fmt", "bgr0",
                "-s", &format!("{width}x{height}"),
                "-framerate", &fps.to_string(),
                "-i", "pipe:0",
            ]),
        };
        command
            .args(timing_args(fps))
            .args(["-c:v", "h264_nvenc", "-preset", "p4", "-tune", "ull", "-bf", "0"])
            .args(if chroma_444 {
                ["-profile:v", "high444p", "-pix_fmt", "yuv444p"].as_slice()
            } else {
                ["-profile:v", "high", "-pix_fmt", "yuv420p"].as_slice()
            })
            .args(["-g", &format!("{}", (fps.max(1) * 2).max(2))])
            // 质量优先：vbr + cq，码率只当上限（target = 3/4 上限，留突发余量）。
            .args([
                "-rc", "vbr",
                "-cq", &cq.to_string(),
                "-b:v", &format!("{}k", maxrate * 3 / 4),
                "-maxrate", &format!("{maxrate}k"),
                "-bufsize", &format!("{}k", buffer_kbits(maxrate)),
            ])
            .args([
                "-rc-lookahead", "0", "-zerolatency", "1", "-delay", "0", "-aud", "1",
                "-an", "-f", "h264", "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| format!("起 ffmpeg 失败：{e}"))?;
        let stdin = child.stdin.take().ok_or("ffmpeg 没有 stdin")?;
        let mut stdout = child.stdout.take().ok_or("ffmpeg 没有 stdout")?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_thread = closed.clone();
        std::thread::spawn(move || {
            let mut buffer: Vec<u8> = Vec::with_capacity(1 << 20);
            let mut chunk = [0u8; 1 << 16];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buffer.extend_from_slice(&chunk[..n]);
                        for unit in split_access_units(&mut buffer) {
                            if tx.send(unit).is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            closed_thread.store(true, Ordering::Relaxed);
        });
        let stderr = child.stderr.take();
        if let Some(mut stderr) = stderr {
            std::thread::spawn(move || {
                // ffmpeg 的错误必须看得见，否则只能靠 EOF 猜（驱动拒绝、会话上限、
                // 显示连不上都在这里报）。一次会话最多记几条，别刷日志。
                let mut lines = io::BufReader::new(&mut stderr).lines();
                let mut logged = 0usize;
                while let Some(Ok(line)) = lines.next() {
                    if logged < 8 {
                        logged += 1;
                        println!("   NVENC(ffmpeg)：{line}");
                    }
                }
            });
        }
        Ok(Self { child, stdin, units: rx, closed })
    }

    /// ffmpeg 是否还在跑（stdout EOF = 已退出）。
    pub fn alive(&self) -> bool {
        !self.closed.load(Ordering::Relaxed)
    }

    /// 喂一整帧 BGR0。管道满时阻塞——这本身就是天然配速。
    pub fn feed(&mut self, bgr0: &[u8]) -> io::Result<()> {
        self.stdin.write_all(bgr0)?;
        self.stdin.flush()
    }

    /// 取一个已完成的访问单元；超时返回 None（编码器还在攒头几帧）。
    pub fn next_unit(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.units.recv_timeout(timeout).ok()
    }
}

fn find_start_code(data: &[u8]) -> Option<usize> {
    data.windows(3).position(|window| window == [0, 0, 1])
}

/// 从缓冲区切出**完整**访问单元：第二个 VCL NAL 的起始码处即 AU 边界。
/// 不完整的尾帧留在 buffer 里等下一块数据。
fn split_access_units(buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    let mut au_start = 0usize;
    let mut has_slice = false;
    let mut i = 0usize;
    while let Some(relative) = find_start_code(&buffer[i..]) {
        let pos = i + relative;
        let nal_offset = if buffer[pos..].starts_with(&[0, 0, 0, 1]) { pos + 4 } else { pos + 3 };
        if buffer.len() <= nal_offset {
            break; // NAL 类型还没读进来，等下一块
        }
        let nal_type = buffer[nal_offset] & 0x1f;
        if H264_VCL.contains(&nal_type) {
            if has_slice {
                units.push(buffer[au_start..pos].to_vec());
                au_start = pos;
            }
            has_slice = true;
        }
        i = nal_offset;
    }
    if au_start > 0 {
        buffer.drain(..au_start);
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc4(nal: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, nal]
    }
    fn sc3(nal: u8) -> Vec<u8> {
        vec![0, 0, 1, nal]
    }

    #[test]
    fn second_slice_closes_the_access_unit() {
        // 单 slice 流：SPS+PPS+IDR 一帧，随后每个 slice 各一帧。
        let mut buffer = vec![
            sc4(0x67), sc3(0x68), sc3(0x65), sc3(0x41), sc3(0x41),
        ]
        .concat();
        let units = split_access_units(&mut buffer);
        assert_eq!(units.len(), 2, "第二个 slice 闭合 IDR 帧，第三个闭合 P 帧");
        assert_eq!(units[0].len(), 13);
        assert_eq!(units[1].len(), 4);
        assert_eq!(buffer.len(), 4, "最后一帧还没闭合，留在缓冲区");
    }

    #[test]
    fn single_open_unit_is_not_emitted() {
        let mut buffer = vec![sc4(0x67), sc3(0x68), sc3(0x65)].concat();
        assert!(split_access_units(&mut buffer).is_empty());
        assert_eq!(buffer.len(), 13);
    }

    #[test]
    fn non_vcl_nals_never_close_a_unit() {
        let mut buffer = vec![sc4(0x67), sc3(0x68), sc3(0x65), sc3(0x06), sc3(0x09)].concat();
        assert!(split_access_units(&mut buffer).is_empty());
    }

    #[test]
    fn quality_presets_map_to_constant_quality_not_bitrate_only() {
        assert_eq!(preset_cq(4), 17, "原画");
        assert_eq!(preset_cq(3), 21, "超清");
        assert_eq!(preset_cq(2), 26, "高清");
        assert_eq!(preset_cq(1), 30, "流畅");
        assert_eq!(preset_cq(5), 18, "自动");
        assert_eq!(preset_cq(6), 18, "自定义（码率当上限）");
    }

    #[test]
    fn vbv_buffer_scales_with_bitrate_and_is_not_smaller_than_a_frame() {
        // 250 ms 缓冲 = 码率 × 0.25；30 Mbps 时 7500 kbit ≈ 11 帧（一帧 667 kbit），
        // 远大于单帧——缓冲小于几帧时 IDR 会被量化到发糊（参照实现的教训）。
        let bits = buffer_kbits(30_000);
        let frame_bits = 30_000 / 45;
        assert_eq!(bits, 7_500, "30 Mbps 默认 250 ms 缓冲");
        assert!(bits >= frame_bits * 4, "缓冲至少装得下 4 帧（{bits} vs {frame_bits} kbit）");
        assert!(buffer_kbits(1_000) >= 1_000, "低码率也有下限");
    }
}
