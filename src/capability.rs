//! 连接建立后向云端上报的"设备能力"，客户端靠它决定画质菜单与采集/编码显示。
//!
//! 官方被控端在 PeerConnection 进入 Connected 后发三条信令事件：
//! `forward_setting`(device_capability)、`forward_setting`(sender_para_info)、`ice_finished_log`。
//! 形状照官方 Windows 4.37 实测：负载嵌在 `data.device_capability` 里，`type` 与它并列——
//! 摊平在顶层的话客户端不认，画质与色度会一直停在默认值。
//!
//! `video_codec_capability` 每项带自己的最大尺寸；官方查看器把 0x0 当成"不支持"，
//! 所以只列我们真能产的格式（H.264 / 4:2:0 / 8 bit），不吹 HEVC 和 4:4:4。

use serde_json::{json, Value};

pub const ICE_FINISHED_EVENT: &str = "ice_finished_log";
pub const FORWARD_SETTING_EVENT: &str = "forward_setting";
/// 官方枚举：video_codec 1 = H.264、2 = H.265；chroma_sampling 1 = 4:2:0、3 = 4:4:4。
pub const VIDEO_CODEC_H264: i32 = 1;
/// codec_impl：0 NVENC、1 AMF、2 QSV、3 VideoToolbox、4 MediaCodec、≥5 软编。
/// 5 = 软件是官方日志里的推断，本项目软编固定报 5。
pub const CODEC_IMPL_SOFTWARE: i32 = 5;
pub const CODEC_IMPL_NVENC: i32 = 0;
/// 编码能力尺寸：参照实现固定报 3840x2160（编码器的能力，不是当前屏幕的尺寸），
/// 客户端画质档位按它解锁；报实际屏幕尺寸会把菜单锁死在屏幕档（"原画：被控端不支持"）。
pub const CODEC_CAPABILITY_SIZE: (u32, u32) = (3840, 2160);

/// `forward_setting` 的 device_capability：可采集显示器 + 编码能力。
///
/// `codec_impl` 报真实后端（NVENC=0 / 软编=5）：报成软件会让客户端把"原画"档
/// 判成被控端不支持。`width`/`height` 是**编码器能力**（NVENC 固定 4K，照参照实现），
/// 屏幕清单才报实际屏幕尺寸。`hevc`/`full_chroma` 只有真能发才置真。
pub fn device_capability(
    client_id: &str,
    ice_id: &str,
    display_id: i32,
    fps: i32,
    codec_impl: i32,
    hevc: bool,
    full_chroma: bool,
    width: u32,
    height: u32,
) -> Value {
    let mut codecs = Vec::new();
    for codec in [VIDEO_CODEC_H264].into_iter().chain(hevc.then_some(2)) {
        for chroma in [1i32].into_iter().chain(full_chroma.then_some(3)) {
            codecs.push(json!({
                "video_codec": codec,
                "chroma_sampling": chroma,
                "codec_impl": codec_impl,
                "bit_depth": 8,
                "width": width,
                "height": height,
            }));
        }
    }
    json!({
        "client_id": client_id,
        "data": {
            "device_capability": {
                "display_info": [{"id": display_id, "fps": fps, "type": 0, "hdr": -1}],
                "video_codec_capability": codecs,
                "ice_id": ice_id,
            },
            "type": "device_capability",
        },
    })
}

/// `forward_setting` 的 sender_para_info：采集与编码器名字，客户端设置面板直接显示。
pub fn sender_para_info(client_id: &str, ice_id: &str, capture_impl: &str, encoder_impl: &str) -> Value {
    json!({
        "client_id": client_id,
        "data": {
            "sender_para_info": {
                "capture_impl": capture_impl,
                "encoder_impl": encoder_impl,
                "ice_id": ice_id,
                "sender_media_infos": [{
                    "capture_impl": capture_impl,
                    "encoder_impl": encoder_impl,
                    "video_track_index": 0,
                }],
                "video_track_index": 0,
            },
            "type": "sender_para_info",
        },
    })
}

/// `ice_finished_log`：官方 telemetry，不是 ACK。缺 app_control_id 就不发。
pub fn ice_finished(ice_id: &str, app_control_id: &str) -> Value {
    json!({"ice_id": ice_id, "app_control_id": app_control_id})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_uses_nested_official_shape_not_flat() {
        let payload = device_capability("client-example", "ice-example", 0, 30, CODEC_IMPL_NVENC, false, false, 3840, 2160);
        assert_eq!(payload["data"]["type"], "device_capability");
        // 顶层不能有 type：摊平形状官方客户端不认（会把画质锁在默认值）。
        assert!(payload.get("type").is_none());
        let caps = payload["data"]["device_capability"]["video_codec_capability"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(caps.len(), 1);
        assert_eq!(
            caps[0],
            json!({"video_codec": 1, "chroma_sampling": 1, "codec_impl": 0, "bit_depth": 8, "width": 3840, "height": 2160})
        );
        assert_eq!(payload["data"]["device_capability"]["display_info"][0]["hdr"], -1);
        assert_eq!(payload["data"]["device_capability"]["ice_id"], "ice-example");
        // HEVC + 真彩叠加出 4 项（照参照实现的笛卡尔积）；没实现的能力绝不虚报。
        let wide = device_capability("c", "i", 0, 60, CODEC_IMPL_NVENC, true, true, 3840, 2160);
        assert_eq!(wide["data"]["device_capability"]["video_codec_capability"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn sender_para_names_the_real_backends() {
        let payload = sender_para_info("c", "i", "X11", "OpenH264");
        assert_eq!(payload["data"]["type"], "sender_para_info");
        let inner = &payload["data"]["sender_para_info"];
        assert_eq!(inner["capture_impl"], "X11");
        assert_eq!(inner["encoder_impl"], "OpenH264");
        assert_eq!(inner["video_track_index"], 0);
        assert_eq!(inner["sender_media_infos"][0]["encoder_impl"], "OpenH264");
    }

    #[test]
    fn ice_finished_carries_only_the_two_official_fields() {
        assert_eq!(ice_finished("i", "a"), json!({"ice_id": "i", "app_control_id": "a"}));
    }
}
