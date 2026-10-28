//! uuremote-host —— UU 远程 Linux 被控端宿主。
//!
//! 一条链路串起来：信令（socket.io）→ 等客户端拉流（`be-controlled`）→ WebRTC offer/answer
//! （`soac`）→ 采整机画面 → 内容门控 → openh264 → 视频轨；反方向 CONTROL 通道 JSON → XTest 注入。
//!
//! 用法：`uuremote-host [--display :1] [--fps 30] [--still-fps 3] [--bitrate 0] [--gop 60]`
//! 登录态来自 `uu-login`（私有配置 0600）；token 过期直接报出来让人重扫。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use linux_uuremote::api::{ApiClient, Credentials, RoomError};
use linux_uuremote::capture::x11::{DamageLevel, X11Screen};
use linux_uuremote::config;
use linux_uuremote::content::ContentChange;
use linux_uuremote::capability;
use linux_uuremote::control::{
    action_name, decode_control, encode_capture_setting_sync, encode_echo_response, encode_rpc_response,
    preset_bitrate_kbps, ControlMessage, RpcRequest,
};
use linux_uuremote::encoder::{FrameKind, FramePlanner, OpenH264};
use linux_uuremote::gating::{Action, FrameDropper, GateConfig};
use linux_uuremote::initialization::{encode_capture_change, encode_permissions, encode_secure_desktop, encode_screen_sources, Permissions, Screen, ScreenGeometry};
use linux_uuremote::input::X11Input;
use linux_uuremote::signaling::{self, Received, Signaling};
use linux_uuremote::transport::{decode_offer_sdp, ice_configuration, ice_servers_from_json, Peer, PeerEvent, CONTROL_LABEL};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

/// 一条待发送的视频帧（Annex-B 访问单元 + 时长）。
struct Encoded {
    data: Vec<u8>,
    duration: Duration,
}

/// 信令侧事件（主循环看得懂的形式）。
enum Signal {
    IceServers(Value),
    /// 客户端发起被控：header 里带着这次会议的 ICE 配置与采集目标。
    BeControlled {
        header: Value,
        capture_type: u64,
    },
    Offer {
        sdp: String,
        ice_id: Option<Value>,
        client_id: Option<String>,
        app_control_id: Option<Value>,
    },
    Candidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    PeerLeft,
    Closed(String),
}

/// 一次被控会话：PeerConnection + 注入引擎 + 关键帧请求标志 + 采集线程。
struct Session {
    peer: Arc<Peer>,
    keyframe: Arc<AtomicBool>,
    input: Option<Arc<std::sync::Mutex<X11Input>>>,
    capture: Option<std::thread::JoinHandle<()>>,
    /// 这次 soac 会话的路由字段（ice_id / client_id / app_control_id）：
    /// trickle candidate 时必须原样带回，否则云端不知道发给哪个客户端。
    route: Value,
    /// 诊断用：本会话收到过几条 CONTROL 报文（前几条打形状，查"鼠标没反应"）。
    control_msgs: u32,
    /// 是否已发出 answer（远端描述已设）：早到 candidate 要等它才能 add。
    answered: bool,
}

/// 远程可调的采集参数。码率变化触发编码器重建 + IDR；分辨率只支持原生，降档显式拒绝。
struct RemoteSettings {
    moving_fps: AtomicU64,
    still_fps: AtomicU64,
    /// 目标码率 kbps；0 = 按尺寸自动。
    bitrate_kbps: AtomicU64,
    quality: AtomicU64,
    custom_bps: AtomicU64,
    /// 实测原生尺寸（采集线程每轮刷新；RPC 回复与 Sync 报真实值）。
    native: std::sync::Mutex<(u32, u32)>,
}

impl RemoteSettings {
    fn new(fps: u32, still_fps: u32) -> Self {
        Self {
            moving_fps: AtomicU64::new(fps.max(1) as u64),
            still_fps: AtomicU64::new(still_fps as u64),
            bitrate_kbps: AtomicU64::new(0),
            quality: AtomicU64::new(5),
            custom_bps: AtomicU64::new(0),
            native: std::sync::Mutex::new((0, 0)),
        }
    }
}

