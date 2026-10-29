//! 桌面声音：默认输出的 monitor 送进 ffmpeg 编成 Opus，从 offer 谈定的音频 m 行发出。
//!
//! 路线照参照实现（fork）：客户端 offer 永远带 Opus（官方 Windows 解码侧实锤有
//! opusdec），被控端在 answer 里加 audio 轨。本机是 PipeWire + pulse 兼容层，
//! `pactl get-default-sink` + `.monitor` 实测可抓（静音/暂停时出静音包，链路不断）。
//!
//! `UU_AUDIO=0` 关闭；`UU_AUDIO_SOURCE` 指定别的 Pulse source。
//! 音频载荷是内容，不写日志、不落盘。

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;
use webrtc::media::io::ogg_reader::OggReader;
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// 每个 Opus 包一个 20 ms 帧（48 kHz 立体声 = 1920 采样）。
pub const FRAME: Duration = Duration::from_millis(20);

pub fn enabled() -> bool {
    std::env::var("UU_AUDIO").map_or(true, |v| v != "0")
}

/// 抓流 source：虚拟路由开着 = 自建 monitor（本地静音也能给远端有声）；
/// 否则 `UU_AUDIO_SOURCE` 覆盖，再否则默认输出的 monitor。
pub fn default_source() -> String {
    if let Some(source) = std::env::var("UU_AUDIO_SOURCE").ok().filter(|v| !v.is_empty()) {
        return source;
    }
    if crate::virtual_audio::active() {
        return crate::virtual_audio::MONITOR_SOURCE.to_owned();
    }
    match std::process::Command::new("pactl")
        .args(["get-default-sink"])
        .stderr(Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => {
            let sink = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            format!("{sink}.monitor")
        }
        _ => "@DEFAULT_MONITOR@".to_owned(),
    }
}

