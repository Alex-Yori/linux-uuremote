//! 编码层：出帧计划（纯逻辑）+ BGRA→I420 + openh264。
//!
//! 管线（对齐参照实现的实测行为，见 SPEC §8）：
//!   整屏 BGRA（XShm 读回）→ 逐块比对出脏框（`diff`）→ 门控（`gating`：动静分档）
//!   → **门控决定发不发**；发的时候整屏喂 openh264，出 Annex-B 访问单元交给 WebRTC。
//!
//! 为什么整屏喂：实测静止档只有 3 fps（见 `gating`），静止时喂的是同一帧像素，
//! 出的 P 帧近乎空——官方「静止 3 fps / 231 Kbps」就是这么来的。脏区裁子帧编是
//! 下一步的省法，先把整帧链路跑通并且量出来。
//!
//! BGRA→I420 自己写：openh264 的 `write_yuv_by_pixel` 是逐像素 f32 通用实现，
//! 1440p 撑不起帧率。这里用整数 BT.601 + 按行分带多线程，可确定性单测。
//!
//! 关键帧：首帧、客户端接入、PLI/FIR、编码失败后一律 IDR；周期走 `intra_frame_period`。

use std::time::Duration;

/// 一次出帧的决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodePlan {
    /// 本帧是否编码发出（false = 静止被门控吃掉）。
    pub emit: bool,
    /// 强制 IDR。
    pub force_idr: bool,
    /// 喂 WebRTC `Sample` 的时长。
    pub sample_duration: Duration,
}

/// 出帧计划器：把门控结果翻译成编码动作，并管关键帧与后端放弃。
///
/// - 首帧与客户端接入（`note_subscribed`）、PLI/FIR（`note_keyframe_request`）→ 下一帧 IDR。
///   新订阅的解码器除了关键帧什么都解不了，所以构造时就要求 IDR。
/// - 连续失败 `max_failures` 次 → `given_up()`，上层换后端（NVENC → VAAPI → openh264）。
#[derive(Debug)]
pub struct FramePlanner {
    still_fps: u32,
    moving_fps: u32,
    want_idr: bool,
    failures: u32,
    max_failures: u32,
    give_up: bool,
}

impl FramePlanner {
    pub fn new(moving_fps: u32, still_fps: u32) -> Self {
        Self {
            still_fps,
            moving_fps: moving_fps.max(1),
            want_idr: true,
            failures: 0,
            max_failures: 5,
            give_up: false,
        }
    }

    /// 客户端接入（be-controlled / 新订阅）。
    pub fn note_subscribed(&mut self) {
        self.want_idr = true;
        self.failures = 0;
        self.give_up = false;
    }

    /// RTCP PLI / FIR：官方宿主对 PLI 的回法就是下一帧 IDR。
    pub fn note_keyframe_request(&mut self) {
        self.want_idr = true;
    }

    pub fn note_failure(&mut self) {
        self.failures += 1;
        self.want_idr = true;
        if self.failures >= self.max_failures {
            self.give_up = true;
        }
    }

    pub fn note_success(&mut self) {
        self.failures = 0;
    }

    pub fn given_up(&self) -> bool {
        self.give_up
    }

    /// 每 tick 调一次；`moving` = 内容门控判定画面在变。
    pub fn plan(&mut self, moving: bool) -> EncodePlan {
        if self.give_up {
            return EncodePlan { emit: false, force_idr: false, sample_duration: Duration::ZERO };
        }
        let fps = if moving { self.moving_fps } else { self.still_fps };
        if fps == 0 {
            return EncodePlan { emit: false, force_idr: false, sample_duration: Duration::ZERO };
        }
        let force = self.want_idr;
        self.want_idr = false;
        EncodePlan { emit: true, force_idr: force, sample_duration: Duration::from_secs_f64(1.0 / f64::from(fps)) }
    }
}

/// 码率粗算：每像素比特 × 像素数 × 帧率，下限 100 kbps。
///
/// 默认 0.10 bpp 是桌面内容 + openh264 屏幕模式的经验值；参照实现的码率随
/// 分辨率/帧率走同样形状，具体系数等真机码率对齐后再调。
pub fn default_bitrate_kbps(width: usize, height: usize, fps: u32, bits_per_pixel: f64) -> u32 {
    let bps = width as f64 * height as f64 * f64::from(fps) * bits_per_pixel;
    (bps / 1000.0).round().max(100.0) as u32
}

