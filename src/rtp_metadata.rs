//! UU 私有 + 标准时序 RTP 头扩展（官方被控端每帧都挂）。
//!
//! UU 私有两条：video-capture-index（源序号，u16 大端）+ new-frame（1 字节布尔）。
//! 时序三条（libwebrtc 标准布局，官方 Android/Windows 被控端实测同款 extmap）：
//! abs-send-time（发送时刻 NTP 中段 24 位）、abs-capture-time（帧采集时刻：RTP 时钟 +
//! NTP 快照）、video-timing（编码起止毫秒，tag/value 对）。
//! 客户端右下角"延迟/frm"就是从这些时序还原出来的——不挂，接收端按名义帧长兜底推算，
//! 永远显示 50 ms。URI 与布局照 libstreamer.so 逆向结果。

use std::time::SystemTime;

use webrtc::rtp::extension::abs_send_time_extension::unix2ntp;
use webrtc::rtp::extension::HeaderExtension;
use webrtc::util::{Marshal, MarshalSize};

pub const CAPTURE_INDEX_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index";
pub const NEW_FRAME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame";
pub const ABS_SEND_TIME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time";
pub const ABS_CAPTURE_TIME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";

#[derive(Debug)]
struct NewFrame(pub bool);

impl MarshalSize for NewFrame {
    fn marshal_size(&self) -> usize {
        1
    }
}

impl Marshal for NewFrame {
    fn marshal_to(&self, buffer: &mut [u8]) -> webrtc::util::Result<usize> {
        let byte = buffer
            .first_mut()
            .ok_or_else(|| webrtc::util::Error::Other("New frame buffer too short".into()))?;
        *byte = u8::from(self.0);
        Ok(1)
    }
}

#[derive(Debug)]
struct CaptureIndex(pub u16);

impl MarshalSize for CaptureIndex {
    fn marshal_size(&self) -> usize {
        2
    }
}

impl Marshal for CaptureIndex {
    fn marshal_to(&self, buffer: &mut [u8]) -> webrtc::util::Result<usize> {
        let target = buffer
            .get_mut(..2)
            .ok_or_else(|| webrtc::util::Error::Other("Capture index buffer too short".into()))?;
        target.copy_from_slice(&self.0.to_be_bytes());
        Ok(2)
    }
}

/// abs-capture-time（官方 spec 布局，缩短版）：8 字节大端 NTP 时间戳（UQ32.32），
/// 与 RTCP 发送者报告的 NTP 同时钟。接收端拿"收包 NTP − 采集 NTP"就是全链路延迟——
/// 这正是右下角 frm/延迟的正式口径。不带 8 字节时钟偏移扩展（采集端=发送端，偏移恒 0）。
#[derive(Debug)]
struct AbsCaptureTime {
    capture_ntp: u64,
}

impl MarshalSize for AbsCaptureTime {
    fn marshal_size(&self) -> usize {
        8
    }
}

impl Marshal for AbsCaptureTime {
    fn marshal_to(&self, buffer: &mut [u8]) -> webrtc::util::Result<usize> {
        let target = buffer
            .get_mut(..8)
            .ok_or_else(|| webrtc::util::Error::Other("Abs capture time buffer too short".into()))?;
        target.copy_from_slice(&self.capture_ntp.to_be_bytes());
        Ok(8)
    }
}

/// 主采集源序号固定 0（本实现只有一个屏幕源）。
pub fn capture_index(source: u16) -> HeaderExtension {
    HeaderExtension::Custom {
        uri: CAPTURE_INDEX_URI.into(),
        extension: Box::new(CaptureIndex(source)),
    }
}

/// 参照实现每帧都报 new_frame=true（恒发帧，没有重复帧一说）。
pub fn new_frame() -> HeaderExtension {
    HeaderExtension::Custom {
        uri: NEW_FRAME_URI.into(),
        extension: Box::new(NewFrame(true)),
    }
}

/// 发送时刻（webrtc-rs 内置布局：NTP 右移 14 位取中 24 位）。
pub fn abs_send_time(now: SystemTime) -> HeaderExtension {
    HeaderExtension::AbsSendTime(webrtc::rtp::extension::abs_send_time_extension::AbsSendTimeExtension::new(now))
}

pub const VIDEO_TIMING_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-timing";

/// libwebrtc video-timing 布局：1 字节标志（0x01=定时挂载）+ 6×16-bit 大端毫秒
/// 增量（相对帧采集时刻）：encode 开始 / encode 结束 / 打包完成 / 末包离开发送队列
/// / 保留 / 保留。官方被控端每帧都带；客户端的"延迟/frm"把它当发送侧管线耗时加进
/// 全链路统计——没有它，接收端只能按兜底常量 50 ms 报发送侧延迟（实机观察：
/// UDP LAN 零丢包仍恒显 50 ms，加 abs-send-time 也不变，只有 video-timing 才是正源）。
#[derive(Debug)]
struct VideoTiming {
    encode_start_ms: u16,
    encode_finish_ms: u16,
    packetization_ms: u16,
    pacer_ms: u16,
}

impl MarshalSize for VideoTiming {
    fn marshal_size(&self) -> usize {
        13
    }
}

