//! 传输层：webrtc-rs 的 `RTCPeerConnection`，被控端是 **answerer**，DTLS 角色 **Server**。
//!
//! 官方云信令里没有独立的 ICE 通道：SDP 与 ICE candidate 都走 `soac` 事件（见 SPEC §9），
//! 所以这一层只暴露四件事——吃 offer 回 answer、喂远端 candidate、把画面与控制字节送出去。
//!
//! 关键约束（参照实现踩过的坑，这里全部照抄成代码）：
//! - **answerer 必须声明 DTLS Server**，否则与 offerer（Client）角色冲突，DTLS 握手直接失败。
//! - **关 mDNS**：云端信令给的是 stun/turn，本机 mDNS candidate 客户端根本解不了。
//! - **answer 要补 `a=max-message-size`**：webrtc-rs 不写这行，libwebrtc 会按 64 KiB 上限
//!   拒绝大的控制/文件消息。只改建出去的副本——本端描述必须与 webrtc-rs 自己生成的 answer 一致。
//! - **必须一直读视频 sender 的 RTCP**：不读的话 NACK/PLI/FIR 堆在管道里，画面卡死不自愈。
//! - **CONTROL 通道要等 `Open`**：webrtc-rs 在 handle_open 之前就把通道announce出来（ready_state
//!   Connecting），那时 send 会返回 ErrClosedPipe，所以发送一律先查 ready_state。

use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;

use webrtc::ice_transport::ice_gathering_state::RTCIceGatheringState;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// CONTROL 通道标签：官方所有控制 JSON 与 SimpleAction protobuf 都走它。
pub const CONTROL_LABEL: &str = "CONTROL_DATA_CHANNEL";
/// webrtc-rs 不写 max-message-size，我们替客户端补上（libwebrtc 按此上限放行）。
pub const MAX_MESSAGE_SIZE: usize = 262_144;
/// 视频轨 id / stream id：SDP 里的 m=video mid 来源，客户端靠它认轨。
pub const VIDEO_TRACK_ID: &str = "uu-screen";

/// 传输层往外报的事件，由宿主主循环消费。
#[derive(Debug, Clone)]
pub enum PeerEvent {
    /// CONTROL 通道可用（首次 announce 与真正 Open 都报，宿主用 `control_ready` 复核）。
    ControlOpen { label: String },
    ControlClose { label: String },
    ControlData { label: String, data: Vec<u8> },
    /// 客户端报了 PLI 或 FIR：下一帧必须 IDR。
    KeyframeRequest,
    /// 连接状态名（connecting/connected/disconnected/failed/closed），供状态机与日志。
    ConnectionState(String),
    /// 视频轨 RTCP 丢包统计，宿主可据此降码率。
    ReceiverReport { fraction_lost: u8, total_lost: i64 },
    /// 本机收集到的 ICE candidate：云端不靠 answer 里写死 candidate，而是 trickle——
    /// 必须逐条以 `soac type=candidate` 发给客户端，否则对端永远找不到我们。
    IceCandidate { candidate: String, sdp_mid: Option<String>, sdp_mline_index: Option<u32> },
}

/// 从信令的 `room_info.iceServers` 建 ICE 服务器列表。
///
/// 形状按官方返回容错：`{urls:[...]}` 或 `{url:"..."}`，用户名/密码可缺（纯 stun）。
pub fn ice_servers_from_json(value: &Value) -> Vec<RTCIceServer> {
    let mut out = Vec::new();
    let items = match value {
        Value::Array(items) => items.clone(),
        Value::Object(map) => map.get("iceServers").and_then(Value::as_array).cloned().unwrap_or_default(),
        _ => Vec::new(),
    };
    for item in items {
        let urls = match &item {
            Value::Object(map) => match map.get("urls").and_then(Value::as_array) {
                Some(list) => list.iter().filter_map(|u| u.as_str().map(str::to_owned)).collect::<Vec<_>>(),
                None => map.get("url").and_then(Value::as_str).map(|u| vec![u.to_owned()]).unwrap_or_default(),
            },
            Value::String(url) => vec![url.clone()],
            _ => Vec::new(),
        };
        if urls.is_empty() {
            continue;
        }
        out.push(RTCIceServer {
            urls,
            username: item.get("username").and_then(Value::as_str).unwrap_or("").to_owned(),
            credential: item.get("credential").and_then(Value::as_str).unwrap_or("").to_owned(),
            ..Default::default()
        });
    }
    out
}

