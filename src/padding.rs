//! 发送端带宽地板 padding —— 对齐官方引擎的 ALR padding（credit/debt 模型）。
//!
//! REA 逆向 `streamer.dll` 实锤（references/official-padding-bandwidth.md）：官方引擎内嵌
//! libwebrtc 发包栈，`bitrate_allocator.cc` 给视频一条恒定分配目标，编码器输出不足时由
//! `PacketRouter::GeneratePadding` 补满到线上。官方日志字段 `padding_budget_kbps` /
//! `padding_debt` 说明它是**按时间累计的字节预算**，不是逐帧补齐——IDR 大帧吃空预算后，
//! 后续小帧补得少，长时间窗流量恒定。
//!
//! 踩过的两个坑（2026-10-31 实机）：
//! 1. 逐帧预算版：每帧都补满「帧时长×地板」，周期 IDR 又整坨叠在窗口上 → 总量仍逐窗交替。
//!    正解 = 速率积分器（下面的 credit/debt）。
//! 2. **载体选错**：`rtp-0.10.0` 的 `H264Payloader::emit` 对 `AUD_NALU_TYPE(9)` 与
//!    `FILLER_NALU_TYPE(12)` 直接 `return` ——补 Filler Data NAL 时**线上零增量**
//!    （宿主账面 2000kbps、wlo1 实测 381kbps、客户端显示 176kbps）。改用
//!    **SEI NAL(type 6) + unregistered payload_type(256)**：打包器透传、解码器按规范跳过
//!    payload。只加字节不加帧数 → **帧率完全不动**。
//!
//! 模型：
//! - 每帧按真实间隔把 `credit += floor × dt`；
//! - 内容字节花 credit；credit > 0 就补 SEI 把差额花光（单帧封顶防爆）；
//! - IDR 尖峰把 credit 打成负数（debt）→ 之后自动少补/不补 → 窗口均值贴地板。

use std::time::Instant;

/// 单帧 padding 字节上限：credit 大额积压（长空闲后）一帧吃不完的下次继续花。
pub const MAX_PAD_BYTES: usize = 64 * 1024;

/// credit 积压上限（秒数）：空闲攒的预算最多抵这么久，防止长空闲后一坨暴补。
const MAX_CREDIT_SECONDS: f64 = 1.0;

/// 欠债上限（秒数）：IDR 尖峰的债最多记这么久，超出部分的尖峰不指望后续帧抹平。
const DEBT_CAP_SECONDS: f64 = 2.0;

/// SEI NAL 头：forbidden=0, nal_ref_idc=0, nal_unit_type=6 → 0x06。
/// `H264Payloader::emit` 只丢 AUD/Filler，type 6 透传；解码器跳过 payload。
const SEI_START: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x06];

/// SEI 头部：起始码 + NAL 头 + payload_type 扩展（0xFF 0x01 → type 256，私有/未注册）。
const SEI_HEADER_LEN: usize = 5 + 2;

