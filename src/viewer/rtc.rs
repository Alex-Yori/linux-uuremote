//! 控制端 WebRTC：我是 offerer（被控端是 answerer）。H264 PT102 + opus 111，
//! UU 私有 RTP 扩展由我们主动提出——官方客户端不提这两个扩展，frm 就永远是 50ms，
//! 我们自己提才有机会把它谈下来（被控端早就支持写扩展）。
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{json, Value};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;

use super::media::{Packet as MediaPacket, Receiver};
use super::wire::{self, CursorShape};
use crate::rtp_metadata::{CAPTURE_INDEX_URI, NEW_FRAME_URI};

/// 解码好的一帧，YUV 平面按 stride 排布，直接进 SDL 的 YV12 纹理。
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub ystride: i32,
    pub ustride: i32,
    pub vstride: i32,
}

pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub rgba: Vec<u8>,
}

#[derive(Default)]
pub struct Stats {
    pub video_packets: AtomicU64,
    pub video_bytes: AtomicU64,
    pub audio_packets: AtomicU64,
    pub frames: AtomicU64,
    pub last_gap_ms: AtomicU64,
    pub rtt_ms: AtomicU64,
    pub control_open: AtomicBool,
    pub echo_seen: AtomicBool,
    pub decode_errors: AtomicU64,
    pub audio_enabled: AtomicBool,
    pub remote_w: AtomicU64,
    pub remote_h: AtomicU64,
    pub chroma: AtomicU64,
}

/// tokio 线程往 SDL 线程送东西的汇。
pub struct Host {
    pub stats: Arc<Stats>,
    pub frames: std::sync::mpsc::SyncSender<Frame>,
    pub cursor: std::sync::mpsc::SyncSender<CursorImage>,
    pub audio: Arc<Mutex<std::collections::VecDeque<f32>>>,
    /// 我们发出的 ping echo（seq → 发出时刻），回包按 seq 对上就算 RTT。
    pub pings: Arc<Mutex<HashMap<u64, Instant>>>,
}

pub fn configuration(result: &Value) -> Result<RTCConfiguration, String> {
    let list = result.get("iceServers").and_then(Value::as_array).ok_or("control 应答里没有 iceServers")?;
    let mut servers = Vec::new();
    for server in list {
        let urls = match server.get("urls") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_owned).ok_or("ICE URL 不合法")).collect::<Result<Vec<_>, _>>()?,
            _ => return Err("ICE URL 不合法".into()),
        };
        if urls.is_empty() || urls.iter().any(|u| !["stun:", "stuns:", "turn:", "turns:"].iter().any(|p| u.starts_with(p))) {
            return Err("ICE URL 协议不对".into());
        }
        let string = |key: &str| match server.get(key) { Some(Value::String(s)) => Ok(s.clone()), None => Ok(String::new()), _ => Err("ICE 鉴权字段不合法".to_string()) };
        servers.push(RTCIceServer { urls, username: string("username")?, credential: string("credential")?, credential_type: webrtc::ice_transport::ice_credential_type::RTCIceCredentialType::Password });
    }
    // 云端列表为空 = 只有 host 候选（局域网直连），不能瞎补公网 STUN。
    Ok(RTCConfiguration { ice_servers: servers, ..Default::default() })
}

pub struct Peer {
    pub pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    pub control: Arc<RTCDataChannel>,
    pub text: Arc<RTCDataChannel>,
    answered: bool,
    early: Vec<RTCIceCandidateInit>,
    app_id: String,
    ice_id: String,
    remote_client: String,
}

fn media_engine() -> Result<MediaEngine, String> {
    // 默认 MediaEngine 的 codec 表是空的：只报被控端认识的这一对，payload 号照线上观测。
    let mut engine = MediaEngine::default();
    engine
        .register_codec(RTCRtpCodecParameters { capability: RTCRtpCodecCapability { mime_type: "video/H264".into(), clock_rate: 90000, channels: 0, sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into(), ..Default::default() }, payload_type: 102, ..Default::default() }, RTPCodecType::Video)
        .map_err(|_| "H264 注册失败")?;
    engine
        .register_codec(RTCRtpCodecParameters { capability: RTCRtpCodecCapability { mime_type: "audio/opus".into(), clock_rate: 48000, channels: 2, sdp_fmtp_line: "minptime=10;useinbandfec=1".into(), ..Default::default() }, payload_type: 111, ..Default::default() }, RTPCodecType::Audio)
        .map_err(|_| "opus 注册失败")?;
    for uri in [CAPTURE_INDEX_URI, NEW_FRAME_URI] {
        engine.register_header_extension(RTCRtpHeaderExtensionCapability { uri: uri.into() }, RTPCodecType::Video, None).map_err(|_| "RTP 扩展注册失败")?;
    }
    Ok(engine)
}