/// 云端在一次会议里给的 ICE 配置：`iceServers` + 可选 `force_relay`。
///
/// 信令把它塞在 `be-controlled` 的 header（或其 `data` 子对象）里，`room_info` ack 里常常是空的，
/// 所以没有供应时必须退回公共 STUN——纯 host candidate 跨 NAT 连不上。
/// 只接受 stun/stuns/turn/turns 前缀的 URL，其他（比如客户端误发的 https）一律丢掉。
/// 不 derive Debug、不打印：里面是 TURN 凭据。
pub fn ice_configuration(header: &Value) -> Result<RTCConfiguration, String> {
    let data = header.get("data").unwrap_or(header);
    let field = |name: &str| header.get(name).or_else(|| data.get(name));
    let relay_only = field("force_relay").and_then(Value::as_bool).unwrap_or(false);
    let mut servers = Vec::new();
    if let Some(items) = field("iceServers").and_then(Value::as_array) {
        for item in items.iter().take(16) {
            let urls = match item.get("urls").or_else(|| item.get("url")) {
                Some(Value::String(url)) => vec![url.clone()],
                Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).take(16).map(str::to_owned).collect(),
                _ => Vec::new(),
            };
            let urls: Vec<String> = urls
                .into_iter()
                .filter(|url| url.len() <= 2048 && ["stun:", "stuns:", "turn:", "turns:"].iter().any(|p| url.starts_with(p)))
                .collect();
            if urls.is_empty() {
                continue;
            }
            let secret = |name: &str| item.get(name).and_then(Value::as_str).filter(|s| s.len() <= 4096).unwrap_or("").to_owned();
            servers.push(RTCIceServer {
                urls,
                username: secret("username"),
                credential: secret("credential"),
                credential_type: RTCIceCredentialType::Password,
            });
        }
    }
    let has_turn = servers.iter().any(|s| s.urls.iter().any(|u| u.starts_with("turn:") || u.starts_with("turns:")));
    if relay_only && !has_turn {
        return Err("云端要求纯中继，但没给可用的 TURN 配置".into());
    }
    let supplied = !servers.is_empty();
    let ice_servers = if supplied {
        servers
    } else {
        vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".into(), "stun:stun1.l.google.com:19302".into(), "stun:stun.miwifi.com:3478".into()],
            ..Default::default()
        }]
    };
    log_info(format!("ICE 配置：云端供应={supplied} 服务器 {} 台 TURN={has_turn} 纯中继={relay_only}", ice_servers.len()));
    Ok(RTCConfiguration {
        ice_servers,
        ice_transport_policy: if relay_only { RTCIceTransportPolicy::Relay } else { RTCIceTransportPolicy::All },
        ..Default::default()
    })
}

/// 从 soac 载荷拿到 offer SDP：`sdp` 优先，空则解 `gzip_sdp`（base64 + gzip）。
pub fn decode_offer_sdp(sdp: Option<&str>, gzip_sdp: Option<&str>) -> Option<String> {
    if let Some(text) = sdp.filter(|s| !s.is_empty()) {
        return Some(text.to_owned());
    }
    let compressed = base64::engine::general_purpose::STANDARD.decode(gzip_sdp?).ok()?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(&compressed[..]).read_to_string(&mut text).ok()?;
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 给 answer 的数据通道段补 `a=max-message-size`（只改发出去的副本）。
pub fn with_max_message_size(sdp: &str) -> String {
    if sdp.contains("a=max-message-size:") {
        return sdp.to_owned();
    }
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(sdp.len() + 32);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        if line.trim_end().starts_with("a=sctp-port:") {
            out.push_str(&format!("a=max-message-size:{MAX_MESSAGE_SIZE}{eol}"));
        }
    }
    out
}

/// SDP 里谈定的 RTP 头扩展（`a=extmap:<id> <uri>`）。
///
/// 只有出现在这里的 URI 才会被写进包：webrtc-rs 发样本时按**谈定表**查 id，
/// 查不到就静默丢掉那个扩展。UU 私有两条（capture-index / new-frame）缺任何一条，
/// 接收端就算收到画面也只能按名义帧长推算延迟与 frm，右下角永远显示 50 ms
/// （2026-10-29 现场：帧率 45 fps、frm 仍 50 ms，就是这两条没谈定）。
pub fn negotiated_extmap(sdp_text: &str) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    for line in sdp_text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=extmap:") {
            let mut parts = rest.splitn(2, ' ');
            let id = match parts.next().and_then(|v| v.split('/').next().and_then(|v| v.parse::<i64>().ok())) {
                Some(id) => id,
                None => continue,
            };
            if let Some(uri) = parts.next() {
                out.push((id, uri.to_owned()));
            }
        }
    }
    out
}