fn main() {
    let mut display: Option<String> = None;
    let mut fps: u32 = 30;
    let mut still_fps: u32 = 3;
    let mut bitrate_kbps: u32 = 0;
    let mut gop: u32 = 60;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut number = || args.next().and_then(|v| v.parse::<u32>().ok());
        match arg.as_str() {
            "--display" => display = args.next(),
            "--fps" => fps = number().unwrap_or(fps),
            "--still-fps" => still_fps = number().unwrap_or(still_fps),
            "--bitrate" => bitrate_kbps = number().unwrap_or(bitrate_kbps),
            "--gop" => gop = number().unwrap_or(gop),
            "--help" | "-h" => {
                println!("用法: uuremote-host [--display :1] [--fps 30] [--still-fps 3] [--bitrate kbps] [--gop 帧数]");
                return;
            }
            other => eprintln!("未知参数 {other}"),
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = rt.block_on(run(display, fps, still_fps, bitrate_kbps, gop)) {
        eprintln!("宿主退出：{e}");
        std::process::exit(1);
    }
}

async fn run(display: Option<String>, fps: u32, still_fps: u32, bitrate_kbps: u32, gop: u32) -> Result<(), String> {
    let (file, borrowed) = config::read_with_fallback()?;
    println!("登录态：{}", if borrowed { "借用参照实现的配置" } else { "本机私有配置" });
    let creds = Credentials::load(file.as_ref())?;
    let api = ApiClient::new(creds);

    println!("建房…");
    let room = api.create_room().await.map_err(|e| match e {
        RoomError::LoginExpired => "登录 token 已失效：重跑 uu-login 扫码。".to_string(),
        RoomError::Other(msg) => msg,
    })?;
    println!("   signaling_server = {}", room.signaling_server);

    let mut sig = Signaling::connect(&room.signaling_server, &room.token, room.ws_connect_timeout_ms).await?;
    println!("   CONNECT_ACK ✓");
    sig.send_with_ack(10, signaling::EVENT_ROOM_INFO, None).await?;

    let (sig_tx, mut sig_rx) = mpsc::unbounded_channel::<Signal>();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let (emit_tx, mut emit_rx) = mpsc::unbounded_channel::<(String, Option<Value>)>();

    // 信令 IO 独占 socket：收帧转事件、发帧走队列，心跳必须一直跑（半开 TCP 靠判死窗口断）。
    tokio::spawn(async move {
        loop {
            tokio::select! {
                out = emit_rx.recv() => match out {
                    Some((event, payload)) => {
                        if let Err(e) = sig.send(&event, payload.as_ref()).await {
                            eprintln!("信令发送失败：{e}");
                            let _ = sig_tx.send(Signal::Closed(e));
                            break;
                        }
                    }
                    None => break,
                },
                got = sig.recv() => match got {
                    Ok(Received::Event(name, payload)) => {
                        let v = payload.unwrap_or(Value::Null);
                        // 全量信令事件日志：第一次点连不上、要连十多秒这类问题的根因就在
                        // 「客户端到底发了什么」——未知事件不能静默吞掉。
                        let kind = v.get("type").and_then(Value::as_str).unwrap_or("-");
                        println!("   信令事件：{name} type={kind}");
                        match name.as_str() {
                            signaling::EVENT_SOAC => forward_soac(&sig_tx, v),
                            signaling::EVENT_BE_CONTROLLED => { let _ = sig_tx.send(Signal::BeControlled { header: v, capture_type: 0 }); }
                            signaling::EVENT_RELEASED | signaling::EVENT_LEFT => { let _ = sig_tx.send(Signal::PeerLeft); }
                            _ => {}
                        }
                    }
                    Ok(Received::Ack(id, v)) => {
                        let brief = v.to_string();
                        println!("   信令 ack id={id:?}：{}", &brief[..brief.len().min(120)]);
                        let _ = sig_tx.send(Signal::IceServers(v));
                    }
                    Ok(Received::BinaryEvent(name, header, attachments)) => {
                        // be-controlled 的关键信息在附件的 protobuf 里：field 1 = 采集目标
                        // （0 未知 / 1 桌面 / 2 窗口 / 5 文件 / 8 终端 / 9 端口映射）。
                        let capture_type = attachments
                            .iter()
                            .find_map(|bytes| linux_uuremote::wire::decode_wire(bytes).ok())
                            .map(|fields| fields.iter().find(|f| f.number == 1).and_then(|f| match &f.value { linux_uuremote::wire::WireValue::Varint(v) => Some(*v), _ => None }))
                            .flatten()
                            .unwrap_or(0);
                        let header = header.unwrap_or(Value::Null);
                        println!("   {name}(binary)：附件 {} 段 capture_type={capture_type}", attachments.len());
                        if name == signaling::EVENT_BE_CONTROLLED {
                            let _ = sig_tx.send(Signal::BeControlled { header, capture_type });
                        }
                    }
                    Ok(Received::Closed) => { let _ = sig_tx.send(Signal::Closed("信令关闭".into())); break; }
                    Err(e) => { let _ = sig_tx.send(Signal::Closed(e)); break; }
                }
            }
        }
    });

    let settings = Arc::new(RemoteSettings::new(fps, still_fps));
    let mut session: Option<Session> = None;
    let mut early_candidates: Vec<(String, Option<String>, Option<u16>)> = Vec::new();

    println!("等待客户端拉流（Ctrl-C 退出）…");
    loop {
        tokio::select! {
            signal = sig_rx.recv() => match signal {
                Some(Signal::IceServers(v)) => {
                    // room_info 的 iceServers 只是备份：真正的每会议配置在 be-controlled 的 header 里。
                    println!("   room_info ack ✓：iceServers {} 台", ice_servers_from_json(&v).len());
                }
                Some(Signal::BeControlled { header, capture_type }) => {
                    println!("   收到拉流请求（capture_type={capture_type}）");
                    // 每次拉流新建一条会话：ICE 配置属于这一次会议，不能复用上一条。
                    if let Some(old) = session.take() {
                        stop_session(old).await;
                    }
                    let config = match ice_configuration(&header) {
                        Ok(config) => config,
                        Err(e) => {
                            eprintln!("   这次会议连不上：{e}");
                            continue;
                        }
                    };
                    match Peer::new(config, peer_tx.clone()).await {
                        Ok(peer) => session = Some(Session { peer: Arc::new(peer), keyframe: Arc::new(AtomicBool::new(false)), input: None, capture: None, route: json!({}), control_msgs: 0, answered: false }),
                        Err(e) => eprintln!("   建会话失败：{e}"),
                    }
                }
                Some(Signal::Offer { sdp, ice_id, client_id, app_control_id }) => {
                    let Some(session) = session.as_mut() else {
                        println!("   soac offer 在没有会话时到达，忽略");
                        continue;
                    };
                    session.route = json!({ "ice_id": ice_id, "client_id": client_id, "app_control_id": app_control_id });
                    match session.peer.answer_offer(&sdp).await {
                        Ok(answer) => {
                            // fork 把 SoacMessage 的字段摊平在顶层发；顺手带一份 data 副本，两边都认。
                            let body = json!({ "type": "answer", "sdp": answer, "ice_id": ice_id, "app_control_id": app_control_id });
                            let payload = json!({
                                "client_id": client_id,
                                "data": body,
                                "type": "answer",
                                "sdp": answer,
                                "ice_id": ice_id,
                                "app_control_id": app_control_id,
                            });
                            emit_tx.send(("soac".to_string(), Some(payload))).ok();
                            session.answered = true;
                            println!("   answer 已发出（SDP {} 字节）", answer.len());
                            // 补上早到的 candidate（客户端可能靠它们打洞）。
                            for (c, mid, mline) in std::mem::take(&mut early_candidates) {
                                if let Err(e) = session.peer.add_candidate(&c, mid.as_deref(), mline).await {
                                    eprintln!("   补 candidate 失败：{e}");
                                }
                            }
                            if !early_candidates.is_empty() { println!("   早到 candidate 已补发"); }
                        }
                        Err(e) => eprintln!("   answer 失败：{e}"),
                    }
                }
                Some(Signal::Candidate { candidate, sdp_mid, sdp_mline_index }) => {
                    // 客户端的 trickle candidate 常常比 offer 先到；这时 add 会报
                    // "remote description is not set"，白丢一批，连接就慢一步。先攒着。
                    if session.as_ref().is_some_and(|s| s.answered) {
                        if let Some(session) = session.as_mut() {
                            if let Err(e) = session.peer.add_candidate(&candidate, sdp_mid.as_deref(), sdp_mline_index).await {
                                eprintln!("   candidate 失败：{e}");
                            }
                        }
                    } else {
                        early_candidates.push((candidate, sdp_mid, sdp_mline_index));
                        println!("   candidate 先于 offer 到达，暂存 {} 条", early_candidates.len());
                    }
                }
                Some(Signal::PeerLeft) => {
                    println!("   对端离开，关会话");
                    if let Some(session) = session.take() {
                        stop_session(session).await;
                    }
                }
                Some(Signal::Closed(reason)) => {
                    if let Some(session) = session.take() {
                        stop_session(session).await;
                    }
                    return Err(reason);
                }
                None => break,
            },
            event = peer_rx.recv() => match event {
                Some(PeerEvent::ControlOpen { label }) => {
                    println!("   数据通道 open：{label}");
                    if label == CONTROL_LABEL {
                        if let Some(session) = session.as_mut() {
                            let keyframe = session.keyframe.clone();
                            start_video(&display, fps, still_fps, bitrate_kbps, gop, &keyframe, &settings, session, emit_tx.clone());
                        }
                    }
                }
                Some(PeerEvent::ControlData { label, data }) => {
                    if label != CONTROL_LABEL {
                        // Windows 客户端的键盘可能走 TEXT_DATA_CHANNEL 而不是 CONTROL：
                        // 打出前 48 字节，看它到底在哪个通道、什么格式。
                        let head: String = data.iter().take(48).map(|b| format!("{b:02x}")).collect();
                        println!("   非 CONTROL 通道（{label}）：{} 字节 head={head}", data.len());
                    } else if let Some(session) = session.as_mut() {
                        handle_control(&data, session, &settings).await;
                    }
                }
                Some(PeerEvent::KeyframeRequest) => {
                    if let Some(session) = session.as_ref() {
                        session.keyframe.store(true, Ordering::Relaxed);
                    }
                }
                Some(PeerEvent::ConnectionState(state)) => println!("   连接状态：{state}"),
                Some(PeerEvent::ReceiverReport { fraction_lost, total_lost }) => {
                    if fraction_lost > 0 {
                        println!("   视频丢包：fraction_lost={fraction_lost} total_lost={total_lost}");
                    }
                }
                Some(PeerEvent::IceCandidate { candidate, sdp_mid, sdp_mline_index }) => {
                    // trickle：照 fork 的形状摊平在顶层，再带一份 data 副本。
                    let route = session.as_ref().map(|s| s.route.clone()).unwrap_or_else(|| json!({}));
                    let body = json!({
                        "type": "candidate",
                        "candidate": { "candidate": candidate, "sdpMid": sdp_mid, "sdpMLineIndex": sdp_mline_index },
                        "ice_id": route.get("ice_id").cloned().unwrap_or(Value::Null),
                        "client_id": route.get("client_id").cloned().unwrap_or(Value::Null),
                        "app_control_id": route.get("app_control_id").cloned().unwrap_or(Value::Null),
                    });
                    let mut payload = body.clone();
                    if let Some(map) = payload.as_object_mut() {
                        map.insert("data".to_owned(), body.clone());
                    }
                    emit_tx.send(("soac".to_string(), Some(payload))).ok();
                    println!("   candidate 已 trickle");
                }
                Some(PeerEvent::ControlClose { label }) => println!("   数据通道 close：{label}"),
                None => break,
            },
        }
    }
    if let Some(session) = session.take() {
        stop_session(session).await;
    }
    Ok(())
}

/// 关会话：先停采集线程（它在 blocking_send 上等），再关 PeerConnection，最后松开按住的键。
async fn stop_session(session: Session) {
    if let Some(handle) = &session.capture {
        // 采集线程会在 frames_tx 关闭后退出，这里先不等它，避免卡死在 blocking_send。
        let _ = handle;
    }
    session.peer.close().await;
    if let Some(input) = &session.input {
        let engine = input.clone();
        let _ = tokio::task::spawn_blocking(move || engine.lock().unwrap().release_all()).await;
    }
}

fn forward_soac(sig_tx: &mpsc::UnboundedSender<Signal>, payload: Value) {
    let inner = payload.get("data").cloned().unwrap_or(payload.clone());
    let kind = inner.get("type").and_then(Value::as_str).unwrap_or("");
    let ice_id = inner.get("ice_id").cloned();
    let client_id = payload.get("client_id").and_then(Value::as_str).map(str::to_owned);
    let app_control_id = inner.get("app_control_id").cloned();
    match kind {
        "offer" => match decode_offer_sdp(inner.get("sdp").and_then(Value::as_str), inner.get("gzip_sdp").and_then(Value::as_str)) {
            Some(sdp) => {
                let _ = sig_tx.send(Signal::Offer { sdp, ice_id, client_id, app_control_id });
            }
            None => eprintln!("   soac offer 里没有 SDP"),
        },
        "candidate" => {
            let candidate = match inner.get("candidate") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.get("candidate").and_then(Value::as_str).unwrap_or("").to_string(),
                None => String::new(),
            };
            let sdp_mid = inner.get("sdpMid").and_then(Value::as_str).map(str::to_owned);
            let mline = inner.get("sdpMLineIndex").and_then(Value::as_u64).map(|i| i as u16);
            let _ = sig_tx.send(Signal::Candidate { candidate, sdp_mid, sdp_mline_index: mline });
        }
        other => println!("   soac {other}（忽略）"),
    }
}