/// BGRA（X 的 32bpp packed，小端 B 在前）→ I420，原地写入 `dst`。
///
/// `dst` 长必须 `3*w*h/2`（Y + U + V）；`threads` 按行分带，带高取 2 的倍数，
/// 这样色度带与亮度带一一对应，多线程结果与串行逐位相同。
pub fn bgra_to_i420(src: &[u8], width: usize, height: usize, dst: &mut [u8], threads: usize) {
    assert_eq!(dst.len(), width * height * 3 / 2, "I420 目标缓冲尺寸不对");
    assert_eq!(width % 2, 0);
    assert_eq!(height % 2, 0);
    let plane = width * height;
    let chroma = plane / 4;
    let (yp, rest) = dst.split_at_mut(plane);
    let (up, vp) = rest.split_at_mut(chroma);

    let rows_per_band = (((height + threads.max(1) - 1) / threads.max(1)).max(2)) & !1usize;
    let y_chunks = yp.chunks_mut(rows_per_band * width);
    let u_chunks = up.chunks_mut(rows_per_band / 2 * (width / 2));
    let v_chunks = vp.chunks_mut(rows_per_band / 2 * (width / 2));

    std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        for (band, ((yc, uc), vc)) in y_chunks.zip(u_chunks).zip(v_chunks).enumerate() {
            let base_row = band * rows_per_band;
            jobs.push(scope.spawn(move || convert_band(src, width, height, base_row, yc, uc, vc)));
        }
        for j in jobs {
            j.join().expect("色彩转换线程崩了");
        }
    });
}