/// UU 私有的两条采集元数据扩展是否谈定（缺哪条就报哪条）。
pub fn missing_uu_extensions(sdp_text: &str) -> Vec<&'static str> {
    let mapped = negotiated_extmap(sdp_text);
    let have: Vec<&str> = mapped.iter().map(|(_, uri)| uri.as_str()).collect();
    [crate::rtp_metadata::CAPTURE_INDEX_URI, crate::rtp_metadata::NEW_FRAME_URI]
        .into_iter()
        .filter(|uri| !have.contains(uri))
        .collect()
}

/// answer 的 video m 行里谈定的 codec 名字（按 payload type 回查 rtpmap）。
pub fn negotiated_video_codecs(answer_sdp: &str) -> Vec<String> {
    let mut payload_types: Vec<String> = Vec::new();
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in answer_sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("m=video ") {
            // m=video <port> <proto> <fmt>...
            payload_types = rest.split_whitespace().skip(2).map(str::to_owned).collect();
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some((payload, value)) = rest.split_once(' ') {
                names.insert(payload.to_owned(), value.split('/').next().unwrap_or("").to_owned());
            }
        }
    }
    let mut out = Vec::new();
    for payload in payload_types {
        if let Some(name) = names.get(&payload) {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    out
}

/// offer 里有没有可用的音频 m 行（`m=audio` 且端口非 0）。
fn offer_has_audio(sdp: &str) -> bool {
    sdp.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some("m=audio") && fields.next().map(|port| port != "0").unwrap_or(false)
    })
}

/// 从 offer SDP 里挑一个能用的 H.264 codec 能力。
///
/// 各家客户端给的 profile-level-id / packetization-mode 都不一样（Windows、Mac、手机各不同），
/// 硬写 42e01f 会协商不上，所以必须照 offer 原样挑。优先级：
/// packetization-mode=1 + baseline(42xx) > packetization-mode=1 > packetization-mode=0。
pub fn h264_capability_from_offer(sdp: &str) -> Option<RTCRtpCodecCapability> {
    let mut payload_types: Vec<u8> = Vec::new();
    let mut fmtps: Vec<(u8, String)> = Vec::new();
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let (payload, value) = rest.split_once(' ')?;
            if value.to_ascii_lowercase().starts_with("h264/") {
                payload_types.push(payload.parse().ok()?);
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            if let Some((payload, params)) = rest.split_once(' ') {
                if let Ok(payload) = payload.parse::<u8>() {
                    fmtps.push((payload, params.to_owned()));
                }
            }
        }
    }
    let mut best: Option<String> = None;
    let mut best_score = -1i8;
    for (payload, params) in fmtps {
        if !payload_types.contains(&payload) {
            continue;
        }
        let mode_one = params.contains("packetization-mode=1");
        let profile = params
            .split(';')
            .find_map(|kv| kv.trim().strip_prefix("profile-level-id="))
            .unwrap_or("")
            .to_ascii_lowercase();
        // 42xx 是 baseline 系：openh264 软编默认就产这个，最容易对上。
        let score = match (mode_one, profile.starts_with("42")) {
            (true, true) => 3,
            (true, false) => 2,
            (false, true) => 1,
            (false, false) => 0,
        };
        if score > best_score {
            best_score = score;
            best = Some(params);
        }
    }
    Some(RTCRtpCodecCapability {
        // webrtc-rs 认的是 SDP rtpmap 里的名字 "video/H264"；写成 RTP 线上的 "video/avc"
        // 会让 codec 匹配整个失败（连 mime 兜底都比不上），报 "codec is not supported by remote"。
        mime_type: "video/H264".to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: best?,
        rtcp_feedback: vec![],
    })
}

/// 连接状态 → 可日志化的短名。
pub fn state_name(state: RTCPeerConnectionState) -> String {
    format!("{state:?}").to_lowercase()
}