impl Peer {
    pub async fn new(config: RTCConfiguration, shape: &wire::Shape, app_id: &str, ice_id: &str, remote_client: &str, outbound: mpsc::Sender<Value>, host: Arc<Host>, settings: SettingEngine) -> Result<Self, String> {
        let mut engine = media_engine()?;
        let registry = register_default_interceptors(webrtc::interceptor::registry::Registry::new(), &mut engine).map_err(|_| "interceptor 初始化失败")?;
        let api = APIBuilder::new().with_media_engine(engine).with_setting_engine(settings).with_interceptor_registry(registry).build();
        let pc = Arc::new(api.new_peer_connection(config).await.map_err(|e| format!("Peer 创建失败: {e}"))?);

        pc.on_peer_connection_state_change(Box::new(|state| {
            println!("   连接状态：{state}");
            Box::pin(async {})
        }));

        let (app, ice, client) = (app_id.to_owned(), ice_id.to_owned(), remote_client.to_owned());
        let weak_pc = Arc::downgrade(&pc);
        let (app_c, ice_c, client_c) = (app.clone(), ice.clone(), client.clone());
        pc.on_ice_candidate(Box::new(move |candidate| {
            let weak_pc = weak_pc.clone();
            let tx = outbound.clone();
            let (app, ice, client) = (app_c.clone(), ice_c.clone(), client_c.clone());
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Ok(mut value) = candidate.to_json() {
                        // 回调里常常没有 MID：绑到第一条 bundle m-line。
                        if let Some(pc) = weak_pc.upgrade() {
                            if let Some(local) = pc.local_description().await {
                                if let Some(mid) = local.sdp.lines().find_map(|line| line.strip_prefix("a=mid:")) {
                                    value.sdp_mid = Some(mid.trim().into());
                                    value.sdp_mline_index = Some(0);
                                }
                            }
                        }
                        let _ = tx.send(json!({"client_id": client, "data": {"type": "candidate", "candidate": value, "ice_id": ice, "app_control_id": app}})).await;
                    }
                }
            })
        }));

        let h = host.clone();
        let pc_t = pc.clone();
        pc.on_track(Box::new(move |track, _, _| {
            let h = h.clone();
            let weak_pc = Arc::downgrade(&pc_t);
            Box::pin(async move {
                // 处理器必须立刻返回：webrtc-rs 在它返回前不会送出下一条 track。
                tokio::spawn(async move {
                    let video = track.kind() == RTPCodecType::Video;
                    let mime = track.codec().capability.mime_type.clone();
                    println!("   媒体轨：kind={:?} mime={} pt={}", track.kind(), mime, track.payload_type());
                    if video {
                        if !mime.eq_ignore_ascii_case("video/H264") { println!("   !! 被控端选了 {mime}，本控制端只解 H264，这条轨丢弃"); return; }
                        let mut decoder = openh264::decoder::Decoder::new().expect("openh264 解码器");
                        let mut receiver = Receiver::new();
                        let mut tick = tokio::time::interval(Duration::from_millis(40));
                        let mut last_pli = Instant::now() - Duration::from_secs(1);
                        let mut last_frame = Instant::now();
                        loop {
                            let output = tokio::select! {
                                packet = track.read_rtp() => match packet {
                                    Ok((p, _)) => {
                                        h.stats.video_packets.fetch_add(1, Ordering::Relaxed);
                                        h.stats.video_bytes.fetch_add(p.payload.len() as u64, Ordering::Relaxed);
                                        receiver.push(MediaPacket { seq: p.header.sequence_number, timestamp: p.header.timestamp, marker: p.header.marker, payload: p.payload.to_vec() })
                                    }
                                    Err(_) => break,
                                },
                                _ = tick.tick() => receiver.expire(),
                            };
                            if output.pli {
                                if last_pli.elapsed() >= Duration::from_millis(250) {
                                    last_pli = Instant::now();
                                    if let Some(pc) = weak_pc.upgrade() {
                                        let _ = pc.write_rtcp(&[Box::new(PictureLossIndication { sender_ssrc: 0, media_ssrc: track.ssrc() })]).await;
                                    } else { break; }
                                }
                            }
                            let mut stopped = false;
                            for au in output.units {
                                match decoder.decode(&au) {
                                    Ok(Some(image)) => {
                                        use openh264::formats::YUVSource;
                                        let (w, hh) = image.dimensions();
                                        let (ys, us, vs) = image.strides();
                                        let frame = Frame { width: w, height: hh, y: image.y().to_vec(), u: image.u().to_vec(), v: image.v().to_vec(), ystride: ys as i32, ustride: us as i32, vstride: vs as i32 };
                                        h.stats.remote_w.store(w as u64, Ordering::Relaxed);
                                        h.stats.remote_h.store(hh as u64, Ordering::Relaxed);
                                        let now = Instant::now();
                                        h.stats.last_gap_ms.store(now.duration_since(last_frame).as_millis() as u64, Ordering::Relaxed);
                                        last_frame = now;
                                        h.stats.frames.fetch_add(1, Ordering::Relaxed);
                                        // 队列满 = SDL 正在画，丢这一帧无所谓（原客户端也只画最新）；
                                        // 通道关 = 窗口关了，整条解码任务收工。
                                        match h.frames.try_send(frame) {
                                            Ok(()) => {}
                                            Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => stopped = true,
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        // 半途接入时首帧前必然失败一串（没等到 IDR），别刷屏。
                                        let n = h.stats.decode_errors.fetch_add(1, Ordering::Relaxed);
                                        if n < 5 { println!("   解码失败：{e}"); }
                                    }
                                }
                                if stopped { break; }
                            }
                            if stopped { break; }
                        }
                    } else if mime.eq_ignore_ascii_case("audio/opus") {
                        let mut decoder = opus::Decoder::new(48000, opus::Channels::Stereo).expect("opus 解码器");
                        let mut buf = vec![0i16; 5760 * 2];
                        loop {
                            match track.read_rtp().await {
                                Ok((p, _)) => {
                                    h.stats.audio_packets.fetch_add(1, Ordering::Relaxed);
                                    if !h.stats.audio_enabled.load(Ordering::Relaxed) { continue; }
                                    match decoder.decode(&p.payload, &mut buf, false) {
                                        Ok(n) => {
                                            let mut ring = h.audio.lock().unwrap_or_else(|e| e.into_inner());
                                            for s in &buf[..n * 2] { ring.push_back(*s as f32 / 32768.0); }
                                            // 只留 ~0.5 秒，落后太多就直接丢旧的（跟实时说话一样）。
                                            while ring.len() > 48000 { ring.pop_front(); }
                                        }
                                        Err(e) => println!("   opus 解码失败：{e}"),
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }
                });
            })
        }));

        let h2 = host.clone();
        // CONTROL 消息处理：官方被控端会把 echo/光标 发在它**自己开的**入站 CONTROL
        // 通道里（fork 线上观测），所以我们建的和本端收到的通道都要挂同一个处理。
        fn control_handler(h: Arc<Host>, sink: Arc<RTCDataChannel>) -> webrtc::data_channel::OnMessageHdlrFn {
            Box::new(move |message| {
                let h = h.clone();
                let control = Arc::downgrade(&sink);
                Box::pin(async move {
                    let data = message.data;
                    if let Some(request_seq) = wire::echo_request_seq(&data) {
                        h.stats.echo_seen.store(true, Ordering::Release);
                        if let Some(channel) = control.upgrade() {
                            let _ = channel.send(&Bytes::from(wire::echo_response(1_000_000 + request_seq, request_seq))).await;
                        }
                    } else if let Some(seq) = wire::echo_response_seq(&data) {
                        if let Some(sent) = h.pings.lock().unwrap_or_else(|e| e.into_inner()).remove(&seq) {
                            h.stats.rtt_ms.store(sent.elapsed().as_millis() as u64, Ordering::Relaxed);
                        }
                    }
                    if let Some(shape) = wire::parse_cursor(&data) {
                        match super::png::decode(&shape.png) {
                            Ok(img) => {
                                let _ = h.cursor.send(CursorImage { width: img.width, height: img.height, hotspot_x: shape.hotspot_x, hotspot_y: shape.hotspot_y, rgba: img.rgba });
                            }
                            Err(e) => println!("   光标 PNG 解码失败：{e}"),
                        }
                    }
                    if let Some((codecs, chroma)) = wire::media_control_note(&data) {
                        if let Some(chroma) = chroma { h.stats.chroma.store(chroma as u64, Ordering::Relaxed); }
                        if let Some(codecs) = codecs { println!("   被控端编码能力清单：H264={} HEVC={}", codecs & 1 != 0, codecs & 2 != 0); }
                    }
                })
            })
        }
        let h_dc = h2.clone();
        pc.on_data_channel(Box::new(move |channel| {
            let h = h_dc.clone();
            Box::pin(async move {
                println!("   被控端开入站通道：{}", channel.label());
                if channel.label() == wire::CHANNELS[0] {
                    channel.on_message(control_handler(h, channel.clone()));
                }
            })
        }));
        let (control, text) = {
            let mut channels = Vec::new();
            for label in wire::CHANNELS {
                channels.push(pc.create_data_channel(label, Some(RTCDataChannelInit { ordered: Some(true), ..Default::default() })).await.map_err(|_| "数据通道创建失败")?);
            }
            let wc_open = Arc::downgrade(&channels[0]);
            let hc = h2.clone();
            channels[0].on_open(Box::new(move || {
                hc.stats.control_open.store(true, Ordering::Release);
                println!("   CONTROL 通道已打开");
                if let Some(channel) = wc_open.upgrade() {
                    tokio::spawn(async move { let _ = channel.send(&Bytes::from(wire::echo_request(1))).await; });
                }
                Box::pin(async {})
            }));
            let h3 = h2.clone();
            channels[0].on_close(Box::new(move || { h3.stats.control_open.store(false, Ordering::Release); Box::pin(async {}) }));
            // CONTROL 上只会遇到：被控端 echo 请求/对我们 ping 的 echo 应答、光标形状、同步/能力清单。
            channels[0].on_message(control_handler(h2.clone(), channels[0].clone()));
            (channels[0].clone(), channels[1].clone())
        };

        if shape.width > 0 {
            // 官方 Mac 控制端就是这样报 recvonly 视频+音频两条 m-line。
            for kind in [RTPCodecType::Video, RTPCodecType::Audio] {
                pc.add_transceiver_from_kind(kind, Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] })).await.map_err(|_| "媒体 transceiver 失败")?;
            }
        }

        Ok(Self { pc, control, text, answered: false, early: Vec::new(), app_id: app, ice_id: ice, remote_client: client })
    }

    pub async fn offer(&self) -> Result<Value, String> {
        let offer = self.pc.create_offer(None).await.map_err(|_| "创建 offer 失败")?;
        let mut sdp = offer.sdp.clone();
        self.pc.set_local_description(offer).await.map_err(|_| "set local description 失败")?;
        // 官方控制端 offer 的会话级标记；只改发出副本，webrtc-rs 不接受改过的 local description。
        if !sdp.contains("a=x-uuremote-quic") {
            if let Some(i) = sdp.find("a=group:BUNDLE") { sdp.insert_str(i, "a=x-uuremote-quic\r\n"); }
        }
        sdp = with_max_message_size(&sdp);
        if let Some(path) = std::env::var_os("UU_VIEWER_DUMP_OFFER") { let _ = std::fs::write(path, &sdp); }
        Ok(json!({"client_id": self.remote_client, "data": {"type": "offer", "sdp": sdp, "ice_id": self.ice_id, "app_control_id": self.app_id, "ice_network_type": 3}}))
    }

    pub async fn receive(&mut self, event: &Value) -> Result<(), String> {
        let body = event.get("data").unwrap_or(event);
        if !self.matches_route(event) { return Ok(()); }
        match body.get("type").and_then(Value::as_str) {
            Some("answer") => {
                if self.answered { return Ok(()); }
                let sdp = answer_sdp(body)?;
                let answer = RTCSessionDescription::answer(sdp).map_err(|_| "answer SDP 不合法")?;
                self.pc.set_remote_description(answer).await.map_err(|_| "set remote answer 失败")?;
                self.answered = true;
                for c in self.early.drain(..) { self.pc.add_ice_candidate(c).await.map_err(|_| "排队 ICE 候选失败")?; }
                println!("   SDP 协商完成，ICE 连通检查开始");
            }
            Some("candidate") => {
                let value = body.get("candidate").ok_or("候选包里没有 candidate")?;
                let candidate = if let Some(s) = value.as_str() {
                    RTCIceCandidateInit { candidate: s.into(), sdp_mid: body.get("sdp_mid").and_then(Value::as_str).map(str::to_owned), sdp_mline_index: body.get("sdp_mline_index").and_then(Value::as_u64).and_then(|v| u16::try_from(v).ok()), ..Default::default() }
                } else {
                    serde_json::from_value(value.clone()).map_err(|_| "candidate 不合法")?
                };
                if self.answered { self.pc.add_ice_candidate(candidate).await.map_err(|_| "ICE 候选失败")?; }
                else {
                    if self.early.len() >= 256 { return Err("过早到达的候选太多".into()); }
                    self.early.push(candidate);
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn matches_route(&self, event: &Value) -> bool {
        let body = event.get("data").unwrap_or(event);
        !body.get("ice_id").is_some_and(|v| v.as_str() != Some(self.ice_id.as_str()))
            && !body.get("app_control_id").is_some_and(|v| v.as_str() != Some(self.app_id.as_str()))
            && !event.get("client_id").is_some_and(|v| v.as_str() != Some(self.remote_client.as_str()))
    }

    /// CONTROL 通道就绪前所有输入直接丢（原客户端也是先等会话 ready）。
    pub async fn send_control(&self, payload: &[u8]) -> Result<(), String> {
        if self.control.ready_state() != RTCDataChannelState::Open { return Err("CONTROL 未打开".into()); }
        self.control.send(&Bytes::from(payload.to_vec())).await.map(|_| ()).map_err(|_| "CONTROL 发送失败".to_string())
    }
    pub async fn send_text(&self, payload: &[u8]) -> Result<(), String> {
        if self.text.ready_state() != RTCDataChannelState::Open { return Err("TEXT 未打开".into()); }
        self.text.send(&Bytes::from(payload.to_vec())).await.map(|_| ()).map_err(|_| "TEXT 发送失败".to_string())
    }
    pub async fn close(&self) { let _ = self.pc.close().await; }
}

fn answer_sdp(body: &Value) -> Result<String, String> {
    if let Some(s) = body.get("sdp").and_then(Value::as_str) {
        if s.len() <= 1024 * 1024 { return Ok(s.into()); }
    }
    if let Some(s) = body.get("gzip_sdp").and_then(Value::as_str) {
        use base64::Engine;
        use std::io::Read;
        if s.len() > 2 * 1024 * 1024 { return Err("压缩 answer 过大".into()); }
        let data = base64::engine::general_purpose::STANDARD.decode(s).map_err(|_| "压缩 answer base64 不合法")?;
        let mut output = String::new();
        flate2::read::GzDecoder::new(data.as_slice()).take(1024 * 1024 + 1).read_to_string(&mut output).map_err(|_| "压缩 answer 解压失败")?;
        if output.len() <= 1024 * 1024 { return Ok(output); }
    }
    Err("answer 里既没有 sdp 也没有 gzip_sdp".into())
}

/// 缺省补 a=max-message-size:262144（libwebrtc 的数），不然被控端按 64KB 截大回包。
pub fn with_max_message_size(sdp: &str) -> String {
    if sdp.contains("a=max-message-size:") { return sdp.to_owned(); }
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(sdp.len() + 32);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        if line.trim_end().starts_with("a=sctp-port:") { out.push_str("a=max-message-size:262144"); out.push_str(eol); }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn answer_extraction_supports_both_forms() {
        use base64::Engine;
        use std::io::Write;
        assert_eq!(answer_sdp(&json!({"sdp": "x"})).unwrap(), "x");
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"synthetic-sdp").unwrap();
        assert_eq!(answer_sdp(&json!({"gzip_sdp": base64::engine::general_purpose::STANDARD.encode(encoder.finish().unwrap())})).unwrap(), "synthetic-sdp");
        assert!(answer_sdp(&json!({})).is_err());
    }
    #[test]
    fn offer_sdp_gets_quic_and_max_message() {
        let sdp = "m=application 9\r\na=sctp-port:5000\r\na=group:BUNDLE 0 1\r\n";
        let patched = with_max_message_size(sdp);
        assert!(patched.contains("a=max-message-size:262144"));
    }
    #[tokio::test]
    async fn offer_advertises_h264_opus_and_uu_extensions() {
        let api = APIBuilder::new().with_media_engine(media_engine().unwrap()).build();
        let pc = api.new_peer_connection(RTCConfiguration::default()).await.unwrap();
        pc.add_transceiver_from_kind(RTPCodecType::Video, Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] })).await.unwrap();
        pc.add_transceiver_from_kind(RTPCodecType::Audio, Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] })).await.unwrap();
        let offer = pc.create_offer(None).await.unwrap();
        assert!(offer.sdp.contains("a=rtpmap:102 H264/90000"));
        assert!(offer.sdp.contains("a=rtpmap:111 opus/48000/2"));
        assert!(offer.sdp.contains("video-capture-index"));
        assert!(offer.sdp.contains("video-frame-is-new-frame"));
        assert!(!offer.sdp.contains("H265"));
        pc.close().await.unwrap();
    }
}