/// 一条带：亮度行 `base_row..base_row + yc.len()/width`，色度行随之。
fn convert_band(src: &[u8], width: usize, height: usize, base_row: usize, yp: &mut [u8], up: &mut [u8], vp: &mut [u8]) {
    let rows = yp.len() / width;
    let cw = width / 2;
    for r in 0..rows {
        let row = base_row + r;
        let s0 = &src[row * width * 4..row * width * 4 + width * 4];
        let y = &mut yp[r * width..r * width + width];
        for x in 0..width {
            let b = s0[x * 4] as i32;
            let g = s0[x * 4 + 1] as i32;
            let red = s0[x * 4 + 2] as i32;
            // BT.601 整数式，+32768 做四舍五入。
            y[x] = ((19595 * red + 38470 * g + 7485 * b + 32768) >> 16) as u8;
        }
        // 色度只在带内偶数行算（带高与起点都是偶数）。
        if row % 2 == 0 && row + 1 < height {
            let s1 = &src[(row + 1) * width * 4..(row + 1) * width * 4 + width * 4];
            let cy = (row - base_row) / 2;
            for c in 0..cw {
                let i = 2 * c;
                let mut bs = 0i32;
                let mut gs = 0i32;
                let mut rs = 0i32;
                for s in [s0, s1] {
                    bs += s[i * 4] as i32 + s[i * 4 + 4] as i32;
                    gs += s[i * 4 + 1] as i32 + s[i * 4 + 5] as i32;
                    rs += s[i * 4 + 2] as i32 + s[i * 4 + 6] as i32;
                }
                let b = (bs + 2) >> 2;
                let g = (gs + 2) >> 2;
                let red = (rs + 2) >> 2;
                // 满范围 RGB（0..255）喂进视频域的矩阵时端点会溢出（纯蓝 U = 256），
                // 所以必须饱和裁剪——libyuv/swscale 同样如此。
                let u = (((-11056 * red - 21712 * g + 32768 * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                let v = (((32768 * red - 27054 * g - 5643 * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                up[cy * cw + c] = u;
                vp[cy * cw + c] = v;
            }
        }
    }
}

/// 编出来的一帧是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// 关键帧（带 SPS/PPS）。
    Idr,
    /// 差分帧。
    Delta,
    /// 编码器自己跳过的帧（我们关了 skip_frames，正常不该出现）。
    Skipped,
}

/// openh264 封装：整屏 BGRA 进（内部转 I420、缓冲复用），Annex-B 访问单元出。
pub struct OpenH264 {
    inner: openh264::encoder::Encoder,
    width: usize,
    height: usize,
    i420: Vec<u8>,
    threads: usize,
}

impl OpenH264 {
    /// `bitrate_kbps` 0 = 按 `bitrate_kbps(w,h,fps,0.10)` 粗算。宽高必须偶数（4:2:0）。
    pub fn new(width: usize, height: usize, fps: u32, bitrate_kbps: u32, gop_frames: u32, threads: usize) -> Result<Self, String> {
        if width % 2 != 0 || height % 2 != 0 {
            return Err(format!("编码尺寸必须偶数对齐（收到 {width}x{height}）"));
        }
        let threads = threads.max(1);
        let kbps = if bitrate_kbps > 0 { bitrate_kbps } else { default_bitrate_kbps(width, height, fps, 0.10) };
        let cfg = openh264::encoder::EncoderConfig::new()
            .bitrate(openh264::encoder::BitRate::from_bps(kbps * 1000))
            .rate_control_mode(openh264::encoder::RateControlMode::Bitrate)
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(fps.max(1) as f32))
            // 屏幕内容模式：文字与边缘优先，配自适应量化。
            .usage_type(openh264::encoder::UsageType::ScreenContentRealTime)
            .complexity(openh264::encoder::Complexity::Medium)
            .adaptive_quantization(true)
            // WebRTC 的时长连续性不能容忍编码器自行丢帧。
            .skip_frames(false)
            // 桌面内容没有「镜头切换」；开着会毫无理由地插 IDR。
            .scene_change_detect(false)
            .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(gop_frames.max(1)))
            // 每个 IDR 换 SPS/PPS id：解码器能认出「中间丢了帧、来了新关键帧」。
            .sps_pps_strategy(openh264::encoder::SpsPpsStrategy::IncreasingId)
            .num_threads(threads.min(16) as u16);
        let inner = openh264::encoder::Encoder::with_api_config(openh264::OpenH264API::from_source(), cfg)
            .map_err(|e| format!("openh264 初始化失败: {e}"))?;
        Ok(Self { inner, width, height, i420: vec![0u8; width * height * 3 / 2], threads })
    }

    /// 下一帧强制 IDR。
    pub fn force_idr(&mut self) {
        self.inner.force_intra_frame();
    }

    /// 编一帧整屏 BGRA，返回 Annex-B 字节与帧类型（IDR 自带 SPS/PPS）。
    pub fn encode_bgra(&mut self, bgra: &[u8], force_idr: bool) -> Result<(Vec<u8>, FrameKind), String> {
        if bgra.len() < self.width * self.height * 4 {
            return Err(format!("输入太小（{} < {}）", bgra.len(), self.width * self.height * 4));
        }
        bgra_to_i420(bgra, self.width, self.height, &mut self.i420, self.threads);
        if force_idr {
            self.force_idr();
        }
        let plane = self.width * self.height;
        let chroma = plane / 4;
        let source = openh264::formats::YUVSlices::new(
            (&self.i420[..plane], &self.i420[plane..plane + chroma], &self.i420[plane + chroma..plane + 2 * chroma]),
            (self.width, self.height),
            (self.width, self.width / 2, self.width / 2),
        );
        let bs = self.inner.encode(&source).map_err(|e| format!("openh264 encode 失败: {e}"))?;
        let kind = match bs.frame_type() {
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I => FrameKind::Idr,
            openh264::encoder::FrameType::P => FrameKind::Delta,
            _ => FrameKind::Skipped,
        };
        Ok((bs.to_vec(), kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(b: u8, g: u8, r: u8, w: usize, h: usize) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for px in v.chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        v
    }

    #[test]
    fn plan_starts_with_idr_then_follows_the_gate() {
        let mut p = FramePlanner::new(30, 3);
        let first = p.plan(true);
        assert!(first.emit && first.force_idr);
        assert!(!p.plan(true).force_idr);
        let still = p.plan(false);
        assert!(still.emit && !still.force_idr);
        assert_eq!(still.sample_duration.as_millis(), 333);
    }

    #[test]
    fn keyframe_request_and_reconnect_force_idr() {
        let mut p = FramePlanner::new(60, 3);
        assert!(p.plan(true).force_idr);
        assert!(!p.plan(true).force_idr);
        p.note_keyframe_request();
        assert!(p.plan(true).force_idr);
        p.note_subscribed();
        assert!(p.plan(true).force_idr);
    }

    #[test]
    fn still_zero_emits_nothing() {
        let mut p = FramePlanner::new(30, 0);
        assert!(!p.plan(false).emit);
        assert!(p.plan(true).emit);
    }

    #[test]
    fn repeated_failures_give_up_and_reconnect_revives() {
        let mut p = FramePlanner::new(30, 3);
        for _ in 0..4 {
            p.note_failure();
            assert!(!p.given_up() && p.plan(true).force_idr);
        }
        p.note_failure();
        assert!(p.given_up());
        assert!(!p.plan(true).emit);
        p.note_subscribed();
        assert!(!p.given_up());
        assert!(p.plan(true).emit);
    }

    #[test]
    fn failure_count_clears_only_on_success() {
        let mut p = FramePlanner::new(30, 3);
        p.note_failure();
        p.note_failure();
        p.note_success();
        for _ in 0..4 {
            p.note_failure();
            assert!(!p.given_up());
        }
        p.note_failure();
        assert!(p.given_up());
    }

    #[test]
    fn bitrate_scales_with_pixels_and_fps() {
        assert_eq!(default_bitrate_kbps(1920, 1080, 30, 0.10), 6221);
        assert_eq!(default_bitrate_kbps(2560, 1440, 60, 0.10), 22118);
        assert_eq!(default_bitrate_kbps(10, 10, 1, 0.10), 100, "下限 100 kbps");
    }

    #[test]
    fn conversion_matches_known_colors() {
        let (w, h) = (8, 4);
        let mut dst = vec![0u8; w * h * 3 / 2];

        bgra_to_i420(&solid(0, 0, 0, w, h), w, h, &mut dst, 1);
        assert!(dst[..w * h].iter().all(|&y| y <= 16), "黑 → Y 很低");
        assert!(dst[w * h..].iter().all(|&c| c == 128), "黑 → 中性色度");

        bgra_to_i420(&solid(255, 255, 255, w, h), w, h, &mut dst, 1);
        assert!(dst[..w * h].iter().all(|&y| y >= 235), "白 → Y≥235");

        bgra_to_i420(&solid(255, 0, 0, w, h), w, h, &mut dst, 1);
        let y = dst[0] as i32;
        let u = dst[w * h] as i32;
        let v = dst[w * h + w * h / 4] as i32;
        // 纯蓝：Y≈29，U≈240，V≈110（BT.601）。
        assert!((25..=33).contains(&y), "纯蓝 Y（收到 {y}）");
        assert!(u > 220, "纯蓝 U 高（收到 {u}）");
        assert!((100..=125).contains(&v), "纯蓝 V 偏低（收到 {v}）");
    }

    #[test]
    fn multithread_matches_single_thread() {
        let (w, h) = (64, 32);
        let mut src = vec![0u8; w * h * 4];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            px[0] = (i * 7) as u8;
            px[1] = (i * 13) as u8;
            px[2] = (i * 29) as u8;
            px[3] = 255;
        }
        let mut one = vec![0u8; w * h * 3 / 2];
        let mut many = vec![0u8; w * h * 3 / 2];
        bgra_to_i420(&src, w, h, &mut one, 1);
        bgra_to_i420(&src, w, h, &mut many, 8);
        assert_eq!(one, many, "分带并行必须与串行逐位一致");
    }

    #[test]
    fn odd_thread_counts_still_cover_everything() {
        // 13 线程处理 30 行：带高向上取到偶数，最后一带可能为空 chunk——必须不丢像素。
        let (w, h) = (16, 30);
        let mut src = vec![0u8; w * h * 4];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            px[2] = (i % 251) as u8;
        }
        let mut one = vec![0u8; w * h * 3 / 2];
        let mut many = vec![0u8; w * h * 3 / 2];
        bgra_to_i420(&src, w, h, &mut one, 1);
        bgra_to_i420(&src, w, h, &mut many, 13);
        assert_eq!(one, many);
    }
}