/// 被控端传输会话。
pub struct Peer {
    pc: Arc<RTCPeerConnection>,
    /// 视频轨要等看见 offer 才能建：codec 必须照 offer 里的 fmtp 挑（见 `h264_capability_from_offer`）。
    track: Arc<Mutex<Option<Arc<TrackLocalStaticSample>>>>,
    /// 音频轨（opus）：offer 带 audio m 行且没关 UU_AUDIO 时挂上；宿主拿到它发 Opus 包。
    audio_track: Arc<Mutex<Option<Arc<TrackLocalStaticSample>>>>,
    control: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    /// 客户端会开多条通道（CONTROL / TEXT / STREAMER…），回包必须回到来的那条。
    channels: Arc<Mutex<std::collections::HashMap<String, Arc<RTCDataChannel>>>>,
    events: mpsc::UnboundedSender<PeerEvent>,
}

impl Peer {
    /// 建 PeerConnection：默认编解码（含 H.264）+ 拦截器 + answerer DTLS Server。
    pub async fn new(config: RTCConfiguration, events: mpsc::UnboundedSender<PeerEvent>) -> Result<Self, String> {
        let mut media = MediaEngine::default();
        media.register_default_codecs().map_err(|e| format!("注册默认编解码失败: {e}"))?;
        // UU 私有头扩展（capture-index / new-frame）：客户端延迟与 frm 统计靠它们，
        // 不注册协商不上，右下角永远 50 ms（见 rtp_metadata）。
        for uri in [crate::rtp_metadata::CAPTURE_INDEX_URI, crate::rtp_metadata::NEW_FRAME_URI] {
            media
                .register_header_extension(
                    webrtc::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionCapability { uri: uri.into() },
                    webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Video,
                    None,
                )
                .map_err(|e| format!("注册 UU RTP 头扩展失败: {e}"))?;
        }
        let registry = register_default_interceptors(webrtc::interceptor::registry::Registry::new(), &mut media)
            .map_err(|e| format!("注册 RTP 拦截器失败: {e}"))?;
        let mut settings = SettingEngine::default();
        settings.set_answering_dtls_role(DTLSRole::Server).map_err(|e| format!("设置 DTLS answer 角色失败: {e}"))?;
        settings.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        let api = APIBuilder::new().with_media_engine(media).with_setting_engine(settings).with_interceptor_registry(registry).build();

        let pc = Arc::new(api.new_peer_connection(config).await.map_err(|e| format!("创建 PeerConnection 失败: {e}"))?);

        // 本机 candidate 逐条上报：云端靠 `soac type=candidate` trickle，answer 里那几条收集不全，
        // 不 trickle 对端就永远找不到我们（连接状态一直停在 New）。
        {
            let ev = events.clone();
            pc.on_ice_candidate(Box::new(move |candidate: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
                let ev = ev.clone();
                Box::pin(async move {
                    let Some(candidate) = candidate else { return };
                    if let Ok(init) = candidate.to_json() {
                        let _ = ev.send(PeerEvent::IceCandidate {
                            candidate: init.candidate,
                            sdp_mid: init.sdp_mid,
                            sdp_mline_index: init.sdp_mline_index.map(|v| v as u32),
                        });
                    }
                })
            }));
        }

