//! 发送端带宽地板 padding —— 对齐官方引擎的 ALR padding（credit/debt 模型）。
//!
//! REA 逆向 `streamer.dll` 实锤（references/official-padding-bandwidth.md）：官方引擎内嵌
//! libwebrtc 发包栈，`bitrate_allocator.cc` 给视频一条恒定分配目标，编码器输出不足时由
//! `PacketRouter::GeneratePadding` 补满到线上，官方日志字段 `padding_budget_kbps` /
//! `padding_debt` 说明它是**按时间累计的字节预算**，不是逐帧补齐——IDR 大帧吃空预算后，
//! 后续小帧补得少，长时间窗流量恒定。
//!
//! 第一版按「帧名义时长 × 地板」逐帧预算踩了坑（2026-10-31 实机：每帧都补到满预算，
//! 周期性 IDR 又整坨叠在窗口上 → 总量仍 3360↔4044 逐窗交替）。正解 = 速率积分器：
//! - 每帧按真实间隔把 `credit += floor × dt`；
//! - 内容字节花 credit；credit > 0 就补 filler NAL 把差额花光（单帧封顶防爆）；
//! - IDR 尖峰把 credit 打成负数（debt）→ 之后自动少补/不补 → 窗口均值贴地板。
//! 只加字节、不加帧数 → **帧率完全不动**。

use std::time::Instant;

/// 单帧 filler 字节上限：credit 大额积压（长空闲后）一帧吃不完的下次继续花。
pub const MAX_PAD_BYTES: usize = 64 * 1024;

/// credit 积压上限（秒数）：空闲攒的预算最多抵这么久，防止长空闲后一坨暴补。
const MAX_CREDIT_SECONDS: f64 = 1.0;

/// 欠债上限（秒数）：IDR 尖峰的债最多记这么久，超出部分的尖峰不指望后续帧抹平。
const DEBT_CAP_SECONDS: f64 = 2.0;

/// Filler Data NAL：forbidden=0, nal_ref_idc=0, nal_unit_type=12 → 0x0C。
/// 0xFF payload 不产生 00 00 序列（无防仿真问题）；type 12 不参与参考帧，
/// 对解码器与后续 P 帧引用链完全透明。
const FILLER_START: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x0C];

pub struct RateFloor {
    floor_bytes_per_sec: f64,
    last: Instant,
    credit: f64,
    pub padded_bytes: u64,
    pub padded_frames: u64,
}

impl RateFloor {
    /// `floor_kbps = 0` → 关闭（`UU_PAD_KBPS=0`）。`start` 用会话开始时刻，
    /// 首帧 dt≈0 → 无初始预算。
    pub fn new(floor_kbps: u64, start: Instant) -> Self {
        Self {
            floor_bytes_per_sec: floor_kbps as f64 * 1000.0 / 8.0,
            last: start,
            credit: 0.0,
            padded_bytes: 0,
            padded_frames: 0,
        }
    }

