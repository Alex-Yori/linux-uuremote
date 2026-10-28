//! uu-selftest —— 本地回环端到端验收：真 DTLS / SRTP / SCTP，只是不经云。
//!
//! 一条进程里跑两端：控制器（offerer，收视频 + 建 CONTROL 通道）与宿主（answerer，
//! `linux_uuremote::transport::Peer`）。SDP 直接内存交换，跳过信令；其余全部走真实栈。
//! 验四件事，任何一件不成立就非零退出：
//!   1. 视频协商成功（H.264），画面字节真的到达控制器；
//!   2. CONTROL 通道双向可用：宿主发的初始化 protobuf 控制器能解出 which=7/8；
//!   3. 控制器发的 CONTROL 输入 JSON 宿主能解析成注入事件；
//!   4. 两端连接状态都到 connected。
//!
//! 用法：`uu-selftest [--display :1] [--frames 8]`。给了 DISPLAY 就用真实桌面帧，
//! 否则退回合成渐变帧（一样能验传输）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use linux_uuremote::encoder::{OpenH264, FrameKind};
use linux_uuremote::control::decode_control;
use linux_uuremote::initialization::{encode_capture_change, encode_permissions, encode_secure_desktop, encode_screen_sources, Permissions, Screen, ScreenGeometry};
use linux_uuremote::transport::{wait_gathering_complete, Peer, PeerEvent, CONTROL_LABEL};
use linux_uuremote::wire::{decode_wire, WireValue};
use serde_json::json;
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::track::track_remote::TrackRemote;

/// 控制器收到的视频载荷字节数与包数。
#[derive(Default)]
struct VideoCount {
    bytes: AtomicU64,
    packets: AtomicU64,
}

fn main() {
    let mut display: Option<String> = None;
    let mut frames: u32 = 8;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--display" => display = args.next(),
            "--frames" => frames = args.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(frames),
            "--help" | "-h" => {
                println!("用法: uu-selftest [--display :1] [--frames 8]");
                return;
            }
            other => eprintln!("未知参数 {other}"),
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    match rt.block_on(run(display, frames)) {
        Ok(()) => {
            println!("SELFTEST PASS：回环 WebRTC 全链路可用");
        }
        Err(e) => {
            eprintln!("SELFTEST FAIL：{e}");
            std::process::exit(1);
        }
    }
}