        let control: Arc<Mutex<Option<Arc<RTCDataChannel>>>> = Arc::new(Mutex::new(None));
        let channels: Arc<Mutex<std::collections::HashMap<String, Arc<RTCDataChannel>>>> = Arc::new(Mutex::new(std::collections::HashMap::new()));
        pc.on_data_channel(Box::new({
            let control = control.clone();
            let channels = channels.clone();
            let events = events.clone();
            move |d: Arc<RTCDataChannel>| {
                let label = d.label().to_owned();
                let is_control = label == CONTROL_LABEL;
                // 真正能用是 on_open 之后：那时才存通道、才报 ControlOpen。
                let control_open = control.clone();
                let channels_open = channels.clone();
                let channel_open = d.clone();
                let events_open = events.clone();
                let label_open = label.clone();
                d.on_open(Box::new(move || {
                    let events = events_open.clone();
                    let control = control_open.clone();
                    let channels = channels_open.clone();
                    let channel = channel_open.clone();
                    let label = label_open.clone();
                    Box::pin(async move {
                        if is_control {
                            *control.lock().await = Some(channel.clone());
                        }
                        (*channels.lock().await).insert(label.clone(), channel);
                        let _ = events.send(PeerEvent::ControlOpen { label });
                    })
                }));
                let events_close = events.clone();
                let label_close = label.clone();
                d.on_close(Box::new(move || {
                    let events = events_close.clone();
                    let label = label_close.clone();
                    Box::pin(async move {
                        let _ = events.send(PeerEvent::ControlClose { label });
                    })
                }));
                let events_msg = events.clone();
                let label_msg = label.clone();
                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    let events = events_msg.clone();
                    let label = label_msg.clone();
                    let data = msg.data.to_vec();
                    Box::pin(async move {
                        let _ = events.send(PeerEvent::ControlData { label, data });
                    })
                }));
                Box::pin(async move {})
            }
        }));
        // webrtc-rs 0.10 没有 `on_connection_state_change`：轮询状态，变化时报出去。
        let poll_pc = pc.clone();
        let poll_events = events.clone();
        tokio::spawn(async move {
            let mut last = poll_pc.connection_state();
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let now = poll_pc.connection_state();
                if now != last {
                    let _ = poll_events.send(PeerEvent::ConnectionState(state_name(now)));
                    last = now;
                }
            }
        });
        Ok(Self { pc, track: Arc::new(Mutex::new(None)), audio_track: Arc::new(Mutex::new(None)), control, channels, events })
    }

    /// 吃客户端 offer，回我们要用的 answer SDP（已补 max-message-size）。
    ///
    /// 等 ICE 收集完再返回：官方信令不做 trickle offer，answer 必须带齐本机 candidate。
    pub async fn answer_offer(&self, sdp_text: &str) -> Result<String, String> {
        let offer = RTCSessionDescription::offer(sdp_text.to_owned()).map_err(|e| format!("offer 不合法: {e}"))?;
        // 挂轨必须在 set_remote_description 之前：之后再 add_track 会多出一条 offer 里没有的 m 行。
        if self.track.lock().await.is_none() {
            let capability = h264_capability_from_offer(sdp_text).ok_or("offer 里没有可用的 H.264 codec")?;
            println!("   按 offer 选中的 H.264：{}", capability.sdp_fmtp_line);
            let track = Arc::new(TrackLocalStaticSample::new(capability, VIDEO_TRACK_ID.to_owned(), VIDEO_TRACK_ID.to_owned()));
            let sender = self.pc.add_track(track.clone()).await.map_err(|e| format!("添加视频轨失败: {e}"))?;
            spawn_rtcp_drain(sender, self.events.clone());
            *self.track.lock().await = Some(track);
        }
        // 客户端 offer 永远带 Opus 音频 m 行（官方 Windows 解码侧有 opusdec）——挂音频轨
        // 才有声音；没 audio 行或 UU_AUDIO=0 就不挂（webrtc-rs 会自己把 m 行收成 inactive）。
        if self.audio_track.lock().await.is_none() && crate::audio::enabled() && offer_has_audio(sdp_text) {
            let audio = Arc::new(TrackLocalStaticSample::new(
                RTCRtpCodecCapability {
                    mime_type: "audio/opus".to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    ..Default::default()
                },
                "audio_0".to_owned(),
                "audio_0".to_owned(),
            ));
            match self.pc.add_track(audio.clone()).await {
                Ok(sender) => {
                    spawn_rtcp_drain(sender, self.events.clone());
                    *self.audio_track.lock().await = Some(audio);
                    println!("   音频轨已挂：opus 48k 立体声");
                }
                Err(e) => println!("   挂音频轨失败（没声音，视频不受影响）：{}", e.to_string().chars().take(160).collect::<String>()),
            }
        }
        self.pc.set_remote_description(offer).await.map_err(|e| format!("设置远端描述失败: {e}"))?;
        let answer = self.pc.create_answer(None).await.map_err(|e| format!("生成 answer 失败: {e}"))?;
        // 协商结果直接读 answer 的 video m 行：那才是双方真正谈定的 payload type。
        // 里面没有 H264 就说明 offer 对不上，早报错比客户端黑屏好查。
        let negotiated = negotiated_video_codecs(&answer.sdp);
        if !negotiated.iter().any(|c| c.eq_ignore_ascii_case("H264")) {
            return Err(format!("协商后视频没有 H.264（谈定的是 {negotiated:?}）"));
        }
        println!("   视频协商完成：{}", negotiated.join(" / "));
        // 采集元数据扩展没谈定 = 接收端 frm/延迟永远按名义 50 ms 报，必须看得见。
        let missing = missing_uu_extensions(&answer.sdp);
        if missing.is_empty() {
            println!("   RTP 采集元数据已谈定：capture-index + new-frame");
        } else {
            println!("   ⚠ offer 没提出这些 RTP 头扩展，写不进包：{:?}", missing);
        }
        self.pc.set_local_description(answer.clone()).await.map_err(|e| format!("设置本端描述失败: {e}"))?;
        if !wait_gathering_complete(&self.pc, Duration::from_secs(1)).await {
            // 超时照发：剩余 candidate 走 trickle，客户端照样能打洞。
            // 等齐 7 台 STUN + TURN 会把 answer 拖后好几秒，连接半分钟的账就在这。
            log_warn("ICE 收集未齐，剩余 candidate 走 trickle");
        }
        // create_answer 那一刻还没有 candidate（收集在 set_local_description 之后才开始），
        // 拿 current_local_description 才带得上本机 candidate——客户端没有 trickle 通道时只能靠它。
        let gathered = self.pc.current_local_description().await.map(|d| d.sdp).unwrap_or(answer.sdp);
        Ok(with_max_message_size(&gathered))
    }

    /// 是否已经连通：宿主靠它决定什么时候上报设备能力，不必引入 webrtc 的类型。
    pub fn is_connected(&self) -> bool {
        matches!(self.pc.connection_state(), RTCPeerConnectionState::Connected)
    }

    /// 当前连接状态：宿主主循环与验收脚本靠它判断会话是否真的通了。
    pub fn connection_state(&self) -> RTCPeerConnectionState {
        self.pc.connection_state()
    }

    /// 喂一条远端 ICE candidate（soac 事件里带的）。
    /// 音频轨（挂了才有声音可发）。
    pub async fn audio_track(&self) -> Option<Arc<TrackLocalStaticSample>> {
        self.audio_track.lock().await.clone()
    }

    /// RTCPeerConnection 句柄（音频推流任务要查连接状态）。
    pub fn pc_handle(&self) -> Arc<RTCPeerConnection> {
        self.pc.clone()
    }

    pub async fn add_candidate(&self, candidate: &str, sdp_mid: Option<&str>, mline_index: Option<u16>) -> Result<(), String> {
        if candidate.is_empty() {
            return Ok(());
        }
        let init = RTCIceCandidateInit {
            candidate: candidate.to_owned(),
            sdp_mid: sdp_mid.map(str::to_owned),
            sdp_mline_index: mline_index,
            ..Default::default()
        };
        self.pc.add_ice_candidate(init).await.map_err(|e| format!("加远端 candidate 失败: {e}"))
    }

    /// 送一个 H.264 访问单元（Annex-B 字节）到视频轨。
    pub async fn write_video(&self, access_unit: Vec<u8>, duration: Duration) -> Result<(), String> {
        let track = self
            .track
            .lock()
            .await
            .clone()
            .ok_or("视频轨还没建立（还没收到 offer）")?;
        let extensions = [crate::rtp_metadata::capture_index(0), crate::rtp_metadata::new_frame()];
        track
            .write_sample_with_extensions(
                &Sample {
                    data: Bytes::from(access_unit),
                    timestamp: std::time::SystemTime::now(),
                    duration: duration.max(Duration::from_micros(1)),
                    packet_timestamp: 0,
                    prev_dropped_packets: 0,
                    prev_padding_packets: 0,
                },
                &extensions,
            )
            .await
            .map_err(|e| format!("写视频样本失败: {e}"))
    }

    /// CONTROL 通道是否真的可发（必须 Open，Connecting 时 send 会 ErrClosedPipe）。
    pub async fn control_ready(&self) -> bool {
        self.control
            .lock()
            .await
            .as_ref()
            .map(|d| d.ready_state() == RTCDataChannelState::Open)
            .unwrap_or(false)
    }

    /// 往指定 label 的通道发字节；回包要回到消息来的那条通道。
    pub async fn send_on(&self, label: &str, data: &[u8]) -> Result<(), String> {
        let guard = self.channels.lock().await;
        let channel = guard.get(label).ok_or(format!("{label} 通道还没建立"))?;
        if channel.ready_state() != RTCDataChannelState::Open {
            return Err(format!("{label} 通道还没 Open"));
        }
        channel.send(&Bytes::from(data.to_vec())).await.map(|_| ()).map_err(|e| format!("{label} 发送失败: {e}"))
    }

    /// 往 CONTROL 通道发字节（protobuf 回包、剪贴板等）。
    pub async fn send_control(&self, data: &[u8]) -> Result<(), String> {
        let guard = self.control.lock().await;
        let channel = guard.as_ref().ok_or("CONTROL 通道还没建立")?;
        if channel.ready_state() != RTCDataChannelState::Open {
            return Err("CONTROL 通道还没 Open".into());
        }
        channel
            .send(&Bytes::from(data.to_vec()))
            .await
            .map(|_| ())
            .map_err(|e| format!("CONTROL 发送失败: {e}"))
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}