impl Marshal for VideoTiming {
    fn marshal_to(&self, buffer: &mut [u8]) -> webrtc::util::Result<usize> {
        let target = buffer
            .get_mut(..13)
            .ok_or_else(|| webrtc::util::Error::Other("Video timing buffer too short".into()))?;
        target[0] = 0x01;
        target[1..3].copy_from_slice(&self.encode_start_ms.to_be_bytes());
        target[3..5].copy_from_slice(&self.encode_finish_ms.to_be_bytes());
        target[5..7].copy_from_slice(&self.packetization_ms.to_be_bytes());
        target[7..9].copy_from_slice(&self.pacer_ms.to_be_bytes());
        target[9..13].fill(0);
        Ok(13)
    }
}

/// 发送侧管线耗时上报（毫秒，饱和到 u16）。采集锚点 = AU 出编码器时刻，
/// encode 段算 0（AU 到手时已编完），排队 + 配速 + 打包算 packetization/pacer。
pub fn video_timing(pipeline_ms: u64) -> HeaderExtension {
    let ms = pipeline_ms.min(u16::MAX as u64) as u16;
    HeaderExtension::Custom {
        uri: VIDEO_TIMING_URI.into(),
        extension: Box::new(VideoTiming {
            encode_start_ms: 0,
            encode_finish_ms: 0,
            packetization_ms: ms,
            pacer_ms: ms,
        }),
    }
}

/// 帧采集时刻：墙钟 NTP（与 abs-send-time 和 RTCP SR 共用同一时钟）。本管线里
/// 采集≈写样本瞬间（编码耗时相对 LAN 延迟可忽略），与 Sample.timestamp 用同一个
/// `now`，保证接收端算出的分段自洽。
pub fn abs_capture_time(now: SystemTime) -> HeaderExtension {
    HeaderExtension::Custom {
        uri: ABS_CAPTURE_TIME_URI.into(),
        extension: Box::new(AbsCaptureTime { capture_ntp: unix2ntp(now) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn capture_index_is_two_byte_be() {
        let mut buf = [0u8; 4];
        assert_eq!(CaptureIndex(0x1234).marshal_to(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], &[0x12, 0x34]);
    }

    #[test]
    fn new_frame_is_one_byte_boolean() {
        let mut buf = [0u8; 2];
        assert_eq!(NewFrame(true).marshal_to(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], 1);
    }

    #[test]
    fn abs_capture_time_is_eight_byte_be_ntp() {
        // spec 缩短版：8 字节大端 NTP（UQ32.32），无标志字。unix→ntp 偏移 2208988800s。
        let t = UNIX_EPOCH + Duration::from_secs(1_000);
        let ext = abs_capture_time(t);
        let HeaderExtension::Custom { extension, .. } = &ext else { panic!("shape") };
        assert_eq!(extension.marshal_size(), 8);
        let mut buf = [0u8; 8];
        assert_eq!(extension.marshal_to(&mut buf).unwrap(), 8);
        let ntp = ((1_000 + 2_208_988_800u64) << 32) as u64;
        assert_eq!(buf, ntp.to_be_bytes());
    }

    #[test]
    fn abs_capture_time_buffer_too_short_fails() {
        let HeaderExtension::Custom { extension, .. } = abs_capture_time(SystemTime::now()) else { panic!("shape") };
        assert!(extension.marshal_to(&mut [0u8; 7]).is_err());
    }

    #[test]
    fn video_timing_is_flag_plus_six_be_u16() {
        // 官方 spec：1 字节标志 + 6×16-bit 大端增量，共 13 字节。
        let HeaderExtension::Custom { extension, .. } = video_timing(90) else { panic!("shape") };
        assert_eq!(extension.marshal_size(), 13);
        let mut buf = [0xFFu8; 13];
        assert_eq!(extension.marshal_to(&mut buf).unwrap(), 13);
        assert_eq!(buf[0], 0x01);
        assert_eq!(&buf[1..3], &[0x00, 0x00]);
        assert_eq!(&buf[3..5], &[0x00, 0x00]);
        assert_eq!(&buf[5..7], &[0x00, 0x5a]);
        assert_eq!(&buf[7..9], &[0x00, 0x5a]);
        assert_eq!(&buf[9..13], &[0, 0, 0, 0]);
    }

    #[test]
    fn video_timing_saturates_at_u16_max() {
        let HeaderExtension::Custom { extension, .. } = video_timing(100_000) else { panic!("shape") };
        let mut buf = [0u8; 13];
        extension.marshal_to(&mut buf).unwrap();
        assert_eq!(&buf[5..7], &[0xFF, 0xFF]);
    }

    #[test]
    fn video_timing_buffer_too_short_fails() {
        let HeaderExtension::Custom { extension, .. } = video_timing(1) else { panic!("shape") };
        assert!(extension.marshal_to(&mut [0u8; 12]).is_err());
    }

    #[test]
    fn extension_uris_are_canonical() {
        assert_eq!(capture_index(0).uri(), CAPTURE_INDEX_URI);
        assert_eq!(new_frame().uri(), NEW_FRAME_URI);
        assert_eq!(abs_send_time(SystemTime::now()).uri(), ABS_SEND_TIME_URI);
        assert_eq!(abs_capture_time(SystemTime::now()).uri(), ABS_CAPTURE_TIME_URI);
        assert_eq!(video_timing(0).uri(), VIDEO_TIMING_URI);
    }
}