/// 构造一个恰好 `total` 字节（含起始码）的 SEI NAL，payload 全 0xFF。
///
/// SEI 语法里 `payload_size` 是「连续 0xFF 累加 + 末尾一个 <255 的字节」的扩展编码；
/// 编码长度 k 本身又依赖 payload 长度（pl 满足 255(k-1) < pl ≤ 255k-1），所以递增 k
/// 直到自洽（几千字节也就几十次循环）。返回 false = `total` 放不下。
fn push_sei(au: &mut Vec<u8>, total: usize) -> bool {
    let mut k = 1usize;
    let payload_len;
    loop {
        if total < SEI_HEADER_LEN + k + 1 || k > MAX_PAD_BYTES {
            return false;
        }
        let pl = total - SEI_HEADER_LEN - k;
        if pl > 255 * (k - 1) && pl <= 255 * k - 1 {
            payload_len = pl;
            break;
        }
        k += 1;
    }
    au.reserve(total);
    au.extend_from_slice(SEI_START);
    au.extend_from_slice(&[0xFF, 0x01]);
    for _ in 0..k - 1 {
        au.push(0xFF);
    }
    au.push((payload_len - 255 * (k - 1)) as u8);
    au.resize(au.len() + payload_len, 0xFF);
    true
}

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
        let before = au.len();
        if !push_sei(au, need) {
            // 余额放不下一个 SEI NAL：留着下次花（credit 保持余额，不清零）。
            return 0;
        }
        let added = au.len() - before;
        self.credit -= added as f64;
        self.padded_bytes += added as u64;
        self.padded_frames += 1;
        added
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
            let mut au = content(frame_bytes);
            f.pad(&mut au, t0 + step * (i as u32 + 1));
            if i >= warm {
                win += au.len();
            }
        }
        let secs = (n - warm) as f64 * step.as_secs_f64();
        win as f64 * 8.0 / secs
    }

    /// 按 Annex-B 起始码切 NAL，返回 (nal_unit_type, 含起始码长度)。
    fn nalus(au: &[u8]) -> Vec<(u8, usize)> {
        let mut heads: Vec<(usize, usize)> = Vec::new(); // (起始码起点, NAL 数据起点)
        let mut i = 0usize;
        while i + 2 < au.len() {
            if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
                let mut s = i;
                while s > 0 && au[s - 1] == 0 {
                    s -= 1; // 4 字节起始码：把前导 0 也算进来
                }
                heads.push((s, i + 3));
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut out = Vec::new();
        for (idx, (s, d)) in heads.iter().enumerate() {
            let end = heads.get(idx + 1).map(|h| h.0).unwrap_or(au.len());
            out.push((au[*d] & 0x1F, end - s));
        }
        out
    }

    /// 测试用的假内容：一个合法 slice NAL（type 1），payload 全 0x11（不会误造起始码）。
    fn content(n: usize) -> Vec<u8> {
        let mut v = vec![0x11u8; n];
        if n >= 5 {
            v[0] = 0;
            v[1] = 0;
            v[2] = 0;
            v[3] = 1;
            v[4] = 0x41;
        }
        v
    }

    /// 解 SEI 的 payload_size 扩展编码，返回 (payload 起点, payload 长度)。
    fn sei_payload(au: &[u8], nal_start: usize) -> (usize, usize) {
        let mut i = nal_start + SEI_HEADER_LEN;
        let mut size = 0usize;
        loop {
            let b = au[i];
            i += 1;
            size += b as usize;
            if b != 0xFF {
                break;
            }
        }
        (i, size)
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
        // 周期 IDR（340KB ≈ 实机整屏 IDR 形态）：spike 窗高于地板、随后窗 padding 被债
        // 压制（低于地板）→ 两侧抵消，多窗平均贴内容均值而不是逐窗交替。
        let t0 = Instant::now();
        let mut f = RateFloor::new(2000, t0);
        let step = Duration::from_millis(11);
        let mut w: [usize; 4] = [0; 4];
        for i in 0..360 {
            let size = if i == 95 { 340_000 } else { 1900 };
            let mut au = content(size);
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
            let mut au = content(30_000);
            added_total += f.pad(&mut au, t0 + step * (i + 1));
        }
        assert_eq!(added_total, 0);
    }

    #[test]
    fn idle_credit_capped_no_explosion() {
        // 长空闲（10s）后第一帧：credit 封顶 1 秒预算，单帧补量 ≤ MAX_PAD_BYTES。
        let t0 = Instant::now();
        let mut f = RateFloor::new(2000, t0);
        let mut au = content(1900);
        let added = f.pad(&mut au, t0 + Duration::from_secs(10));
        assert!(added <= MAX_PAD_BYTES, "added={added}");
    }

    #[test]
    fn padding_carrier_is_sei_not_filler() {
        // 关键回归：载体必须是打包器透传的 SEI(6)，不能是被丢掉的 Filler(12)/AUD(9)。
        let t0 = Instant::now();
        let mut f = RateFloor::new(2000, t0);
        let mut au = content(1900);
        let added = f.pad(&mut au, t0 + Duration::from_millis(83)); // credit≈20KB
        assert!(added > 10_000, "added={added}");
        let nal = nalus(&au);
        assert_eq!(nal.len(), 2, "内容 NAL + 一个 padding NAL: {nal:?}");
        assert_eq!(nal[1].0, 6, "padding NAL 必须是 SEI");
        assert_eq!(nal[1].1, added, "NAL 长度应等于追加字节数");
        assert_eq!(&au[1900..1905], SEI_START);
        assert_eq!(&au[1905..1907], &[0xFF, 0x01], "payload_type=256 扩展编码");
        let (off, size) = sei_payload(&au, 1900);
        assert_eq!(size, au.len() - off, "payload_size 与 payload 长度不符");
        assert!(au[off..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn sei_size_encoding_covers_range() {
        for total in [9usize, 10, 260, 261, 511, 512, 5100, MAX_PAD_BYTES] {
            let mut au = Vec::new();
            assert!(push_sei(&mut au, total), "total={total} 放不下");
            assert_eq!(au.len(), total, "total={total} 实际 {}", au.len());
            let nal = nalus(&au);
            assert_eq!(nal.len(), 1, "total={total} 切出 {nal:?}");
            assert_eq!(nal[0].0, 6);
            let (off, size) = sei_payload(&au, 0);
            assert_eq!(size, au.len() - off, "total={total} size 编码不自洽");
        }
    }
}
