//! NVENC 硬件编码：FFmpeg 子进程（参照实现在本机就是这么把 1440p60 跑稳的，
//! 码率 CBR 恒定、帧率恒定；openh264 软编只做兜底）。
//!
//! 协议：stdin 喂 rawvideo BGR0 整帧，stdout 出 Annex-B 访问单元流。
//! 输入按 `-framerate` 名义配速，NVENC `p6 + tune ll` 几乎逐帧即时出；
//! 访问单元按 VCL NAL 边界切分（同 AU 里第二个 slice 起就是下一帧）。

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::OnceLock;
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

/// 一路常驻的 NVENC H.264 编码管道。Drop（或换档位重启）即杀进程。
pub struct NvencH264 {
    child: Child,
    stdin: ChildStdin,
    units: mpsc::Receiver<Vec<u8>>,
}

impl Drop for NvencH264 {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl NvencH264 {
    /// `chroma_444` = 真彩：NVENC 实测能编 yuv444p（ffprobe 解出 High 4:4:4 Predictive），
    /// 客户端请求 4:4:4 时照参照实现（NVENC 全色度管线）切过去，不是口头拒绝。
    pub fn open(width: u32, height: u32, fps: u32, bitrate_kbps: u32, chroma_444: bool) -> Result<Self, String> {
        let bitrate = bitrate_kbps.max(1_000);
        let mut command = Command::new("ffmpeg");
        command
            .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args([
                "-f", "rawvideo", "-pix_fmt", "bgr0",
                "-s", &format!("{width}x{height}"),
                "-framerate", &fps.to_string(),
                "-i", "pipe:0",
            ])
            .args([
                "-c:v", "h264_nvenc", "-preset", "p6", "-tune", "ll",
                "-bf", "0",
            ])
            // 4:4:4 不锁 profile：ffmpeg 自动选 High 4:4:4 Predictive（实测可解）。
            .args(if chroma_444 { ["-pix_fmt", "yuv444p"].as_slice() } else { ["-profile:v", "high", "-pix_fmt", "yuv420p"].as_slice() })
            .args([
                "-rc", "cbr",
                "-b:v", &format!("{bitrate}k"),
                "-maxrate", &format!("{bitrate}k"),
                "-bufsize", &format!("{}", bitrate * 2),
                "-g", &format!("{}", (fps.max(1) * 2).max(2)),
                "-an", "-f", "h264", "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|e| format!("起 ffmpeg 失败：{e}"))?;
        let stdin = child.stdin.take().ok_or("ffmpeg 没有 stdin")?;
        let mut stdout = child.stdout.take().ok_or("ffmpeg 没有 stdout")?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
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
        });
        Ok(Self { child, stdin, units: rx })
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
}