/// 处理一条 CONTROL 报文：输入注入 / 保活 / protobuf 回包 / 设置 RPC。
async fn handle_control(
    data: &[u8],
    session: &mut Session,
    settings: &Arc<RemoteSettings>,
) {
    let now_secs = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    session.control_msgs += 1;
    if session.control_msgs <= 3 {
        println!(
            "   CONTROL #{} 形状 {}", session.control_msgs,
            linux_uuremote::wire::wire_shape(data, 3)
        );
    }
    let message = match decode_control(data) {
        Ok(m) => m,
        Err(e) => {
            // 解不开的报文不能哑巴吞掉——上次"鼠标没反应"就是没有任何线索可查。
            println!("   CONTROL 报文解不开：{e}（形状 {}）", linux_uuremote::wire::wire_shape(data, 2));
            return;
        }
    };
    match message {
        ControlMessage::KeyboardHeartbeat => {
            if session.control_msgs <= 3 {
                println!("   kbd 心跳");
            }
        }
        ControlMessage::Unsupported(v) => println!("   CONTROL 未识别 JSON：{}", linux_uuremote::wire::json_shape(&v, 2)),
        ControlMessage::Action(action) => {
            println!("   SimpleAction {} seq={} args={}", action_name(action.action).unwrap_or("?"), action.seq, action.args);
            // echo_request：官方客户端靠它确认宿主活着，args 里的 seq 必须原样回。
            // FeatureFlag（encode_echo_response 里带 ff_capture_setting）决定客户端发不发设置 RPC。
            if action.action == 0 {
                let reply = encode_echo_response(action.seq, next_sequence(), now_secs());
                if let Err(e) = session.peer.send_control(&reply).await {
                    eprintln!("   echo 回包失败：{e}");
                }
            }
        }
        ControlMessage::Rpc(request) => {
            apply_rpc(request, &session.peer, settings).await;
        }
        ControlMessage::Input(ref input) => {
            // 鼠标移动是洪水（每秒几十条），每 64 条打一条；键盘/按钮/滚轮必须全打——
            // 「键盘不能用」这种问题只有逐条日志才看得到事件到没到。
            if !matches!(input, linux_uuremote::control::ControlInput::MouseAbsolute { .. })
                || session.control_msgs % 64 == 1
            {
                let detail = match input {
                    linux_uuremote::control::ControlInput::MouseAbsolute { x, y } =>
                        format!(" x={x:.3} y={y:.3}"),
                    _ => String::new(),
                };
                println!("   输入事件：{}{detail}", linux_uuremote::control::input_summary_of(&message));
            }
            let Some(input) = session.input.clone() else {
                println!("   收到输入但没有注入引擎");
                return;
            };
            // X 调用是阻塞的，别摁在事件循环里。
            let message = message.clone();
            let summary = linux_uuremote::control::input_summary_of(&message);
            let result = tokio::task::spawn_blocking(move || {
                let mut engine = input.lock().unwrap();
                engine.apply(&message)
            })
            .await;
            match result {
                // Ok(false) = 这条事件一个 X 事件都没产生（最常见：VK 码映射不到 keycode）。
                // 键盘「按了没反应」就是这个，必须打出来。
                Ok(Ok(false)) => println!("   注入空结果：{summary}（映射不到键位？）"),
                Ok(Err(e)) => eprintln!("   注入失败：{e}"),
                Err(e) => eprintln!("   注入任务崩了：{e}"),
                Ok(Ok(true)) => {}
            }
        }
    }
}

