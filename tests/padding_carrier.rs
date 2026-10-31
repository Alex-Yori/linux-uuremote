//! 对着**真正上网的那层**（`rtp::codecs::h264::H264Payloader`）验证 padding 载体。
//!
//! 实机踩坑（2026-10-31）：宿主账面「发送带宽 2000 kbps」而客户端显示 176 kbps、
//! wlo1 实测 381 kbps —— 补的 Filler Data NAL(type 12) 被打包器 `emit()` 直接 `return`
//! 丢掉，一个字节都没上网。这个测试把「账面≠线上」这一层钉死：
//! - Filler(12)：payload 出来必须为空（证明坑还在，载体不能选它）；
//! - SEI(6)：payload 必须非空且总字节数 == 输入 NAL 字节数（我们的载体，真的上网）。
use rtp::codecs::h264::H264Payloader;
use rtp::packetizer::Payloader;

const MTU: usize = 1200;

fn payload_bytes(nal: &[u8]) -> usize {
    H264Payloader::default()
        .payload(MTU, &bytes::Bytes::from(nal.to_vec()))
        .unwrap()
        .iter()
        .map(|p| p.len())
        .sum()
}

/// 一个大 Filler Data NAL（type 12）：打包器丢弃 → 线上 0 字节。
#[test]
fn payloader_drops_filler_nal() {
    let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x0C];
    nal.resize(20_000, 0xFF);
    assert_eq!(payload_bytes(&nal), 0, "Filler NAL 不上网，所以不能当 padding 载体");
}

/// 一个大 AUD NAL（type 9）：同样被丢（顺带说明 -aud 1 的开销其实没上网）。
#[test]
fn payloader_drops_aud_nal() {
    let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00];
    nal.resize(20_000, 0xFF);
    assert_eq!(payload_bytes(&nal), 0);
}

/// 我们实际的 AU（内容 slice + 尾部 SEI padding）：SEI 必须整段透传。
#[test]
fn payloader_forwards_sei_padding() {
    use linux_uuremote::padding::RateFloor;
    use std::time::{Duration, Instant};

    let t0 = Instant::now();
    let mut f = RateFloor::new(2000, t0);
    // 静态页形态：内容极小（213B），90fps 一帧 → 补到 ~2.5KB/帧。
    let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x41];
    au.resize(213, 0x11);
    let added = f.pad(&mut au, t0 + Duration::from_millis(11));
    assert!(added > 1_500, "补了 {added} 字节");

    let on_wire = payload_bytes(&au);
    // 打包器可能做 STAP-A/FU-A 重组，字节量应≈输入（允许分片头开销 ±MTU）。
    assert!(
        on_wire >= au.len() - 8,
        "线上只有 {on_wire} 字节，输入 {} 字节 → padding 又没上网",
        au.len()
    );
    assert!(on_wire <= au.len() + 40, "线上 {on_wire} vs 输入 {}", au.len());
}