async fn run(display: Option<String>, frames: u32) -> Result<(), String> {
    // ---------- 控制器（模拟官方客户端：offerer，收视频，建 CONTROL） ----------
    let mut media = MediaEngine::default();
    media.register_default_codecs().map_err(|e| format!("控制器注册编解码失败: {e}"))?;
    let registry = register_default_interceptors(webrtc::interceptor::registry::Registry::new(), &mut media)
        .map_err(|e| format!("控制器注册拦截器失败: {e}"))?;
    let mut settings = SettingEngine::default();
    // 回环两端同机：关 mDNS，candidate 才是可直连的明文 IP；且必须收 LOOPBACK 候选。
    settings.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    let api = APIBuilder::new().with_media_engine(media).with_setting_engine(settings).with_interceptor_registry(registry).build();
    let controller = Arc::new(api.new_peer_connection(RTCConfiguration { ice_servers: vec![], ..Default::default() }).await.map_err(|e| format!("控制器建 PeerConnection 失败: {e}"))?);

    let video_count = Arc::new(VideoCount::default());
    let codec_mime = Arc::new(std::sync::Mutex::new(String::new()));
    let reader_count = video_count.clone();
    let reader_mime = codec_mime.clone();
    controller.on_track(Box::new(move |track: Arc<TrackRemote>, _receiver, _transceiver| {
        let count = reader_count.clone();
        let mime = reader_mime.clone();
        Box::pin(async move {
            *mime.lock().unwrap() = track.codec().capability.mime_type.clone();
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match track.read(&mut buffer).await {
                    Ok((packet, _)) => {
                        count.bytes.fetch_add(packet.payload.len() as u64, Ordering::Relaxed);
                        count.packets.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
        })
    }));
    controller
        .add_transceiver_from_kind(
            RTPCodecType::Video,
            // sendrecv 对齐官方客户端的 offer（libwebrtc 默认就是 sendrecv）：
            // recvonly 会让本端 send track 协商不到 codec，set_local_description 直接失败。
            Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Sendrecv, send_encodings: vec![] }),
        )
        .await
        .map_err(|e| format!("控制器加视频收流失败: {e}"))?;

    let (channel_open_tx, channel_open_rx) = tokio::sync::oneshot::channel::<()>();
    let (control_rx_tx, mut control_rx_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let open_flag = Arc::new(AtomicBool::new(false));
    // oneshot::Sender 不是 Clone，而回调是 FnMut：用 Mutex 让 on_open 只取一次。
    let open_tx = Arc::new(std::sync::Mutex::new(Some(channel_open_tx)));

    /// 把一条 CONTROL 通道接到验收用的 oneshot/mpsc 上。
    /// 自己 create 出来的通道不会走 on_data_channel，必须直接挂回调——
    /// 只挂 on_data_channel 的话控制器永远等不到 open。
    fn wire_channel(
        channel: &Arc<RTCDataChannel>,
        flag: Arc<AtomicBool>,
        messages: mpsc::UnboundedSender<Vec<u8>>,
        opened: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    ) {
        if channel.label() != CONTROL_LABEL {
            return;
        }
        flag.store(true, Ordering::Relaxed);
        channel.on_open(Box::new(move || {
            Box::pin(async move {
                if let Some(sender) = opened.lock().unwrap().take() {
                    let _ = sender.send(());
                }
            })
        }));
        channel.on_message(Box::new(move |msg: DataChannelMessage| {
            let messages = messages.clone();
            let data = msg.data.to_vec();
            Box::pin(async move {
                let _ = messages.send(data);
            })
        }));
    }

    // 对端（真客户端）建的通道走这条路。回调是 FnMut，所以捕的是副本。
    controller.on_data_channel(Box::new({
        let flag = open_flag.clone();
        let messages = control_rx_tx.clone();
        let opened = open_tx.clone();
        move |d: Arc<RTCDataChannel>| {
            wire_channel(&d, flag.clone(), messages.clone(), opened.clone());
            Box::pin(async move {})
        }
    }));
    let channel = controller
        .create_data_channel(CONTROL_LABEL, Some(RTCDataChannelInit { ordered: Some(true), ..Default::default() }))
        .await
        .map_err(|e| format!("控制器建 CONTROL 通道失败: {e}"))?;
    wire_channel(&channel, open_flag.clone(), control_rx_tx.clone(), open_tx.clone());

    let offer = controller.create_offer(None).await.map_err(|e| format!("控制器 create_offer 失败: {e}"))?;
    controller.set_local_description(offer.clone()).await.map_err(|e| format!("控制器 set_local_description 失败: {e}"))?;
    if !wait_gathering_complete(&controller, Duration::from_secs(8)).await {
        return Err("控制器 ICE 收集超时".into());
    }
    // create_offer 时还没收集 candidate；收集完再从 current_local_description 拿，才带得上本机地址。
    let offer_sdp = controller.current_local_description().await.map(|d| d.sdp).unwrap_or_else(|| offer.sdp.clone());
    println!("   控制器 offer：{} 字节，video 行 {} 条", offer_sdp.len(), offer_sdp.lines().filter(|l| l.starts_with("m=video")).count());
    // 调试：H.264 的 payload 与 fmtp 必须和我们的 track 能力对得上，否则 set_local_description 直接拒。
    for line in offer_sdp
        .lines()
        .filter(|l| l.to_lowercase().contains("h264") || l.starts_with("m=video") || l.starts_with("a=fmtp:") || l.starts_with("a=rtcp-fallback"))
    {
        println!("   offer: {line}");
    }

    // ---------- 宿主（我们的被控端） ----------
    let (host_tx, mut host_rx) = mpsc::unbounded_channel::<PeerEvent>();
    // 回环不需要服务器：显式空列表，绕开公共 STUN 的收集等待。
    let host = Peer::new(RTCConfiguration { ice_servers: vec![], ..Default::default() }, host_tx).await?;
    let answer = host.answer_offer(&offer_sdp).await?;
    println!("   宿主 answer：{} 字节，candidate 行 {} 条", answer.len(), answer.lines().filter(|l| l.contains("a=candidate")).count());
    for line in answer.lines().filter(|l| l.starts_with("m=") || l.starts_with("a=sctp-port") || l.starts_with("a=setup")) {
        println!("   answer: {line}");
    }
    controller.set_remote_description(RTCSessionDescription::answer(answer).map_err(|e| format!("answer 不合法: {e}"))?).await.map_err(|e| format!("控制器 set_remote_description 失败: {e}"))?;

    // ---------- 等两端连上 + CONTROL 打开 ----------
    let connected = Instant::now();
    let mut host_connected = false;
    let mut controller_connected = false;
    while (!host_connected || !controller_connected) && connected.elapsed() < Duration::from_secs(10) {
        host_connected |= matches!(host.connection_state(), RTCPeerConnectionState::Connected);
        controller_connected |= matches!(controller.connection_state(), RTCPeerConnectionState::Connected);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    if !host_connected {
        return Err(format!("宿主没连上（状态 {:?}）", host.connection_state()));
    }
    if !controller_connected {
        return Err(format!("控制器没连上（状态 {:?}）", controller.connection_state()));
    }
    println!("   DTLS/SRTP 握手完成：两端 connected（{:.1} s）", connected.elapsed().as_secs_f64());
    if tokio::time::timeout(Duration::from_secs(10), channel_open_rx).await.is_err() {
        return Err("CONTROL 通道 10 s 内没打开".into());
    }
    let mut host_open = false;
    for _ in 0..40 {
        if let Ok(event) = host_rx.try_recv() {
            if matches!(event, PeerEvent::ControlOpen { .. }) {
                host_open = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !host_open {
        return Err("宿主侧没收到 CONTROL 通道 open".into());
    }
    println!("   CONTROL 通道双向 open");

    // ---------- 宿主 → 控制器：初始化 protobuf ----------
    let geometry = (1280u32, 720u32);
    let messages = vec![
        encode_capture_change(1, 2, 0, 0, "Linux desktop").unwrap(),
        encode_screen_sources(
            3,
            4,
            &[Screen {
                id: 0,
                fps: 30,
                resolution: ScreenGeometry { left: 0, top: 0, width: geometry.0 as i32, height: geometry.1 as i32, pixel_width: geometry.0 as i32, pixel_height: geometry.1 as i32 },
                primary: true,
                dpr: 1.0,
                display_name: "",
                video_track_index: 0,
                screen_type: 0,
                builtin_screen_type: 0,
                scaled: &[],
                current: None,
            }],
            0,
        )
        .unwrap(),
        encode_permissions(5, 6, Permissions { assistant: false, video_capture_allowed: true, capture_content_visible: true, audio_capture_allowed: false, floating_ball: false }),
        encode_secure_desktop(7, 8, false, false, false),
    ];
    for message in &messages {
        host.send_control(message).await?;
    }
    let mut seen = vec![];
    for _ in 0..messages.len() {
        match tokio::time::timeout(Duration::from_secs(5), control_rx_rx.recv()).await {
            Ok(Some(data)) => seen.push(data),
            _ => break,
        }
    }
    if seen.len() != messages.len() {
        return Err(format!("初始化报文只到 {} 条（发了 {} 条）", seen.len(), messages.len()));
    }
    let which_seen: Vec<u32> = seen
        .iter()
        .map(|data| {
            decode_wire(data)
                .map_err(|_| "初始化报文不是合法 protobuf")
                .map(|fields| fields.iter().filter(|f| f.number > 2).map(|f| f.number).collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>, _>>()?
        .concat();
    for which in [7u32, 8, 15] {
        if !which_seen.contains(&which) {
            return Err(format!("控制器收到的初始化报文里没有 which={which}（收到 {which_seen:?}）"));
        }
    }
    let (width_px, height_px) = geometry;
    // 屏幕清单里的实测宽高必须原样到达，否则客户端会把画面贴错尺寸。
    let sources = seen
        .iter()
        .find(|data| decode_wire(data).map(|f| f.iter().any(|x| x.number == 7)).unwrap_or(false))
        .ok_or("没有 ScreenSources 报文")?;
    let nested = decode_wire(sources).unwrap().into_iter().find(|f| f.number == 7).unwrap();
    let WireValue::Bytes(inner) = &nested.value else { return Err("ScreenSources 不是嵌套消息".into()) };
    let screen = decode_wire(inner).unwrap().into_iter().find(|f| f.number == 1).unwrap();
    let WireValue::Bytes(body) = &screen.value else { return Err("Screen 不是嵌套消息".into()) };
    // 宽高在 Screen 的 resolution 里再套一层 WinRect（field 3/4 是 WinRect 的 width/height），
    // 不是 Screen 自己的字段——照 fork 的结构剥到底。
    let resolution = decode_wire(body).unwrap().into_iter().find(|f| f.number == 3).unwrap();
    let WireValue::Bytes(rect) = &resolution.value else { return Err("Screen.resolution 不是嵌套消息".into()) };
    let dims: Vec<u64> = decode_wire(rect)
        .unwrap()
        .into_iter()
        .filter_map(|f| match (&f.value, f.number) { (WireValue::Varint(v), 3 | 4) => Some(*v), _ => None })
        .collect();
    if dims != vec![geometry.0 as u64, geometry.1 as u64] {
        return Err(format!("屏幕宽高传错了：期望 [{}, {}]，实际 {dims:?}", geometry.0, geometry.1));
    }
    println!("   初始化 protobuf 到达控制器：which={which_seen:?}，屏幕 {width_px}x{height_px}");

    // ---------- 控制器 → 宿主：CONTROL 输入 JSON ----------
    channel
        .send(&bytes::Bytes::from_static(br#"{"action":"mouse_move_absolute","abs_x":640,"abs_y":360}"#))
        .await
        .map_err(|e| format!("控制器发 CONTROL 失败: {e}"))?;
    let mut got_input = None;
    for _ in 0..60 {
        if let Ok(PeerEvent::ControlData { data, .. }) = host_rx.try_recv() {
            got_input = Some(decode_control(&data)?);
            break;
        }
        if let Ok(PeerEvent::KeyframeRequest) = host_rx.try_recv() {
            continue;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let input = got_input.ok_or("宿主没收到 CONTROL 输入")?;
    println!("   宿主解析到输入事件：{input:?}");

    // ---------- 宿主 → 控制器：真画面 ----------
    // 真实桌面帧可选：有 DISPLAY 就抓整屏，没有就用合成渐变——两者走完全相同的传输路径。
    let mut real = match display.as_deref() {
        Some(d) => linux_uuremote::capture::x11::X11Screen::open(Some(d), linux_uuremote::capture::x11::DamageLevel::BoundingBox).ok(),
        None => None,
    };
    // 编码器尺寸必须等于喂进去的帧尺寸（奇数屏裁 1 像素对齐 4:2:0），否则行错位、画面全花。
    let (width, height) = match real.as_ref() {
        Some(screen) => (usize::from(screen.width()) & !1, usize::from(screen.height()) & !1),
        None => (640usize, 360usize),
    };
    let mut encoder = OpenH264::new(width, height, 30, 0, 60, 4)?;
    let started = Instant::now();
    let mut idr = 0;
    let mut p = 0;
    for index in 0..frames {
        let mut frame = vec![0u8; width * height * 4];
        if let Some(screen) = real.as_mut() {
            let (grabbed, _) = screen.grab_rect(None).map_err(|e| format!("抓屏失败: {}", e.0))?;
            let take = frame.len().min(grabbed.len());
            frame.copy_from_slice(&grabbed[..take]);
        } else {
            for y in 0..height {
                for x in 0..width {
                    let offset = (y * width + x) * 4;
                    frame[offset] = (x * 3 + index as usize * 7) as u8;
                    frame[offset + 1] = (y * 2) as u8;
                    frame[offset + 2] = 128;
                    frame[offset + 3] = 255;
                }
            }
        }
        let (access_unit, kind) = encoder.encode_bgra(&frame, index == 0)?;
        match kind {
            FrameKind::Idr => idr += 1,
            FrameKind::Delta => p += 1,
            FrameKind::Skipped => {}
        }
        host.write_video(access_unit, Duration::from_millis(33)).await?;
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
    let mut waited = 0;
    while video_count.packets.load(Ordering::Relaxed) == 0 && waited < 60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 1;
    }
    let bytes = video_count.bytes.load(Ordering::Relaxed);
    let packets = video_count.packets.load(Ordering::Relaxed);
    let mime = codec_mime.lock().unwrap().clone();
    if packets == 0 || bytes == 0 {
        return Err("控制器一条 RTP 都没收到".into());
    }
    // webrtc-rs 报的是 SDP 名字 "video/H264"，大小写敏感比较会漏。
    if !mime.eq_ignore_ascii_case("video/H264") && !mime.eq_ignore_ascii_case("video/avc") {
        return Err(format!("协商到的不是 H.264（拿到 {mime}）"));
    }
    println!(
        "   视频到达控制器：{packets} 个 RTP 包 / {bytes} 字节，codec={mime}，IDR {idr} P {p}，{:.1} s",
        started.elapsed().as_secs_f64()
    );

    // ---------- 反向：客户端的 FIR/PLI 应当触发关键帧请求 ----------
    let mut saw_rtcp = false;
    for _ in 0..40 {
        match host_rx.try_recv() {
            Ok(PeerEvent::KeyframeRequest) => {
                saw_rtcp = true;
                break;
            }
            Ok(PeerEvent::ReceiverReport { .. }) => {
                saw_rtcp = true;
                break;
            }
            Ok(_) => {}
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    println!(
        "   RTCP 通道 {}：{}",
        if saw_rtcp { "有反馈" } else { "暂未收到（回环无损，正常）" },
        json!({"frames": frames, "video_bytes": bytes}).to_string()
    );

    // 收尾顺序：先关发起方再关被控端。反向会撞上 SCTP 已散架，报
    // "sending reset packet in non-Established state"——那是拆机噪声，不是链路故障。
    if let Err(e) = controller.close().await {
        println!("   关控制器：{e}（连接已验证，忽略）");
    }
    host.close().await;
    Ok(())
}