/// 应用一条设置 RPC：回 Message 22（request_id 原样），再报 Message 25 真实生效值。
/// 只支持帧率与码率；分辨率降档需要缩放管线，显式拒绝而不是默默忽略。
async fn apply_rpc(
    request: RpcRequest,
    peer: &linux_uuremote::transport::Peer,
    settings: &Arc<RemoteSettings>,
) {
    let p = request.patch;
    if request.case == 0 {
        println!("   未识别的 RpcRequest case={}（只 ack 不执行）", request.case);
    } else {
        println!("   设置 RPC case={} fps={:?} quality={:?} bps={:?} {}x{:?}", request.case, p.fps, p.quality, p.custom_bps, "当前", p.width.zip(p.height));
    }
    let error = if p.width.is_some() && !p.follow_host {
        Some("Linux 被控端只输出原生分辨率".to_owned())
    } else if p.chroma == Some(3) {
        Some("只支持 4:2:0".to_owned())
    } else {
        None
    };
    if let Some(fps) = p.fps {
        settings.moving_fps.store(fps.max(1) as u64, Ordering::Relaxed);
    }
    if let Some(quality) = p.quality {
        settings.quality.store(quality, Ordering::Relaxed);
        let kbps = preset_bitrate_kbps(quality, p.custom_bps.unwrap_or(0));
        settings.custom_bps.store(p.custom_bps.unwrap_or(0), Ordering::Relaxed);
        settings.bitrate_kbps.store(kbps as u64, Ordering::Relaxed);
    }
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let reply = encode_rpc_response(request.request_id, next_sequence(), ts, error.as_deref());
    let (native_w, native_h) = *settings.native.lock().unwrap();
    let fps_now = settings.moving_fps.load(Ordering::Relaxed) as u32;
    let sync = encode_capture_setting_sync(
        next_sequence(),
        ts,
        fps_now,
        settings.quality.load(Ordering::Relaxed),
        settings.custom_bps.load(Ordering::Relaxed),
        native_w,
        native_h,
    );
    if let Err(e) = peer.send_control(&reply).await {
        eprintln!("   RPC 回包失败：{e}");
    }
    if let Err(e) = peer.send_control(&sync).await {
        eprintln!("   Sync 上报失败：{e}");
    }
}