/// 一路常驻的 Opus 编码子进程（stdout = Ogg 流）。Drop 即杀。
struct Encoder {
    child: Child,
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Encoder {
    fn spawn(source: &str) -> Result<Self, String> {
        let mut command = Command::new("ffmpeg");
        command.args([
            "-hide_banner", "-loglevel", "error", "-nostdin",
            "-f", "pulse", "-fragment_size", "1920", "-i", source, "-vn",
            "-c:a", "libopus", "-ar", "48000", "-ac", "2", "-b:a", "128k",
            "-application", "lowdelay", "-frame_duration", "20", "-page_duration", "20000",
            "-flush_packets", "1", "-f", "ogg", "pipe:1",
        ]);
        // pulse source 连不上时 ffmpeg 直接报错退出；错误只报进程级，不回放内容。
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().map_err(|e| format!("起音频编码器失败：{e}"))?;
        Ok(Self { child })
    }
}

/// 从编码器子进程读包进通道（spawn_blocking）。返回 (停止=kill child, 读任务)。
fn start_encoder(source: &str, tx: mpsc::Sender<Bytes>) -> Result<(Encoder, tokio::task::JoinHandle<Result<(), String>>), String> {
    let mut encoder = Encoder::spawn(source)?;
    let stdout = encoder.child.stdout.take().ok_or("音频编码器没有 stdout")?;
    let job = tokio::task::spawn_blocking(move || {
        let mut stdout = stdout;
        let (mut reader, _) = OggReader::new(&mut stdout as &mut dyn Read, false)
            .map_err(|e| format!("音频 Ogg 头：{e}"))?;
        loop {
            match reader.parse_next_page() {
                Ok((payload, _)) => {
                    if payload.starts_with(b"OpusTags") {
                        continue;
                    }
                    if tx.blocking_send(payload.freeze()).is_err() {
                        return Ok(());
                    }
                }
                Err(e) => return Err(format!("音频 Ogg 页：{e}")),
            }
        }
    });
    Ok((encoder, job))
}

fn closed(pc: &RTCPeerConnection) -> bool {
    matches!(
        pc.connection_state(),
        RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
    )
}

/// 会话 Connected 之后持续把桌面声音发到音频轨；编码器崩了就重启，连崩 5 次放弃本次会话。
pub async fn stream_audio(pc: Arc<RTCPeerConnection>, track: Arc<TrackLocalStaticSample>) {
    while pc.connection_state() != RTCPeerConnectionState::Connected {
        if closed(&pc) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut failures = 0u32;
    while !closed(&pc) {
        let source = default_source();
        let (tx, mut rx) = mpsc::channel::<Bytes>(25);
        let (encoder, job) = match start_encoder(&source, tx) {
            Ok(started) => started,
            Err(e) => {
                println!("   音频：{e}");
                return;
            }
        };
        println!("   音频已启动：opus 48k 立体声（source 已选定，不打印内容）");
        let mut packets = 0u64;
        let mut health = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                packet = rx.recv() => {
                    let Some(data) = packet else { break };
                    if let Err(e) = track
                        .write_sample(&Sample { data, duration: FRAME, ..Default::default() })
                        .await
                    {
                        println!("   音频发送失败：{}", e.to_string().chars().take(120).collect::<String>());
                        break;
                    }
                    packets += 1;
                }
                _ = health.tick() => {
                    if closed(&pc) || default_source() != source {
                        break;
                    }
                }
            }
        }
        // 先关读侧再杀编码器：任务会在下一个包（或 EOF）处结束。
        drop(rx);
        let encoder = encoder;
        let result = job.await.unwrap_or_else(|e| Err(e.to_string()));
        drop(encoder);
        if closed(&pc) {
            break;
        }
        if default_source() != source {
            // 用户换了默认输出：换 source 重启，不算失败。
            failures = 0;
            continue;
        }
        println!(
            "   音频停了（共 {packets} 包）：{}",
            match result {
                Ok(()) => "编码器到流尾".to_owned(),
                Err(e) => e.chars().take(120).collect::<String>(),
            }
        );
        failures = if packets > 250 { 0 } else { failures + 1 };
        if failures >= 5 {
            println!("   本次会话音频停用（连崩 5 次）");
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    println!("   音频结束");
}

/// 探针：抓 N 秒，只报包数/码率/间隔，不碰内容。
pub async fn probe(seconds: u64) -> Result<(), String> {
    let source = default_source();
    let (tx, mut rx) = mpsc::channel::<Bytes>(256);
    let (encoder, job) = start_encoder(&source, tx)?;
    let started = std::time::Instant::now();
    let mut packets = 0u64;
    let mut bytes = 0u64;
    let mut max_gap = Duration::ZERO;
    let mut last = None::<std::time::Instant>;
    while started.elapsed() < Duration::from_secs(seconds) {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(data)) => {
                let now = std::time::Instant::now();
                if let Some(prev) = last {
                    max_gap = max_gap.max(now - prev);
                }
                last = Some(now);
                packets += 1;
                bytes += data.len() as u64;
            }
            _ => break,
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    drop(rx);
    let result = job.await.map_err(|e| e.to_string())?;
    drop(encoder);
    println!(
        "音频探针：{} 包 / {} 字节 ≈ {:.1} kbps，{:.1} 包/s，最大间隔 {} ms",
        packets,
        bytes,
        bytes as f64 * 8.0 / 1000.0 / elapsed,
        packets as f64 / elapsed,
        max_gap.as_millis()
    );
    if packets == 0 {
        result
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uu_audio_zero_disables() {
        // 串行化：与本模块其他测试共 env。
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("UU_AUDIO", "0");
        assert!(!enabled());
        std::env::set_var("UU_AUDIO", "1");
        assert!(enabled());
        std::env::remove_var("UU_AUDIO");
    }

    #[test]
    fn explicit_source_wins() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("UU_AUDIO_SOURCE", "weird.monitor");
        assert_eq!(default_source(), "weird.monitor");
        std::env::remove_var("UU_AUDIO_SOURCE");
    }
}
