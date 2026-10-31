//! 发送端带宽地板 padding —— 对齐官方引擎的恒定带宽行为。
//!
//! REA 逆向 `streamer.dll` 实锤（references/official-padding-bandwidth.md）：官方引擎内嵌
//! libwebrtc 发包栈，`bitrate_allocator.cc` 给视频一条恒定分配目标，编码器输出不足时由
//! `PacketRouter::GeneratePadding`（`padding_budget_kbps` / `padding_debt` /
//! `avg_padding_bitrate_bps`）把链路流量补满到线上。Windows↔Windows 实测基线：原画 30M /
//! 90 帧配置、静态页面 ≈47fps 稳 2Mbps 不振荡——每帧字节被拉平，而不是内容驱动的
//! 170K↔3M 闪烁。
//!
//! 我们没有 libwebrtc pacer，等价做法：把每个 AU 用 H.264 **Filler Data NAL**
//! （nal_unit_type=12，解码器整条跳过）补到「地板码率 × 帧时长」的单帧预算线。
//! 只加字节、不加帧数 → **帧率完全不动**；活跃画面自身超过地板时不补 →
//! 带宽 = max(内容, 地板)，静止窗从 170K 抬到地板，振荡收敛。

use std::time::Duration;

/// 单 AU filler 字节上限：预算线本身封顶（30Mbps 地板 × 1s 帧间隔 = 3.75MB），
/// 防爆主要是防把 IDR 尖峰再叠一层。
pub const MAX_PAD_BYTES: usize = 256 * 1024;

/// Filler Data NAL 头：forbidden=0, nal_ref_idc=3, nal_unit_type=12。
const FILLER_START: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x6C];

/// 把 AU 补到 `budget` 字节；超过预算不补。返回实际追加字节数。
///
/// 0xFF payload 不产生 00 00 序列（无防仿真问题）；type 12 不参与参考帧，
/// 对解码器与后续 P 帧引用链完全透明。
pub fn pad_au_to(au: &mut Vec<u8>, budget: usize) -> usize {
    if au.len() >= budget {
        return 0;
    }
    let need = (budget - au.len()).min(MAX_PAD_BYTES);
    if need < FILLER_START.len() + 1 {
        return 0; // 放不下 起始码+NAL头+payload(≥1)
    }
    au.reserve(need);
    au.extend_from_slice(FILLER_START);
    au.resize(au.len() + need - FILLER_START.len(), 0xFF);
    need
}

/// 按帧时长折算单帧预算线：`floor_kbps × duration`。floor=0 → 不补。
pub fn frame_budget(floor_kbps: u64, duration: Duration) -> usize {
    if floor_kbps == 0 {
        return 0;
    }
    (floor_kbps as f64 * 1000.0 / 8.0 * duration.as_secs_f64()) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_au() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x09, 0x11, 0x22, 0x33]
    }

    #[test]
    fn disabled_when_zero_floor() {
        let mut au = vec![0u8; 1900];
        assert_eq!(pad_au_to(&mut au, frame_budget(0, Duration::from_millis(11))), 0);
        assert_eq!(au.len(), 1900);
    }

    #[test]
    fn small_au_padded_to_budget_line() {
        let mut au = vec![0u8; 1900];
        let budget = frame_budget(2000, Duration::from_millis(11)); // 2M × 11ms ≈ 2750B
        let added = pad_au_to(&mut au, budget);
        assert_eq!(au.len(), budget);
        assert_eq!(added, budget - 1900);
        assert_eq!(&au[1900..1905], FILLER_START);
        assert!(au[1905..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn large_au_not_padded() {
        let mut au = vec![0u8; 40_000];
        assert_eq!(pad_au_to(&mut au, frame_budget(2000, Duration::from_millis(11))), 0);
        assert_eq!(au.len(), 40_000);
    }

    #[test]
    fn steady_flow_holds_floor_bandwidth_at_12fps() {
        // 静止档 12fps、每帧真实 1000B：4 秒窗口总流量贴 2Mbps ±5%。
        let step = Duration::from_millis(83);
        let mut total: usize = 0;
        for _ in 0..48 {
            let mut au = vec![0u8; 1000];
            pad_au_to(&mut au, frame_budget(2000, step));
            total += au.len();
        }
        let secs = 48.0 * step.as_secs_f64();
        let bps = total as f64 * 8.0 / secs;
        assert!((bps - 2_000_000.0).abs() < 100_000.0, "bps={bps}");
    }

    #[test]
    fn micro_frames_at_90fps_padded_to_floor() {
        // 90fps 静止微帧 1.9KB（实测 170kbps 形态）：补齐后窗口 ≈ 2Mbps。
        let step = Duration::from_millis(11);
        let mut total: usize = 0;
        for _ in 0..90 {
            let mut au = vec![0u8; 1900];
            pad_au_to(&mut au, frame_budget(2000, step));
            total += au.len();
        }
        let secs = 90.0 * step.as_secs_f64();
        let bps = total as f64 * 8.0 / secs;
        assert!((bps - 2_000_000.0).abs() < 120_000.0, "bps={bps}");
    }

    #[test]
    fn active_content_above_floor_not_padded() {
        // 活跃内容 90fps × 30KB（≈21.6Mbps）远高于地板 → 一字节不补。
        let step = Duration::from_millis(11);
        let mut total: usize = 0;
        for _ in 0..90 {
            let mut au = vec![0u8; 30_000];
            pad_au_to(&mut au, frame_budget(2000, step));
            total += au.len();
        }
        let bps = total as f64 * 8.0 / (90.0 * step.as_secs_f64());
        assert!((bps - 21_818_000.0).abs() < 500_000.0, "bps={bps}");
    }

    #[test]
    fn pad_cap_bounds_explosion() {
        let mut au = base_au();
        // 30Mbps 地板 × 2s 帧间隔 = 7.5MB 预算，必须被 MAX_PAD_BYTES 夹住。
        let added = pad_au_to(&mut au, frame_budget(30_000, Duration::from_secs(2)));
        assert!(added <= MAX_PAD_BYTES, "added={added}");
    }
}