fn next_sequence() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// CONTROL 通道打开后启动采集/编码/送帧，一个会话只启动一次。
fn start_video(
    display: &Option<String>,
    fps: u32,
    still_fps: u32,
    bitrate_kbps: u32,
    gop: u32,
    keyframe: &Arc<AtomicBool>,
    settings: &Arc<RemoteSettings>,
    session: &mut Session,
    emit_tx: mpsc::UnboundedSender<(String, Option<Value>)>,
) {
    if session.capture.is_some() {
        return;
    }
    if session.input.is_none() {
        match x11rb::connect(display.as_deref()) {
            Ok((conn, screen_index)) => match X11Input::connect(conn, screen_index) {
                Ok(engine) => session.input = Some(Arc::new(std::sync::Mutex::new(engine))),
                Err(e) => eprintln!("   输入注入起不来：{e}（画面照推）"),
            },
            Err(e) => eprintln!("   连不上 X11：{e}（没有注入能力）"),
        }
    }
    let (frames_tx, mut frames_rx) = mpsc::channel::<Encoded>(4);
    // 采集线程一起来就把实测几何报回来：屏幕清单必须用真实尺寸，不能猜。
    let (geom_tx, geom_rx) = oneshot::channel::<(u32, u32)>();
    // 客户端收到屏幕清单之前不要往轨道写帧，否则第一帧 IDR 会被丢弃、后面全是坏 P 帧。
    let (init_done_tx, init_done_rx) = oneshot::channel::<()>();
    let keyframe = keyframe.clone();
    let display = display.clone();
    let thread_settings = settings.clone();
    session.capture = Some(
        std::thread::Builder::new()
            .name("uu-capture".into())
            .spawn(move || capture_loop(display.as_deref(), fps, still_fps, bitrate_kbps, gop, keyframe, geom_tx, thread_settings, frames_tx))
            .expect("起采集线程"),
    );

    let peer_init = session.peer.clone();
    // 能力上报要带这次会议的路由字段；采集线程跑起来时主循环可能已经换了会话，先抓一份。
    let route = session.route.clone();
    let field = |k: &str| route.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let (client_id, ice_id, app_control_id) = (field("client_id"), field("ice_id"), field("app_control_id"));
    tokio::spawn(async move {
        let (width, height) = match geom_rx.await {
            Ok(size) => size,
            Err(_) => {
                eprintln!("   采集起不来，没有屏幕可上报");
                let _ = init_done_tx.send(());
                return;
            }
        };
        let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        // 顺序照参照实现：先声明采集目标，再给屏幕清单，最后报能力与锁屏状态。
        let messages: Vec<Vec<u8>> = vec![
            encode_capture_change(next_sequence(), now(), 0, 0, "Linux desktop").unwrap_or_default(),
            encode_screen_sources(
                next_sequence(),
                now(),
                &[Screen {
                    id: 0,
                    fps: fps.max(1) as i32,
                    resolution: ScreenGeometry { left: 0, top: 0, width: width as i32, height: height as i32, pixel_width: width as i32, pixel_height: height as i32 },
                    primary: true,
                    dpr: 1.0,
                    display_name: "",
                    video_track_index: 0,
                    screen_type: 0,
                    builtin_screen_type: 0,
                }],
                0,
            )
            .unwrap_or_default(),
            encode_permissions(next_sequence(), now(), Permissions { assistant: false, video_capture_allowed: true, capture_content_visible: true, audio_capture_allowed: false, floating_ball: false }),
            encode_secure_desktop(next_sequence(), now(), false, false, false),
        ];
        for message in messages.iter().filter(|m| !m.is_empty()) {
            if let Err(e) = peer_init.send_control(message).await {
                eprintln!("   初始化报文发送失败：{e}");
                break;
            }
        }
        println!("   CONTROL 初始化已发送（{width}x{height}，报文 {} 条）", messages.iter().filter(|m| !m.is_empty()).count());
        let _ = init_done_tx.send(());
        // 官方被控端在 PeerConnection 进入 Connected 后发三条：device_capability、
        // sender_para_info、ice_finished_log。缺了客户端画质菜单会锁在默认值。
        if client_id.is_empty() || ice_id.is_empty() {
            println!("   没有 client_id/ice_id，跳过能力上报");
            return;
        }
        let waited = Instant::now();
        while !peer_init.is_connected() && waited.elapsed() < Duration::from_secs(15) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if !peer_init.is_connected() {
            println!("   15 s 内没连通，跳过能力上报");
            return;
        }
        emit_tx
            .send((
                capability::FORWARD_SETTING_EVENT.to_owned(),
                Some(capability::device_capability(&client_id, &ice_id, 0, fps.max(1) as i32, width, height)),
            ))
            .ok();
        emit_tx
            .send((
                capability::FORWARD_SETTING_EVENT.to_owned(),
                Some(capability::sender_para_info(&client_id, &ice_id, "X11", "OpenH264")),
            ))
            .ok();
        println!("   能力已上报：H.264 4:2:0 {width}x{height} @{}fps（X11 采集 / OpenH264 软编）", fps);
        if !app_control_id.is_empty() {
            emit_tx
                .send((capability::ICE_FINISHED_EVENT.to_owned(), Some(capability::ice_finished(&ice_id, &app_control_id))))
                .ok();
        }
    });

    let peer = session.peer.clone();
    tokio::spawn(async move {
        let _ = init_done_rx.await;
        while let Some(frame) = frames_rx.recv().await {
            if let Err(e) = peer.write_video(frame.data, frame.duration).await {
                eprintln!("   写视频样本失败：{e}");
            }
        }
    });
}