fn log_warn(msg: &str) {
    eprintln!("[transport] {msg}");
}

/// 一行状态日志：只放结构与计数，绝不放凭据。
fn log_info(msg: String) {
    println!("   [transport] {msg}");
}

/// 一直读视频 sender 的 RTCP，把 PLI/FIR 与丢包统计报出去。
fn spawn_rtcp_drain(sender: Arc<RTCRtpSender>, events: mpsc::UnboundedSender<PeerEvent>) {
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        loop {
            match sender.read(&mut buf).await {
                Ok((packets, _)) => {
                    for packet in packets {
                        if packet.as_any().is::<webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                            || packet.as_any().is::<webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>()
                        {
                            let _ = events.send(PeerEvent::KeyframeRequest);
                        }
                        if let Some(rr) = packet.as_any().downcast_ref::<webrtc::rtcp::receiver_report::ReceiverReport>() {
                            for r in &rr.reports {
                                let _ = events.send(PeerEvent::ReceiverReport { fraction_lost: r.fraction_lost, total_lost: r.total_lost as i64 });
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// 等 ICE candidate 收集完（官方信令不做 trickle offer，answer 必须带齐本机 candidate）。
pub async fn wait_gathering_complete(pc: &RTCPeerConnection, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // webrtc-rs 把收集状态挂在 peer 上（`ice_gathering_state()`），不是每条 transport。
        let done = pc.ice_gathering_state() == RTCIceGatheringState::Complete;
        if done {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uu_capture_extensions_must_be_in_the_negotiated_extmap() {
        // 接收端算 frm/延迟靠这两条私有扩展；answer 谈不到 = 写不进包 = 永远显示 50 ms。
        let offered = format!(
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=extmap:3 {}\r\na=extmap:5 {}\r\n",
            crate::rtp_metadata::CAPTURE_INDEX_URI,
            crate::rtp_metadata::NEW_FRAME_URI
        );
        assert!(missing_uu_extensions(&offered).is_empty());
        assert_eq!(negotiated_extmap(&offered).len(), 2);

        // 只有 abs-send-time 这类标准扩展：UU 两条都缺，必须报出来。
        let plain = "m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=extmap:4 http://www.abs-send-time\r\n";
        assert_eq!(missing_uu_extensions(plain).len(), 2);

        // 只谈定一条也要点名，别报"全齐"。
        let one = format!("a=extmap:3 {}\r\n", crate::rtp_metadata::CAPTURE_INDEX_URI);
        assert_eq!(missing_uu_extensions(&one), vec![crate::rtp_metadata::NEW_FRAME_URI]);
    }

    #[test]
    fn offer_has_audio_detects_active_audio_lines() {
        assert!(offer_has_audio("m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n"));
        assert!(!offer_has_audio("m=audio 0 RTP/AVP 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n"));
        assert!(!offer_has_audio("m=video 9 UDP/TLS/RTP/SAVPF 96\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n"));
    }

    #[test]
    fn h264_capability_is_picked_from_the_offer_not_hardcoded() {
        // 客户端常见的三种 H.264：mode=1 baseline、mode=1 high、mode=0 baseline。
        let offer = "m=video 9 UDP/TLS/RTP/SAVPF 96 98 100\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:98 H264/90000\r\n\
a=fmtp:98 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f\r\n\
a=rtpmap:100 H264/90000\r\n\
a=fmtp:100 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
a=rtpmap:101 H264/90000\r\n\
a=fmtp:101 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r\n";
        let capability = h264_capability_from_offer(offer).unwrap();
        assert_eq!(capability.mime_type, "video/H264");
        assert!(capability.sdp_fmtp_line.contains("packetization-mode=1"));
        assert!(capability.sdp_fmtp_line.contains("profile-level-id=42e01f"));

        // 只有 mode=0 的客户端：也必须能挑出来，不能因为硬要求 mode=1 而失败。
        let only_mode0 = "a=rtpmap:102 H264/90000\r\na=fmtp:102 packetization-mode=0;profile-level-id=42001f";
        assert!(h264_capability_from_offer(only_mode0).unwrap().sdp_fmtp_line.contains("packetization-mode=0"));

        // 没有 H.264 的 offer：明确失败，不假装有 codec。
        assert!(h264_capability_from_offer("a=rtpmap:96 VP8/90000").is_none());
        // rtpmap 有 H.264 但没有 fmtp：挑不出参数，也失败。
        assert!(h264_capability_from_offer("a=rtpmap:98 H264/90000").is_none());
    }

    #[test]
    fn ice_configuration_falls_back_to_public_stun_and_honours_relay_only() {
        // 云端没供应：必须有公共 STUN，否则跨 NAT 只剩 host candidate。
        let config = ice_configuration(&serde_json::json!({})).unwrap();
        assert_eq!(config.ice_servers.len(), 1);
        assert!(config.ice_servers[0].urls.len() == 3);
        assert_eq!(config.ice_transport_policy, RTCIceTransportPolicy::All);

        // 供应 TURN：只用供应的，且凭据类型 Password。
        let config = ice_configuration(&serde_json::json!({
            "data": { "iceServers": [{ "urls": ["turn:relay.example:3478?transport=udp"], "username": "u", "credential": "c" }] }
        }))
        .unwrap();
        assert_eq!(config.ice_servers.len(), 1);
        assert_eq!(config.ice_servers[0].username, "u");
        assert_eq!(config.ice_servers[0].credential_type, RTCIceCredentialType::Password);

        // 非 stun/turn 前缀的 URL 一律丢掉。
        let config = ice_configuration(&serde_json::json!({ "iceServers": [{ "urls": ["https://nope"] }] })).unwrap();
        assert_eq!(config.ice_servers[0].urls.len(), 3, "丢掉无效 URL 后应退回 STUN");

        // 要求纯中继却没 TURN：报错，不静默降级。
        assert!(ice_configuration(&serde_json::json!({ "force_relay": true, "iceServers": [] })).is_err());
        let config = ice_configuration(&serde_json::json!({
            "force_relay": true,
            "iceServers": [{ "url": "turn:relay.example:3478", "username": "u", "credential": "c" }]
        }))
        .unwrap();
        assert_eq!(config.ice_transport_policy, RTCIceTransportPolicy::Relay);
    }

    #[test]
    fn ice_servers_accept_both_url_shapes_and_skip_empties() {
        let json = serde_json::json!({
            "iceServers": [
                {"urls": ["stun:stun.nie.163.com:3478"]},
                {"url": "turn:turn.nie.163.com:3478", "username": "u", "credential": "c"},
                {"urls": []},
                {"urls": ["turn:relay"], "username": "x", "credential": "y"}
            ]
        });
        let servers = ice_servers_from_json(&json);
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].urls, vec!["stun:stun.nie.163.com:3478"]);
        assert_eq!(servers[0].username, "");
        assert_eq!(servers[1].credential, "c");
        assert_eq!(servers[2].urls, vec!["turn:relay"]);
        assert_eq!(ice_servers_from_json(&serde_json::json!([{"urls": ["stun:x"]}])).len(), 1);
        assert_eq!(ice_servers_from_json(&Value::Null).len(), 0);
    }

    #[test]
    fn offer_sdp_prefers_plain_and_falls_back_to_gzip_base64() {
        assert_eq!(decode_offer_sdp(Some("v=0\r\n"), None).unwrap(), "v=0\r\n");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, b"v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96").unwrap();
        let gz = base64::engine::general_purpose::STANDARD.encode(enc.finish().unwrap());
        assert_eq!(decode_offer_sdp(None, Some(&gz)).unwrap(), "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96");
        assert_eq!(decode_offer_sdp(Some(""), Some(&gz)).unwrap(), "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96");
        assert!(decode_offer_sdp(None, None).is_none());
        assert!(decode_offer_sdp(None, Some("not base64!")).is_none());
        assert!(decode_offer_sdp(None, Some(&base64::engine::general_purpose::STANDARD.encode(b"garbage"))).is_none());
    }

    #[test]
    fn max_message_size_is_added_once_after_sctp_port() {
        let sdp = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sctp-port:5000\r\na=mid:1\r\n";
        let amended = with_max_message_size(sdp);
        assert!(amended.contains("a=max-message-size:262144\r\n"));
        assert_eq!(amended.matches("a=max-message-size:").count(), 1);
        assert!(amended.find("a=max-message-size:").unwrap() < amended.find("a=mid:1").unwrap());
        assert_eq!(with_max_message_size(&amended), amended, "已有就不重复加");
        let lf = "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\na=sctp-port:5000\n";
        assert!(with_max_message_size(lf).contains("a=max-message-size:262144\n"));
    }
}