    /// 按真实帧间隔结算并补齐 `au`。返回追加字节数。`now` = 本帧发出时刻。
    pub fn pad(&mut self, au: &mut Vec<u8>, now: Instant) -> usize {
        if self.floor_bytes_per_sec <= 0.0 {
            return 0;
        }
        let dt = now.saturating_duration_since(self.last).as_secs_f64().min(MAX_CREDIT_SECONDS);
        self.last = now;
        self.credit += self.floor_bytes_per_sec * dt;
        self.credit -= au.len() as f64;
        // 欠债封顶 DEBT_CAP_SECONDS 的预算：大额 IDR 债要在后续帧里慢慢还，但不许无限滚。
        self.credit = self.credit.max(-self.floor_bytes_per_sec * DEBT_CAP_SECONDS);
        if self.credit <= 0.0 {
            // 欠债（IDR 尖峰）：不补，等后续帧把债还掉——官方 padding_debt 语义。
            return 0;
        }
        let need = self.credit.floor().min(MAX_PAD_BYTES as f64) as usize;
        if need < FILLER_START.len() + 1 {
            // 余额放不下一个 filler NAL：留着下次花（credit 保持余额，不清零）。
            return 0;
        }
        au.reserve(need);
        au.extend_from_slice(FILLER_START);
        au.resize(au.len() + need - FILLER_START.len(), 0xFF);
        self.credit -= need as f64;
        self.padded_bytes += need as u64;
        self.padded_frames += 1;
        need
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 返回热身之后的稳态窗口 bps（帧间隔 step_ms、每帧真实 frame_bytes）。
    fn steady_bps(kbps: u64, frame_bytes: usize, n: usize, step_ms: u64, warm: usize) -> f64 {
        let t0 = Instant::now();
        let mut f = RateFloor::new(kbps, t0);
        let step = Duration::from_millis(step_ms);
        let mut win: usize = 0;
        for i in 0..n {
            let mut au = vec![0u8; frame_bytes];
            f.pad(&mut au, t0 + step * (i as u32 + 1));
            if i >= warm {
                win += au.len();
            }
        }
        let secs = (n - warm) as f64 * step.as_secs_f64();
        win as f64 * 8.0 / secs
    }

    #[test]
    fn disabled_when_zero() {
        let t0 = Instant::now();
        let mut f = RateFloor::new(0, t0);
        let mut au = vec![0u8; 100];
        assert_eq!(f.pad(&mut au, t0 + Duration::from_millis(11)), 0);
        assert_eq!(au.len(), 100);
    }

    #[test]
    fn steady_micro_frames_converge_to_floor() {
        // 90fps 微帧 1900B、地板 2000kbps：热身 20 帧后窗口贴 2M ±6%。
        let bps = steady_bps(2000, 1900, 110, 11, 20);
        assert!((bps - 2_000_000.0).abs() < 120_000.0, "bps={bps}");
    }

    #[test]
    fn still_stage_converges_to_floor() {
        // 静止档 12fps（83ms）、真实 1000B：贴 2M ±6%。
        let bps = steady_bps(2000, 1000, 60, 83, 15);
        assert!((bps - 2_000_000.0).abs() < 120_000.0, "bps={bps}");
    }

    #[test]
    fn idr_spike_becomes_debt_and_flattens_average() {
        // 周期 IDR（340KB ≈ 实机 CQP17 整屏 IDR 形态）：spike 窗高于地板、随后窗
        // padding 被债压制（低于地板）——两侧相互抵消，多窗平均贴内容均值而不是
        // 逐窗交替。旧逐帧预算版总量恒 = 预算×fps + IDR 整坨，永远高于地板。
        let t0 = Instant::now();
        let mut f = RateFloor::new(2000, t0);
        let step = Duration::from_millis(11);
        let mut w: [usize; 4] = [0; 4];
        for i in 0..360 {
            let size = if i == 95 { 340_000 } else { 1900 };
            let mut au = vec![0u8; size];
            f.pad(&mut au, t0 + step * (i as u32 + 1));
            w[i / 90] += au.len();
        }
        let secs = 90.0 * step.as_secs_f64();
        let b: Vec<f64> = w.iter().map(|x| *x as f64 * 8.0 / secs).collect();
        let avg = b.iter().sum::<f64>() / 4.0;
        let content_avg = (359 * 1900 + 340_000) as f64 * 8.0 / (360.0 * step.as_secs_f64());
        assert!((avg - content_avg).abs() < 250_000.0, "avg={avg} content={content_avg} b={b:?}");
        assert!(b[1] > b[2], "spike 窗应高于还债窗: {b:?}");
        assert!(b[2] < 2_000_000.0, "还债窗 padding 应被压到地板以下: {b:?}");
    }

    #[test]
    fn active_content_above_floor_never_padded() {
        // 90fps × 30KB（≈21.8Mbps）远高于地板 → 全程一字节不补。
        let t0 = Instant::now();
        let mut f = RateFloor::new(2000, t0);
        let step = Duration::from_millis(11);
        let mut added_total = 0usize;
        for i in 0..90 {
            let mut au = vec![0u8; 30_000];
            added_total += f.pad(&mut au, t0 + step * (i + 1));
        }
        assert_eq!(added_total, 0);
    }

    #[test]
    fn idle_credit_capped_no_explosion() {
        // 长空闲（10s）后第一帧：credit 封顶 1 秒预算，单帧补量 ≤ MAX_PAD_BYTES。
        let t0 = Instant::now();
        let mut f = RateFloor::new(2000, t0);
        let mut au = vec![0u8; 1900];
        let added = f.pad(&mut au, t0 + Duration::from_secs(10));
        assert!(added <= MAX_PAD_BYTES, "added={added}");
    }

    #[test]
    fn filler_nal_layout() {
        let t0 = Instant::now();
        let mut f = RateFloor::new(2000, t0);
        let mut au = vec![0u8; 1900];
        let added = f.pad(&mut au, t0 + Duration::from_millis(83)); // credit≈20KB
        assert!(added > 10_000);
        assert_eq!(&au[1900..1905], FILLER_START);
        assert!(au[1905..].iter().all(|&b| b == 0xFF));
    }
}
