//! UU 私有 RTP 头扩展（官方被控端每帧都挂）：video-capture-index（源序号，u16 大端）
//! + new-frame（1 字节布尔）。
//!
//! 客户端的延迟/frm 统计靠这两条扩展还原真实出帧信息；不挂的话接收端按名义帧长
//! 推算，右下角永远显示 50 ms。URI 与布局照参照实现（官方库实测验证）。

use webrtc::rtp::extension::HeaderExtension;
use webrtc::util::{Marshal, MarshalSize};

pub const CAPTURE_INDEX_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index";
pub const NEW_FRAME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame";

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
