//! 连接建立后向云端上报的"设备能力"，客户端靠它决定画质菜单与采集/编码显示。
//!
//! 官方被控端在 PeerConnection 进入 Connected 后发三条信令事件：
//! `forward_setting`(device_capability)、`forward_setting`(sender_para_info)、`ice_finished_log`。
//! 形状照官方 Windows 4.37 实测：负载嵌在 `data.device_capability` 里，`type` 与它并列——
//! 摊平在顶层的话客户端不认，画质与色度会一直停在默认值。
//!
//! `video_codec_capability` 每项带自己的最大尺寸；官方查看器把 0x0 当成"不支持"，
//! 所以只列我们真能产的格式，不吹没实现的能力。
//!
//! Windows 控制端的 `negotiation_helper`（二进制里的 negotiation_helper.cpp）按两个
//! section 解析这份能力：**frame_quality 条目必须有 `max_frame_quality`/`bit_depth`/
//! `video_codec`/`max_width`/`max_height`，display 条目必须有 `fps`/`hdr`**——缺任一项
//! 该条目被 `reason=missing_field` 整条丢弃，codec_count 归零后客户端判定"被控端不支持"
//! （原画与真彩菜单全锁、指定帧率被拒）。所以每条目同时带 `width/height`（旧形状）与
//! `max_width/max_height/max_frame_quality`（Windows 契约），并额外给出
//! `frame_quality`/`display` 别名数组，两代客户端都解得动。

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
/// 我们能接受的最高画质档。官方 VideoQuality：0 未知/1 Fast/2 General/3 HD/4 Bluray/5 Auto/6 Custom；
/// 声明自定义码率（ff_capture_setting=4）就可以到 6。
pub const MAX_FRAME_QUALITY: i32 = 6;

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
                // Windows negotiation_helper 的必需键（缺了整条被丢）。
                "max_width": width,
                "max_height": height,
                "max_frame_quality": MAX_FRAME_QUALITY,
            }));
        }
    }
    let displays = json!([{"id": display_id, "fps": fps, "type": 0, "hdr": -1, "width": width, "height": height}]);
    json!({
        "client_id": client_id,
        "data": {
            "device_capability": {
                "display_info": [{"id": display_id, "fps": fps, "type": 0, "hdr": -1, "width": width, "height": height}],
                "video_codec_capability": codecs.clone(),
                // Windows 控制端按 section 名解析，给出别名数组。
                "display": displays,
                "frame_quality": codecs,
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
        // Windows 必需键必须在场（缺任一项整条被 negotiation_helper 丢弃）。
        assert_eq!(
            caps[0],
            json!({"video_codec": 1, "chroma_sampling": 1, "codec_impl": 0, "bit_depth": 8, "width": 3840,
                   "height": 2160, "max_width": 3840, "max_height": 2160, "max_frame_quality": 6})
        );
        // frame_quality 是 Windows 控制端读的 section 名，必须与 video_codec_capability 同步。
        assert_eq!(payload["data"]["device_capability"]["frame_quality"], payload["data"]["device_capability"]["video_codec_capability"]);
        assert_eq!(payload["data"]["device_capability"]["display"][0]["fps"], 30);
        // display 条目也必须带 fps/hdr：缺一项 negotiation_helper 整条丢，帧率菜单被判受限。
        assert_eq!(payload["data"]["device_capability"]["display"][0]["hdr"], -1);
        assert!(payload["data"]["device_capability"]["display"][0]["hdr"].is_i64());
        assert_eq!(payload["data"]["device_capability"]["display_info"][0]["hdr"], -1);
        assert_eq!(payload["data"]["device_capability"]["ice_id"], "ice-example");
        // HEVC + 真彩叠加出 4 项（照参照实现的笛卡尔积）；没实现的能力绝不虚报。
        let wide = device_capability("c", "i", 0, 60, CODEC_IMPL_NVENC, true, true, 3840, 2160);
        assert_eq!(wide["data"]["device_capability"]["video_codec_capability"].as_array().unwrap().len(), 4);
        assert_eq!(wide["data"]["device_capability"]["frame_quality"].as_array().unwrap().len(), 4);
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