/// 采集 → 门控 → 编码：整机读回，内容比对决定发不发，发的都是整帧。
fn capture_loop(
    display: Option<&str>,
    fps: u32,
    still_fps: u32,
    bitrate_kbps: u32,
    gop: u32,
    keyframe: Arc<AtomicBool>,
    geom_tx: oneshot::Sender<(u32, u32)>,
    settings: Arc<RemoteSettings>,
    frames_tx: mpsc::Sender<Encoded>,
) {
    let mut screen = match X11Screen::open(display, DamageLevel::BoundingBox) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("   采集起不来：{}", e.0);
            return;
        }
    };
    // 编码器要偶数对齐（4:2:0），奇数屏裁掉最后一列/行。
    let width = (screen.width() as usize) & !1;
    let height = (screen.height() as usize) & !1;
    // 上报的是真实屏幕尺寸（编码器只是内部裁 1 像素，坐标空间不变）。
    let measured = (u32::from(screen.width()), u32::from(screen.height()));
    let _ = geom_tx.send(measured);
    *settings.native.lock().unwrap() = measured;
    let mut encoder = match OpenH264::new(width, height, fps, bitrate_kbps, gop, 4) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("   编码器起不来：{e}");
            return;
        }
    };
    let mut fps = fps.max(1);
    let mut gate = FrameDropper::new(GateConfig { moving_fps: fps, still_fps, still_after: Duration::from_millis(1000) });
    let mut planner = FramePlanner::new(fps, still_fps);
    let mut applied_bitrate = bitrate_kbps;
    let mut last_cursor: Option<(i32, i32)> = None;
    let mut frames_sent: u64 = 0;
    let mut last_stat = Instant::now();
    // 分阶段计时：帧率低时要知道慢在抓取、比对、编码还是发送背压。
    let mut acc_grab = Duration::ZERO;
    let mut acc_diff = Duration::ZERO;
    let mut acc_encode = Duration::ZERO;
    let mut acc_send = Duration::ZERO;
    let mut last_reported = measured;
    let mut content = ContentChange::new(width, height, 32, 18);
    let mut emitted = 0u64;
    let mut idr_frames = 0u64;
    let started = Instant::now();

    loop {
        std::thread::sleep(Duration::from_nanos(1_000_000_000 / ((fps.max(1) as u64) * 2)));
        let now = Instant::now();
        // 分辨率可能被动改变（XRandR/切换显示）：每轮免费量一次，变了就重建编码器与比对器。
        let geometry = (u32::from(screen.width()), u32::from(screen.height()));
        if geometry != last_reported {
            last_reported = geometry;
            *settings.native.lock().unwrap() = geometry;
            println!("   屏幕尺寸变化 → {}x{}（客户端要重连才有新清单）", geometry.0, geometry.1);
        }
        // 远程改帧率立刻生效（档位 + 睡眠节奏）。
        let want_fps = (settings.moving_fps.load(Ordering::Relaxed) as u32).max(1);
        if want_fps != fps {
            fps = want_fps;
            let still = settings.still_fps.load(Ordering::Relaxed) as u32;
            gate.set_config(GateConfig { moving_fps: fps, still_fps: still, still_after: Duration::from_millis(1000) });
            planner = FramePlanner::new(fps, still);
            println!("   远程改帧率 → {fps} fps");
        }
        // 远程改码率 → 重建编码器（openh264 的码率在建码器时定），重建后下一帧必须 IDR。
        let want_bitrate = settings.bitrate_kbps.load(Ordering::Relaxed) as u32;
        if want_bitrate != applied_bitrate {
            match OpenH264::new(width, height, fps, want_bitrate, gop, 4) {
                Ok(e) => {
                    encoder = e;
                    applied_bitrate = want_bitrate;
                    planner.note_keyframe_request();
                    println!("   远程改码率 → {want_bitrate} kbps（编码器已重建）");
                }
                Err(e) => eprintln!("   码率重建失败：{e}"),
            }
        }
        // 客户端自己画本地光标（Windows 光标），视频里没有我们的指针；指针移动仍是
        // "有人在用"的唯一证据，用它维持 moving 档，否则静态桌面永远停在 still 档 3fps。
        if let Some(pos) = screen.cursor_position() {
            if last_cursor != Some(pos) {
                last_cursor = Some(pos);
                gate.note_activity(now);
            }
        } else if last_cursor.is_some() {
            last_cursor = None;
            println!("   QueryPointer 拿不到指针位置（Xwayland 不报？）");
        }
        if now.duration_since(last_stat) >= Duration::from_secs(5) {
            let secs = now.duration_since(last_stat).as_secs_f64().max(0.1);
            println!("   出帧速率 {:.1} fps（近 5s 共 {frames_sent} 帧，门控 moving={})", frames_sent as f64 / secs, gate.is_moving());
            let ms = |d: Duration| d.as_millis() as f64 / frames_sent.max(1) as f64;
            println!(
                "   每帧耗时：抓取 {:.1} + 比对 {:.1} + 编码 {:.1} + 发送 {:.1} ms",
                ms(acc_grab), ms(acc_diff), ms(acc_encode), ms(acc_send)
            );
            frames_sent = 0;
            acc_grab = Duration::ZERO;
            acc_diff = Duration::ZERO;
            acc_encode = Duration::ZERO;
            acc_send = Duration::ZERO;
            last_stat = now;
        }
        match gate.decide(now) {
            Action::Idle => continue,
            Action::Keepalive | Action::Capture => {}
        }
        if keyframe.swap(false, Ordering::Relaxed) {
            planner.note_keyframe_request();
        }
        let plan = planner.plan(gate.is_moving());
        if !plan.emit {
            continue;
        }
        let g0 = Instant::now();
        let (frame, _) = match screen.grab_rect(None) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("   抓取失败：{}", e.0);
                planner.note_failure();
                continue;
            }
        };
        acc_grab += g0.elapsed();
        let d0 = Instant::now();
        let diff = content.check(&frame, width, screen.bytes_per_pixel());
        if diff.first || diff.changed_pixels > 0 {
            gate.note_change(now);
        }
        acc_diff += d0.elapsed();
        // 客户端只画它自己的本地光标，画面里必须带上远端指针，否则用户看不到 Linux 光标。
        // 参考帧与内容比对保持无光标（光标只进发出的这份拷贝）。
        let mut out = frame.clone();
        if let (Ok(sprite), Some(pos)) = (screen.fetch_cursor(), screen.cursor_position()) {
            screen.composite_cursor(&mut out, &sprite, pos);
        }
        let e0 = Instant::now();
        match encoder.encode_bgra(&out, plan.force_idr) {
            Ok((au, kind)) => {
                acc_encode += e0.elapsed();
                planner.note_success();
                frames_sent += 1;
                if kind == FrameKind::Idr {
                    idr_frames += 1;
                }
                emitted += 1;
                if emitted % 300 == 0 {
                    println!("   已发 {emitted} 帧（IDR {idr_frames}），{:.1} fps", emitted as f64 / started.elapsed().as_secs_f64().max(0.001));
                }
                let s0 = Instant::now();
                if frames_tx.blocking_send(Encoded { data: au, duration: plan.sample_duration }).is_err() {
                    return;
                }
                acc_send += s0.elapsed();
            }
            Err(e) => {
                eprintln!("   编码失败：{e}");
                planner.note_failure();
            }
        }
    }
}
