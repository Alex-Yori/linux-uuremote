//! uuremote-host —— UU 远程 Linux 被控端宿主。
//!
//! 一条链路串起来：信令（socket.io）→ 等客户端拉流（`be-controlled`）→ WebRTC offer/answer
//! （`soac`）→ 采整机画面 → 内容门控 → openh264 → 视频轨；反方向 CONTROL 通道 JSON → XTest 注入。
//!
//! 用法：`uuremote-host [--display :1] [--fps 60] [--still-fps 3] [--bitrate 0] [--gop 60]`
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
    action_name, decode_control, encode_capture_setting_sync, encode_echo_response, encode_qos_stats, encode_rpc_response,
    preset_bitrate_kbps, ControlMessage, RpcRequest,
};
use linux_uuremote::capture_setting;
use linux_uuremote::desktop_mode::{self, Request};
use linux_uuremote::encoder::{FrameKind, FramePlanner, OpenH264, RtpTimeline};
use linux_uuremote::gating::{Action, FrameDropper, GateConfig};
use linux_uuremote::nvenc::NvencH264;
use linux_uuremote::initialization::{encode_capture_change, encode_permissions, encode_secure_desktop, encode_screen_sources, envelope, Permissions, Screen, ScreenGeometry};
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
        /// 官方 ConnectOptions：客户端带来的初始采集设置与解码能力（可能解不出来，None = 按默认）。
        options: Option<linux_uuremote::capture_setting::ConnectOptions>,
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
    /// 桌面流计数守卫：最后一路结束时把 xrandr 改过的分辨率恢复回原始模式。
    mode_guard: Option<desktop_mode::StreamGuard>,
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
    /// 连接阶段计时（"连接半分钟"的账要能算）：be-controlled 到达的时刻。
    t_be_controlled: std::time::Instant,
    /// answer 发出的时刻（None = 还没收到 offer）。
    t_answer: Option<std::time::Instant>,
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
    /// 真彩：1 = 4:4:4（NVENC yuv444p 管线），0 = 4:2:0。软编永远 4:2:0。
    chroma: AtomicU64,
    /// 控制端解码帧率上限（ConnectOptions.DecoderCap），0 = 未知。
    decoder_fps: AtomicU64,
    /// 控制端能解 4:4:4（官方宿主把 decoder_cap_list 与自身编码能力求交）。
    decoder_full_chroma: AtomicBool,
    /// 接收端报告的丢包（fraction_lost 0..255）：0 = 链路干净（LAN 直连），配速直接旁路。
    loss: AtomicU64,
    /// PeerConnection 是否已连通：自适应降帧的门槛——没连通时统计的是"没人看"，不是编码器吃不下。
    connected: AtomicBool,
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
            chroma: AtomicU64::new(0),
            decoder_fps: AtomicU64::new(0),
            decoder_full_chroma: AtomicBool::new(false),
            loss: AtomicU64::new(0),
            connected: AtomicBool::new(false),
        }
    }
}

/// 信令断线重连的退避秒数：连续失败指数翻倍、封顶 30 秒；
/// 上一轮活过 10 秒说明服务器可达（只是会话/信令断了），回到基础 2 秒。
fn next_reconnect_delay(fail_streak: u32, last_run_secs: u64) -> u64 {
    if last_run_secs >= 10 { return 2; }
    (2u64 << fail_streak.min(30)).min(30)
}

/// 登录态失效不值得原地重试（要人扫码），其余错误（信令关闭、会话异常）就地重建房间。
fn fatal_after_run(err: &str) -> bool {
    err.contains("token 已失效") || err.contains("尚未登录") || err.contains("1120")
}

fn main() {
    let mut display: Option<String> = None;
    let mut fps: u32 = 60;
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
                println!("用法: uuremote-host [--display :1] [--fps 60] [--still-fps 3] [--bitrate kbps] [--gop 帧数]");
                return;
            }
            other => eprintln!("未知参数 {other}"),
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    // 信令断线是常态（云端闲时关连）：就地退避重建房间，只有登录态失效才退出交给 systemd/人。
    let mut fail_streak = 0u32;
    loop {
        let t0 = std::time::Instant::now();
        match rt.block_on(run(display.clone(), fps, still_fps, bitrate_kbps, gop)) {
            Ok(()) => println!("信令通道结束，重建房间…"),
            Err(e) => {
                println!("宿主退出：{e}");
                if fatal_after_run(&e) { std::process::exit(1); }
            }
        }
        let delay = next_reconnect_delay(fail_streak, t0.elapsed().as_secs());
        fail_streak = fail_streak.saturating_add(1).min(5);
        println!("   {delay} 秒后重连（连续失败 {fail_streak} 次）…");
        std::thread::sleep(std::time::Duration::from_secs(delay));
    }
}

async fn run(display: Option<String>, fps: u32, still_fps: u32, bitrate_kbps: u32, gop: u32) -> Result<(), String> {
    // 回收上次崩溃残留的虚拟音频路由；进程退出时恢复默认输出。
    linux_uuremote::virtual_audio::init();
    let _audio_cleanup = linux_uuremote::virtual_audio::Cleanup;
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
        // 官方 ControlledSession RefreshSignalTokenLoop：每 30 s 刷一次重连键（无 payload）。
        // 不刷云端会把会话判旧——客户端"先发了 candidate 却不发 offer、12 s 超时 released"
        // 的三连试就在这个窗口里。
        let mut req_counter: u64 = 10;
        let mut next_refresh = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next_refresh) => {
                    req_counter += 1;
                    match sig.send_with_ack(req_counter, signaling::EVENT_REFRESH_RECONNECT_KEY, None).await {
                        Ok(()) => println!("   信令重连键已刷新（id={req_counter}）"),
                        Err(e) => eprintln!("   信令重连键刷新失败：{e}"),
                    }
                    next_refresh = tokio::time::Instant::now() + Duration::from_secs(30);
                }
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
                            signaling::EVENT_BE_CONTROLLED => { let _ = sig_tx.send(Signal::BeControlled { header: v, capture_type: 0, options: None }); }
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
                        let options = attachments
                            .iter()
                            .find_map(|bytes| linux_uuremote::capture_setting::connect_options(bytes));
                        // ConnectOptions 解不出来时至少把采集目标抠出来，别退成 0。
                        let capture_type = options.as_ref().map(|o| o.capture_type).unwrap_or_else(|| {
                            attachments
                                .iter()
                                .find_map(|bytes| linux_uuremote::wire::decode_wire(bytes).ok())
                                .and_then(|fields| linux_uuremote::wire::varint_field(&fields, 1))
                                .unwrap_or(0)
                        });
                        let header = header.unwrap_or(Value::Null);
                        println!(
                            "   {name}(binary)：附件 {} 段 capture_type={capture_type}{}{}",
                            attachments.len(),
                            options.as_ref().map(|o| format!(" 客户端版本={}", o.client_version)).unwrap_or_default(),
                            options.as_ref().map(|o| format!(" 解码能力 {} 条", o.decoder_caps.len())).unwrap_or_default(),
                        );
                        if options.is_none() && name == signaling::EVENT_BE_CONTROLLED {
                            // 解不出 ConnectOptions 就把结构打出来，下次连接能照着官方 schema 修。
                            for bytes in attachments.iter().take(2) {
                                println!("   ConnectOptions 结构：{}", linux_uuremote::wire::wire_shape(bytes, 3));
                            }
                        }
                        if name == signaling::EVENT_BE_CONTROLLED {
                            let _ = sig_tx.send(Signal::BeControlled { header, capture_type, options });
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
    // offer 看门狗：be-controlled 到后只等到 candidate 而 offer 迟迟不来 = 云端
    // 路由卡死（2026-10-30 20:50 实测：13 条 candidate 全到、offer 一条没有，
    // 客户端 12s 超时重连才通）。到点主动重连信令，把房间重新注册一遍。
    let mut offer_deadline: Option<tokio::time::Instant> = None;

    println!("等待客户端拉流（Ctrl-C 退出）…");
    // 长按重复的宿主自驱动节拍：50ms 粒度排空到期键（周期 33ms），不等 CONTROL
    // 消息到达——长按时鼠标是静止的，靠消息驱动会断流。
    let mut repeat_tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            _ = repeat_tick.tick() => {
                if let Some(input) = session.as_ref().and_then(|s| s.input.clone()) {
                    tokio::task::spawn_blocking(move || {
                        let mut engine = input.lock().unwrap();
                        let _ = engine.tick_repeats();
                    });
                }
            }
            _ = tokio::time::sleep_until(offer_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if offer_deadline.is_some() && session.as_ref().is_some_and(|s| !s.answered) =>
            {
                println!("   ⚠ be-controlled 后 6 秒没等到 offer（只有 candidate）→ 疑似云端路由卡死，重连信令重新注册房间");
                if let Some(old) = session.take() {
                    stop_session(old).await;
                }
                return Err("offer-watchdog: 重连信令重新注册房间".to_string());
            }
            signal = sig_rx.recv() => match signal {
                Some(Signal::IceServers(v)) => {
                    // room_info 的 iceServers 只是备份：真正的每会议配置在 be-controlled 的 header 里。
                    println!("   room_info ack ✓：iceServers {} 台", ice_servers_from_json(&v).len());
                }
                Some(Signal::BeControlled { header, capture_type, options }) => {
                    println!("   收到拉流请求（capture_type={capture_type}）");
                    // 官方 ConnectOptions 带来客户端的初始采集设置与解码能力：
                    // 先按它设初值（客户端 UI 上显示的就是这些），再拿解码能力与自身
                    // 编码能力求交——真彩/帧率上限都由这个交集决定，不是宿主单方面声明。
                    if let Some(opts) = options.as_ref() {
                        let dec_fps = linux_uuremote::capture_setting::decoder_max_fps(&opts.decoder_caps, 0);
                        settings.decoder_fps.store(u64::from(dec_fps), Ordering::Relaxed);
                        let full = linux_uuremote::capture_setting::decoder_allows_full_chroma(&opts.decoder_caps);
                        settings.decoder_full_chroma.store(full, Ordering::Relaxed);
                        if let Some(fps) = opts.initial.fps {
                            settings.moving_fps.store((fps.max(5)).min(144) as u64, Ordering::Relaxed);
                        }
                        if let Some(quality) = opts.initial.quality {
                            let custom = opts.initial.custom_bps.unwrap_or(0);
                            settings.quality.store(quality, Ordering::Relaxed);
                            settings.custom_bps.store(custom, Ordering::Relaxed);
                            let kbps = preset_bitrate_kbps(quality, custom);
                            println!(
                                "   画质档原始值：quality={quality} custom_bps={custom} → {kbps} kbps / CQP {}",
                                linux_uuremote::nvenc::preset_cq(quality)
                            );
                            if kbps > 0 {
                                settings.bitrate_kbps.store(kbps as u64, Ordering::Relaxed);
                            }
                        }
                        if opts.initial.chroma == Some(3) && full && linux_uuremote::nvenc::available() {
                            settings.chroma.store(1, Ordering::Relaxed);
                        }
                        println!(
                            "   客户端初始设置：fps={:?} 画质档={:?} 色度={:?} hdr={} 跟随被控端={}；解码上限 fps={} 真彩={}",
                            opts.initial.fps, opts.initial.quality, opts.initial.chroma, opts.initial.hdr,
                            opts.initial.follow_remote, dec_fps, full,
                        );
                    }
                    SOAC_LOGS.store(0, std::sync::atomic::Ordering::Relaxed);
                    offer_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(6));
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
                        Ok(peer) => session = Some(Session { peer: Arc::new(peer), keyframe: Arc::new(AtomicBool::new(false)), input: None, capture: None, mode_guard: None, route: json!({}), control_msgs: 0, answered: false, t_be_controlled: std::time::Instant::now(), t_answer: None }),
                        Err(e) => eprintln!("   建会话失败：{e}"),
                    }
                }
                Some(Signal::Offer { sdp, ice_id, client_id, app_control_id }) => {
                    offer_deadline = None;
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
                            session.t_answer = Some(std::time::Instant::now());
                            println!(
                                "   answer 已发出（SDP {} 字节；be-controlled→answer {} ms）",
                                answer.len(),
                                std::time::Instant::now().duration_since(session.t_be_controlled).as_millis()
                            );
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
                    offer_deadline = None;
                    linux_uuremote::virtual_audio::disable();
                    println!("   对端离开，关会话");
                    if let Some(session) = session.take() {
                        stop_session(session).await;
                    }
                }
                Some(Signal::Closed(reason)) => {
                    linux_uuremote::virtual_audio::disable();
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
                            if let Some(t) = session.t_answer {
                                println!(
                                    "   阶段耗时：answer→control_open {} ms（be-controlled 起累计 {} ms）",
                                    std::time::Instant::now().duration_since(t).as_millis(),
                                    std::time::Instant::now().duration_since(session.t_be_controlled).as_millis()
                                );
                            }
                            let keyframe = session.keyframe.clone();
                            start_video(&display, fps, still_fps, bitrate_kbps, gop, &keyframe, &settings, session, emit_tx.clone());
                        }
                    }
                }
                Some(PeerEvent::ControlData { label, data }) => {
                    // 参照实现把 CONTROL 和 TEXT 两条通道都当控制面（键盘等 JSON 输入
                    // 实际走 TEXT；只监听 CONTROL 会让键盘无声丢光）。STREAMER 是别的东西，只记日志。
                    if matches!(label.as_str(), CONTROL_LABEL | "TEXT_DATA_CHANNEL") {
                        if let Some(session) = session.as_mut() {
                            handle_control(&data, session, &settings, &label, &display).await;
                        }
                    } else if label != "STREAMER_DATA_CHANNEL" || session.as_ref().is_some_and(|s| s.control_msgs % 20 == 0) {
                        let head: String = data.iter().take(48).map(|b| format!("{b:02x}")).collect();
                        println!("   旁路通道（{label}）：{} 字节 head={head}", data.len());
                        if std::env::var("UU_HOST_DEBUG").is_ok() && label == "STREAMER_DATA_CHANNEL" {
                            // 客户端周期统计（connection_period_stats_event）原样回传，
                            // 排查延迟/frm/帧率口径时直接看它报了些什么。
                            println!("   STREAMER 载荷：{}", String::from_utf8_lossy(&data[..data.len().min(4000)]));
                        }
                    }
                }
                Some(PeerEvent::KeyframeRequest) => {
                    if let Some(session) = session.as_ref() {
                        session.keyframe.store(true, Ordering::Relaxed);
                    }
                }
                Some(PeerEvent::ConnectionState(state)) => {
                    println!("   连接状态：{state}");
                    // Connected = DTLS 完成，可以开始推桌面声音（opus 轨在 answer 里谈定过才有）。
                    if state == "connected" {
                        settings.connected.store(true, Ordering::Relaxed);
                        if let Some(t) = session.as_ref().and_then(|s| s.t_answer) {
                            println!("   阶段耗时：answer→DTLS connected {} ms", std::time::Instant::now().duration_since(t).as_millis());
                        }
                        if let Some(session) = session.as_ref() {
                            if let Some(track) = session.peer.audio_track().await {
                                // 本地默认输出静音时开虚拟路由：远端有声、本地不出声。
                                if linux_uuremote::audio::enabled()
                                    && linux_uuremote::virtual_audio::default_sink_muted()
                                    && linux_uuremote::virtual_audio::enable()
                                {
                                    println!("   本地输出静音 → 开虚拟路由（远端有声，本地不出声）");
                                }
                                tokio::spawn(linux_uuremote::audio::stream_audio(session.peer.pc_handle(), track));
                            }
                        }
                    } else if matches!(state.as_str(), "disconnected" | "closed" | "failed") {
                        linux_uuremote::virtual_audio::disable();
                    }
                }
                Some(PeerEvent::ReceiverReport { fraction_lost, total_lost }) => {
                    settings.loss.store(fraction_lost as u64, Ordering::Relaxed);
                    if fraction_lost > 0 {
                        println!("   视频丢包：fraction_lost={fraction_lost} total_lost={total_lost}（配速接管）");
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

/// 每次拉流会话只打前 8 条 soac 的完整形状：连接慢时要知道客户端到底发了什么
/// （实测前三次尝试只有 13 条 candidate、零条 offer，光靠事件名看不出内容）。
static SOAC_LOGS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn forward_soac(sig_tx: &mpsc::UnboundedSender<Signal>, payload: Value) {
    let inner = payload.get("data").cloned().unwrap_or(payload.clone());
    if SOAC_LOGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
        println!("   soac 形状 {}", linux_uuremote::wire::json_shape(&inner, 2));
    }
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
    reply_label: &str,
    display: &Option<String>,
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
                println!("   kbd 心跳（转发为长按重复按下）");
            }
            // 心跳 = 客户端声明"某个键还按着"。转发给注入引擎：repeater 里按住的键
            // 各补一次按下（不等 500ms 延迟——心跳本身就是节奏）。
            if let Some(input) = session.input.clone() {
                let result = tokio::task::spawn_blocking(move || {
                    let mut engine = input.lock().unwrap();
                    engine.apply(&ControlMessage::KeyboardHeartbeat)
                })
                .await;
                if let Err(e) = result {
                    eprintln!("   心跳注入任务崩了：{e}");
                }
            }
        }
        ControlMessage::Unsupported(v) => println!("   CONTROL 未识别 JSON：{}", linux_uuremote::wire::json_shape(&v, 2)),
        ControlMessage::Action(action) => {
            println!("   SimpleAction {} seq={} args={}", action_name(action.action).unwrap_or("?"), action.seq, action.args);
            // echo_request：官方客户端靠它确认宿主活着，args 里的 seq 必须原样回。
            // FeatureFlag（encode_echo_response 里带 ff_capture_setting）决定客户端发不发设置 RPC。
            if action.action == 0 {
                let reply = encode_echo_response(action.seq, next_sequence(), now_secs());
                if let Err(e) = session.peer.send_on(reply_label, &reply).await {
                    eprintln!("   echo 回包失败：{e}");
                }
            }
        }
        ControlMessage::Rpc(request) => {
            if request.case == 0 {
                println!("   RpcRequest 形状 {}", linux_uuremote::wire::wire_shape(data, 3));
            }
            apply_rpc(request, &session.peer, settings, reply_label, display).await;
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

/// 应用一条设置 RPC：回 Message 22（request_id 原样、空 errors = 已应用），再报 Message 25
/// 真实生效值。分辨率照参照实现**直接切显示模式**（xrandr；官方 Windows 宿主同样改模式本身），
/// 切不动就保持现模式继续推。真彩(4:4:4)接受但 Sync 报实际 4:2:0——不再整体拒绝，
/// 显式拒绝正是客户端弹"被控端受限"的来源之一。
async fn apply_rpc(
    request: RpcRequest,
    peer: &linux_uuremote::transport::Peer,
    settings: &Arc<RemoteSettings>,
    reply_label: &str,
    display: &Option<String>,
) {
    let p = request.patch;
    if request.case == 0 {
        println!("   未识别的 RpcRequest case={}（照参照实现只回空 22，不改状态）", request.case);
    } else {
        println!("   设置 RPC case={} fps={:?} quality={:?} bps={:?} {}x{:?} follow_host={} chroma={:?} codec={:?}",
            request.case, p.fps, p.quality, p.custom_bps, "当前", p.width.zip(p.height), p.follow_host, p.chroma, p.codec);
    }
    // 真彩照参照实现：NVENC 管线真切 4:4:4，软编拒绝并降 4:2:0，Sync 一律报实际值。
    if p.chroma == Some(3) {
        if linux_uuremote::nvenc::available() && settings.decoder_full_chroma.load(Ordering::Relaxed) {
            settings.chroma.store(1, Ordering::Relaxed);
            println!("   真彩：NVENC 切 4:4:4（采集循环按新色度重建编码器）");
        } else {
            settings.chroma.store(0, Ordering::Relaxed);
            println!("   请求 4:4:4 真彩；软编管线只产 4:2:0（Sync 报实际值）");
        }
    } else if p.chroma == Some(1) {
        settings.chroma.store(0, Ordering::Relaxed);
    }
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    if request.case == 0 {
        // 会话/插件 RPC（send_video_track_req 等）照参照实现回对应响应字段；
        // 只回空 CaptureSettingResponse 客户端会认为协商失败，画质菜单锁死。
        let reply = match linux_uuremote::control::session_rpc_response(request.request_id, next_sequence(), ts, &request.kinds) {
            Some(reply) => {
                println!("   会话/插件 RPC 已答：请求字段 {:?} → Message 22", request.kinds);
                reply
            }
            None => encode_rpc_response(request.request_id, next_sequence(), ts, None),
        };
        if let Err(e) = peer.send_on(reply_label, &reply).await {
            eprintln!("   RPC 回包失败：{e}");
        }
        return;
    }
    if let Some(fps) = p.fps {
        settings.moving_fps.store((fps.max(5)).min(144) as u64, Ordering::Relaxed);
    }
    if let Some(quality) = p.quality {
        settings.quality.store(quality, Ordering::Relaxed);
        let kbps = preset_bitrate_kbps(quality, p.custom_bps.unwrap_or(0));
        println!(
            "   画质档原始值：quality={quality} custom_bps={:?} → {kbps} kbps / CQP {}",
            p.custom_bps,
            linux_uuremote::nvenc::preset_cq(quality)
        );
        settings.custom_bps.store(p.custom_bps.unwrap_or(0), Ordering::Relaxed);
        settings.bitrate_kbps.store(kbps as u64, Ordering::Relaxed);
    }
    // 分辨率请求：跟随被控端 = 恢复原始模式；显式像素 = 切到那个模式（xrandr）。
    let resolution = match request.case {
        2 => {
            if p.follow_host {
                Some(Request::Native)
            } else {
                p.width.zip(p.height).map(|(w, h)| Request::Size(w, h))
            }
        }
        6 => p.width.zip(p.height).map(|(w, h)| Request::Size(w, h)),
        _ => None,
    };
    let native_before = *settings.native.lock().unwrap();
    if let Some(req) = resolution {
        if desktop_mode::enabled() {
            let d = display.clone();
            match tokio::task::spawn_blocking(move || desktop_mode::apply(d.as_deref(), req)).await {
                Ok(Some(size)) => *settings.native.lock().unwrap() = size,
                Ok(None) => {}
                Err(_) => println!("   显示模式切换任务失败（保持现模式）"),
            }
        } else if resolution.is_some() {
            println!("   UU_DESKTOP_RESIZE=0：分辨率档位只记账，不改显示");
        }
    }
    let reply = encode_rpc_response(request.request_id, next_sequence(), ts, None);
    let (native_w, native_h) = *settings.native.lock().unwrap();
    let fps_now = settings.moving_fps.load(Ordering::Relaxed) as u32;
    let chroma_now = settings.chroma.load(Ordering::Relaxed);
    let sync = encode_capture_setting_sync(
        next_sequence(),
        ts,
        fps_now,
        settings.quality.load(Ordering::Relaxed),
        settings.custom_bps.load(Ordering::Relaxed),
        native_w,
        native_h,
        if chroma_now == 1 { 3 } else { 1 },
    );
    if let Err(e) = peer.send_on(reply_label, &reply).await {
        eprintln!("   RPC 回包失败：{e}");
    }
    if let Err(e) = peer.send_on(reply_label, &sync).await {
        eprintln!("   Sync 上报失败：{e}");
    }
    // 分辨率真的变了：刷新屏幕清单（原生 + 缩放档 + 刷新率），否则客户端菜单还停在旧尺寸。
    if (native_w, native_h) != native_before {
        let scaled: Vec<(i32, i32)> = capture_setting::scaled_sizes((native_w, native_h))
            .into_iter()
            .map(|(w, h)| (w as i32, h as i32))
            .collect();
        let refresh = capture_setting::display_fps(display.as_deref());
        match encode_screen_sources(
            next_sequence(),
            ts,
            &[Screen {
                id: 0,
                fps: refresh as i32,
                resolution: ScreenGeometry { left: 0, top: 0, width: native_w as i32, height: native_h as i32, pixel_width: native_w as i32, pixel_height: native_h as i32 },
                primary: true,
                dpr: 1.0,
                display_name: "Linux desktop",
                video_track_index: 0,
                screen_type: 0,
                builtin_screen_type: 0,
                scaled: &scaled,
                current: None,
            }],
            0,
        ) {
            Ok(msg) => {
                let _ = peer.send_on(reply_label, &msg).await;
            }
            Err(e) => println!("   屏幕清单刷新失败：{e}"),
        }
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
    // 桌面流守卫：会话结束时把 xrandr 改过的分辨率恢复回原始模式（计数归零才恢复）。
    session.mode_guard = Some(desktop_mode::StreamGuard::new());
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
    let (shapes_tx, mut shapes_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // 采集线程一起来就把实测几何报回来：屏幕清单必须用真实尺寸，不能猜。
    let (geom_tx, geom_rx) = oneshot::channel::<(u32, u32)>();
    // 客户端收到屏幕清单之前不要往轨道写帧，否则第一帧 IDR 会被丢弃、后面全是坏 P 帧。
    let (init_done_tx, init_done_rx) = oneshot::channel::<()>();
    let keyframe = keyframe.clone();
    let display = display.clone();
    let display_init = display.clone();
    let thread_settings = settings.clone();
    let init_settings = settings.clone();
    session.capture = Some(
        std::thread::Builder::new()
            .name("uu-capture".into())
            .spawn(move || capture_loop(display.as_deref(), fps, still_fps, bitrate_kbps, gop, keyframe, geom_tx, thread_settings, frames_tx, shapes_tx))
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
        // 照参照实现的初始化组合：8 采集目标 → 18 编解码协商（fps 上限 + 尺寸描述符，
        // 客户端帧率菜单靠它解锁）→ 7 屏幕清单（带缩放档与真实刷新率）→ 权限 → 锁屏。
        // 缺 18 的宿主会被客户端判成"受限"，菜单锁死默认档、不给指定帧率。
        let refresh = capture_setting::display_fps(display_init.as_deref());
        // 编码能力报编码器上限（NVENC 固定 4K），不是屏幕尺寸——客户端画质档位按它解锁。
        let nvenc = linux_uuremote::nvenc::available();
        let codec_size = if nvenc { capability::CODEC_CAPABILITY_SIZE } else { (width, height) };
        // 与客户端解码能力求交：CodecNegotiation 的 fps 上限不能超过对端能解的帧率，
        // 真彩只有两边都支持才声明（官方宿主就是这么做的）。
        let dec_fps = init_settings.decoder_fps.load(Ordering::Relaxed) as u32;
        let cap_fps = if dec_fps > 0 { refresh.min(dec_fps) } else { refresh };
        let full_chroma = nvenc && init_settings.decoder_full_chroma.load(Ordering::Relaxed);
        let scaled: Vec<(i32, i32)> = capture_setting::scaled_sizes((width, height))
            .into_iter()
            .map(|(w, h)| (w as i32, h as i32))
            .collect();
        let applied_bitrate_init = bitrate_kbps;
        // 各档码率上限照官方档位表报（Message 14 ReportQosStats 的 5-8 号字段）。
        let tiers = (
            u64::from(preset_bitrate_kbps(1, 0)) * 1_000,
            u64::from(preset_bitrate_kbps(2, 0)) * 1_000,
            u64::from(preset_bitrate_kbps(3, 0)) * 1_000,
            u64::from(preset_bitrate_kbps(4, 0)) * 1_000,
        );
        let encoder_name = if nvenc { "NVIDIA NVENC" } else { "OpenH264" };
        let messages: Vec<Vec<u8>> = vec![
            encode_capture_change(next_sequence(), now(), 0, 0, "Linux desktop").unwrap_or_default(),
            envelope(next_sequence(), now(), 18, &capture_setting::codec_negotiation(codec_size, cap_fps, false)),
            encode_qos_stats(
                next_sequence(),
                now(),
                encoder_name,
                "X11",
                u64::from(applied_bitrate_init.max(1)) * 1_000,
                init_settings.quality.load(Ordering::Relaxed),
                tiers,
            ),
            encode_screen_sources(
                next_sequence(),
                now(),
                &[Screen {
                    id: 0,
                    fps: refresh as i32,
                    resolution: ScreenGeometry { left: 0, top: 0, width: width as i32, height: height as i32, pixel_width: width as i32, pixel_height: height as i32 },
                    primary: true,
                    dpr: 1.0,
                    display_name: "Linux desktop",
                    video_track_index: 0,
                    screen_type: 0,
                    builtin_screen_type: 0,
                    scaled: &scaled,
                    current: None,
                }],
                0,
            )
            .unwrap_or_default(),
            encode_permissions(next_sequence(), now(), Permissions { assistant: false, video_capture_allowed: true, capture_content_visible: true, audio_capture_allowed: linux_uuremote::audio::enabled(), floating_ball: false }),
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
        // ControlOpen 晚于 DTLS connected（SCTP 在 DTLS 之上），到这里必然已连通——
        // 照官方时机（Connected 即报）直接发，不再等 15 s。真没连通就跳过这条上报。
        if !peer_init.is_connected() {
            println!("   还没连通，跳过能力上报");
            return;
        }
        emit_tx
            .send((
                capability::FORWARD_SETTING_EVENT.to_owned(),
                Some(capability::device_capability(
                    &client_id,
                    &ice_id,
                    0,
                    refresh as i32,
                    if nvenc { capability::CODEC_IMPL_NVENC } else { capability::CODEC_IMPL_SOFTWARE },
                    // HEVC 本机驱动实测 InitializeEncoder failed，不虚报；真彩要两边都支持。
                    false,
                    full_chroma,
                    codec_size.0,
                    codec_size.1,
                )),
            ))
            .ok();
        emit_tx
            .send((
                capability::FORWARD_SETTING_EVENT.to_owned(),
                Some(capability::sender_para_info(&client_id, &ice_id, "X11", if nvenc { "NVIDIA NVENC" } else { "OpenH264" })),
            ))
            .ok();
        println!(
            "   能力已上报：H.264 {} {}x{} @{}fps（X11 采集 / {}，推流档 {fps}fps）",
            if full_chroma { "4:2:0+4:4:4" } else { "4:2:0" },
            codec_size.0, codec_size.1, cap_fps, if nvenc { "NVENC 硬编" } else { "OpenH264 软编" },
        );
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
    // 光标形状镜像：Message{15: SystemStateChange{2: CursorShape}} 走 CONTROL 通道，
    // 客户端拿它替换自己画的本地贴图（不发的话用户永远看到 Windows 箭头）。
    let peer_shape = session.peer.clone();
    tokio::spawn(async move {
        while let Some(shape) = shapes_rx.recv().await {
            if let Err(e) = peer_shape.send_control(&shape).await {
                eprintln!("   光标形状发送失败：{e}");
            } else {
                println!("   光标形状已发（{} 字节）", shape.len());
            }
        }
    });
}

/// 采集 → 编码 → 送帧。NVENC（FFmpeg 子进程，参照实现在本机的方式）优先，
/// openh264 软编兜底。NVENC 模式恒帧率恒码率（CBR），不做内容门控——
/// 客户端右下角 10–40 fps 横跳、几百 Kbps–几 Mbps 横跳正是软编 + 静止门控的产物。
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
    shapes_tx: mpsc::UnboundedSender<Vec<u8>>,
) {
    fn rebuild_backend(
        pipe: &mut Option<NvencH264>,
        encoder: &mut Option<OpenH264>,
        nvenc: &mut bool,
        w: usize,
        h: usize,
        f: u32,
        br: u32,
        gop: u32,
        cq: u32,
        chroma_444: bool,
        dpy: Option<&str>,
    ) {
        if *nvenc {
            // 先杀旧进程再开新的：NVENC 会话槽与显存（每个 ffmpeg 约 346 MB）都是
            // 稀缺资源，新旧并存会把驱动顶满。
            *pipe = None;
            // 默认让 ffmpeg 自己 x11grab（实测比喂帧快一倍）；UU_NVENC_FEED=1 走喂帧管线。
            let capture = if std::env::var("UU_NVENC_FEED").is_ok() { None } else { Some(dpy.unwrap_or(":0")) };
            match NvencH264::open(w as u32, h as u32, f, br, cq, chroma_444, capture) {
                Ok(p) => *pipe = Some(p),
                Err(e) => {
                    println!("   NVENC 起不来：{e}（退回 openh264 软编）");
                    *nvenc = false;
                    *pipe = None;
                    *encoder = OpenH264::new(w, h, f, br.max(1), gop, 4).ok();
                }
            }
        } else {
            *encoder = OpenH264::new(w, h, f, br.max(1), gop, 4).ok();
        }
    }

    /// NVENC 挂了：杀管道、切软编，软编一定要真建出来（否则一帧都发不出）。
    fn fallback_to_software(
        pipe: &mut Option<NvencH264>,
        encoder: &mut Option<OpenH264>,
        nvenc: &mut bool,
        w: usize,
        h: usize,
        f: u32,
        br: u32,
        gop: u32,
    ) {
        *pipe = None;
        *nvenc = false;
        if encoder.is_none() {
            *encoder = OpenH264::new(w, h, f, br.max(1), gop, 4).ok();
        }
    }

    let mut screen = match X11Screen::open(display, DamageLevel::BoundingBox) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("   采集起不来：{}", e.0);
            return;
        }
    };
    // 编码器要偶数对齐（4:2:0），奇数屏裁掉最后一列/行。
    let mut width = (screen.width() as usize) & !1;
    let mut height = (screen.height() as usize) & !1;
    // 上报的是真实屏幕尺寸（编码器只是内部裁 1 像素，坐标空间不变）。
    let measured = (u32::from(screen.width()), u32::from(screen.height()));
    let _ = geom_tx.send(measured);
    *settings.native.lock().unwrap() = measured;
    let mut fps = fps.max(1);
    let mut applied_bitrate = bitrate_kbps;
    let mut backend_nvenc = linux_uuremote::nvenc::available();
    // NVENC 模式默认让 ffmpeg 自己 x11grab 抓屏（实测 2K@90 管线 76 fps）；
    // XShm 抓屏 + stdin 喂帧只有 ~47 fps——每帧两次 14.7 MB 大拷贝才是 90 帧档
    // 跑不满的真瓶颈（自适应曾把它误判成编码器吃不下）。UU_NVENC_FEED=1 切回喂帧。
    let self_capture = std::env::var("UU_NVENC_FEED").is_err();
    let mut pipe: Option<NvencH264> = None;
    let mut encoder: Option<OpenH264> = None;
    rebuild_backend(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop, linux_uuremote::nvenc::preset_cq(settings.quality.load(Ordering::Relaxed)), settings.chroma.load(Ordering::Relaxed) == 1, display);
    if pipe.is_none() && encoder.is_none() {
        eprintln!("   编码器起不来（NVENC 与 openh264 都失败）");
        return;
    }
    println!(
        "   编码后端：{}",
        if backend_nvenc { format!("NVENC（FFmpeg h264_nvenc，CBR {applied_bitrate} kbps）") } else { "openh264 软编".to_owned() }
    );
    let mut gate = FrameDropper::new(GateConfig { moving_fps: fps, still_fps, still_after: Duration::from_millis(1000) });
    let mut planner = FramePlanner::new(fps, still_fps);
    // RTP 时间戳跟真实出帧时刻走（见 RtpTimeline），否则静止/卡顿会把接收端 frm 攒上天。
    let mut timeline = RtpTimeline::new(fps);
    let mut last_cursor: Option<(i32, i32)> = None;
    let mut frames_sent: u64 = 0;
    let mut last_stat = Instant::now();
    let composite_cursor = std::env::var("UU_COMPOSITE_CURSOR").is_ok();
    let mut last_shape_hash: u64 = 0;
    // 分阶段计时：帧率低时要知道慢在抓取、比对、编码还是发送背压。
    let mut acc_grab = Duration::ZERO;
    let mut acc_diff = Duration::ZERO;
    let mut acc_encode = Duration::ZERO;
    let mut acc_send = Duration::ZERO;
    let mut last_reported = measured;
    let mut content = ContentChange::new(width, height, 32, 18);
    let mut emitted = 0u64;
    let mut idr_frames = 0u64;
    let mut last_chroma = settings.chroma.load(Ordering::Relaxed);
    // 发送端配速（官方靠 libwebrtc pacer，webrtc-rs 没有等价物）：算好的 AU 按设定
    // 码率摊平发出。原画档 IDR 一坨几百 KB 直接灌 UDP = 瞬间几百 Mbps 微突发 →
    // 丢包 → NACK 重传风暴 → 客户端拖窗巨卡、帧率腰斩。
    let mut send_time = Instant::now();
    let mut win_start = Instant::now();
    let mut win_frames: u64 = 0;
    let mut win_bytes: u64 = 0;
    let mut downgraded = false;
    let mut downgrade_from = 0u32;
    let mut paced = Duration::ZERO;
    // 软编兜底后多久允许再试 NVENC（0 = 现在就可以试）。
    let mut last_feed = Instant::now();
    let mut nvenc_retry_at = Instant::now();
    // 关键帧请求限流：PLI/FIR 会连打（客户端解码器一慌就发），每次重启 ffmpeg =
    // 掉帧 + IDR 码率尖峰——正是"码率振荡、帧间隔 100ms+"的来源。3s 内连打只重启一次。
    let mut last_rebuild = Instant::now();
    // 空闲保活与自适应的地基：上一个成品帧的时刻/内容，以及上次真正发出的时刻。
    let mut last_unit = Instant::now();
    let mut last_au: Option<Vec<u8>> = None;
    let mut last_emit = Instant::now();
    let started = Instant::now();

    loop {
        // 软编按 fps×2 tick；NVENC 抓屏在 ffmpeg 里配速，本端只是取成品帧，
        // 轮询间隔直接决定端到端延迟，压到 4 ms。
        std::thread::sleep(if backend_nvenc { Duration::from_millis(4) } else { Duration::from_nanos(1_000_000_000 / ((fps.max(1) as u64) * 2)) });
        let now = Instant::now();
        // 分辨率可能被动改变（客户端 xrandr 切模式 / 手动改显示）：每轮实时量一次根窗口，
        // 变了就重开采集 + 重建后端/比对器/时间线，首帧自然是 IDR。
        let geometry = screen.measure();
        if geometry != last_reported {
            last_reported = geometry;
            *settings.native.lock().unwrap() = geometry;
            width = geometry.0 as usize & !1;
            height = geometry.1 as usize & !1;
            match X11Screen::open(display.as_deref(), DamageLevel::BoundingBox) {
                Ok(s) => screen = s,
                Err(e) => eprintln!("   重开采集失败：{}", e.0),
            }
            rebuild_backend(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop, linux_uuremote::nvenc::preset_cq(settings.quality.load(Ordering::Relaxed)), settings.chroma.load(Ordering::Relaxed) == 1, display);
                win_start = Instant::now();
                win_frames = 0;
                win_bytes = 0;
                paced = Duration::ZERO;
                send_time = Instant::now();
            content = ContentChange::new(width, height, 32, 18);
            timeline = RtpTimeline::new(fps);
            println!("   屏幕尺寸变化 → {}x{}（已重建编码后端，首帧 IDR）", geometry.0, geometry.1);
        }
        // 远程改帧率立刻生效（档位 + 睡眠节奏；NVENC 连进程一起重建，framerate 参数在建进程时定）。
        let want_fps = (settings.moving_fps.load(Ordering::Relaxed) as u32).max(1);
        // 客户端会按菜单值反复重发同一个帧率请求：自适应降速后若照单全收，
        // 每 3 s 就「改回 60 → 又降下来」来回重建编码器（IDR 风暴 → frm 横跳、帧率腰斩）。
        // 只有用户选了新档位（与降速来源不同的值）才真正接受。
        if want_fps != fps && !(downgraded && want_fps == downgrade_from) {
            fps = want_fps;
            // 用户重新选了档位 → 允许再做一次自适应降帧。
            downgraded = false;
            downgrade_from = 0;
            let still = settings.still_fps.load(Ordering::Relaxed) as u32;
            gate.set_config(GateConfig { moving_fps: fps, still_fps: still, still_after: Duration::from_millis(1000) });
            planner = FramePlanner::new(fps, still);
            timeline = RtpTimeline::new(fps);
            if backend_nvenc {
                rebuild_backend(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop, linux_uuremote::nvenc::preset_cq(settings.quality.load(Ordering::Relaxed)), settings.chroma.load(Ordering::Relaxed) == 1, display);
                win_start = Instant::now();
                win_frames = 0;
                win_bytes = 0;
                paced = Duration::ZERO;
                send_time = Instant::now();
            }
            println!("   远程改帧率 → {fps} fps");
        }
        // 远程改码率 → 重建后端（两个后端的码率都在建后端时定），重建后首帧必 IDR。
        // quality=自动 报 0：保持当前码率不动。
        let want_bitrate = settings.bitrate_kbps.load(Ordering::Relaxed) as u32;
        if want_bitrate != 0 && want_bitrate != applied_bitrate {
            applied_bitrate = want_bitrate;
            rebuild_backend(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, want_bitrate, gop, linux_uuremote::nvenc::preset_cq(settings.quality.load(Ordering::Relaxed)), settings.chroma.load(Ordering::Relaxed) == 1, display);
            planner.note_keyframe_request();
            println!("   远程改码率 → {want_bitrate} kbps（编码后端已重建）");
        }
        // 真彩开关（apply_rpc 写 settings.chroma）→ 重建后端换 pix_fmt，首帧自然 IDR。
        let want_chroma = settings.chroma.load(Ordering::Relaxed);
        if want_chroma != last_chroma {
            last_chroma = want_chroma;
            if backend_nvenc {
                rebuild_backend(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop, linux_uuremote::nvenc::preset_cq(settings.quality.load(Ordering::Relaxed)), want_chroma == 1, display);
            }
            planner.note_keyframe_request();
            println!("   色度切换 → {}", if want_chroma == 1 { "4:4:4 真彩" } else { "4:2:0" });
        }
        // 软编兜底之后按 30 s 限流重试硬编（会话槽可能已经释放）。重试失败会在
        // rebuild_backend 里落回软编，不会连打。
        if !backend_nvenc && encoder.is_some() && Instant::now() >= nvenc_retry_at {
            nvenc_retry_at = Instant::now() + Duration::from_secs(30);
            backend_nvenc = true;
            rebuild_backend(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop, linux_uuremote::nvenc::preset_cq(settings.quality.load(Ordering::Relaxed)), settings.chroma.load(Ordering::Relaxed) == 1, display);
                win_start = Instant::now();
                win_frames = 0;
                win_bytes = 0;
                paced = Duration::ZERO;
                send_time = Instant::now();
            if backend_nvenc {
                println!("   重试 NVENC 成功，切回硬编");
            }
        }
        // 客户端自己画本地光标（Windows 光标），视频里没有我们的指针；指针移动仍是
        // "有人在用"的唯一证据，用它维持 moving 档（软编门控用；NVENC 恒发只是日志）。
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
            let since_last = frames_sent;
            println!(
                "   出帧速率 {:.1} fps（近 {secs:.0}s 共 {since_last} 帧 ≈ {} kbps，后端 {}，门控 moving={}）",
                since_last as f64 / secs,
                win_bytes as f64 * 8.0 / win_start.elapsed().as_secs_f64().max(0.1) / 1000.0,
                if backend_nvenc { "NVENC" } else { "软编" },
                gate.is_moving()
            );
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
        // 光标形状单独上报（官方画面不含指针，客户端拿形状贴图换掉自己的箭头）。
        // NVENC 模式的抓屏在 ffmpeg 进程里做，这段照旧跑：一次 XFixes 取图 0.05 ms。
        // UU_COMPOSITE_CURSOR=1 才把光标合成进画面（旧行为）。
        let sprite = screen.fetch_cursor().ok();
        if let Some(sprite) = &sprite {
            if !sprite.is_empty() {
                let mut sig: u64 = 0xcbf29ce484222325;
                let mix = |h: &mut u64, v: u64| {
                    *h ^= v;
                    *h = h.wrapping_mul(0x100000001b3);
                };
                mix(&mut sig, (u64::from(sprite.width) << 32) | u64::from(sprite.height));
                mix(&mut sig, (u64::from(sprite.hotspot_x) << 32) | u64::from(sprite.hotspot_y));
                for &px in &sprite.argb[..sprite.pixel_count()] {
                    mix(&mut sig, u64::from(px));
                }
                if sig != last_shape_hash {
                    last_shape_hash = sig;
                    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                    let _ = shapes_tx.send(linux_uuremote::cursor::encode_cursor_shape(next_sequence(), ts, sprite));
                }
            }
        }

        if backend_nvenc {
            // NVENC 模式不做内容门控：恒帧率恒质量，客户端右下角才不会横跳。
            // 抓屏走我们的 XShm（实测 5.6 ms，比 ffmpeg 的 x11grab/XGetImage 快一倍），
            // 整帧喂进编码器；画面不含指针（光标靠 Message 15 上报），所以不用合成也不用克隆。
            let alive = pipe.as_mut().map(|hw| hw.alive()).unwrap_or(false);
            if !alive {
                println!("   NVENC 进程已退出（本次会话改用软编兜底，30 s 后再试硬编）");
                planner.note_failure();
                fallback_to_software(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop);
                win_start = Instant::now();
                win_frames = 0;
                win_bytes = 0;
                paced = Duration::ZERO;
                nvenc_retry_at = Instant::now() + Duration::from_secs(30);
                continue;
            }
            if keyframe.load(Ordering::Relaxed) && Instant::now() >= last_rebuild + Duration::from_secs(3) {
                // zerolatency + 无 lookahead 的 NVENC 没法从外部塞 IDR，重启进程即首帧 IDR。
                keyframe.store(false, Ordering::Relaxed);
                rebuild_backend(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop, linux_uuremote::nvenc::preset_cq(settings.quality.load(Ordering::Relaxed)), settings.chroma.load(Ordering::Relaxed) == 1, display);
                win_start = Instant::now();
                win_frames = 0;
                win_bytes = 0;
                paced = Duration::ZERO;
                send_time = Instant::now();
                last_rebuild = Instant::now();
                println!("   关键帧请求 → NVENC 重启（首帧 IDR；3s 内连打只重启一次）");
            }
            // 先把已经编完的帧取走：取帧间隔直接决定端到端延迟，不能等下一次喂帧。
            let mut drained = 0u64;
            loop {
                let unit = match pipe.as_mut().and_then(|hw| hw.next_unit(Duration::ZERO)) {
                    Some(unit) => unit,
                    None => break,
                };
                let unit_len = unit.len() as u64;
                let keep = unit.clone();
                let s0 = Instant::now();
                if frames_tx.blocking_send(Encoded { data: unit, duration: timeline.duration_after(Instant::now()) }).is_err() {
                    return;
                }
                acc_send += s0.elapsed();
                frames_sent += 1;
                emitted += 1;
                drained += 1;
                win_frames += 1;
                win_bytes += unit_len;
                last_au = Some(keep);
                last_emit = Instant::now();
                last_unit = Instant::now();
                // 配速对齐官方 libwebrtc pacer：恒定 2.5× 码率串行发送，欠账封顶
                // 两帧间隔。零丢包旁路会让 IDR 整坨轰上 Wi-Fi——到达忽密忽疏，
                // 客户端帧率显示就在 80~90 之间来回摆；丢包出现又切回配速，
                // 状态乒乓本身就是振荡源。稳态内容远低于 2.5× 码率，配速不拖帧率。
                let pace_kbps = (applied_bitrate as u64 * 5 / 2).max(10_000);
                send_time += Duration::from_micros(unit_len * 8 * 1_000 / pace_kbps);
                let pace_now = Instant::now();
                if send_time > pace_now {
                    let gap = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
                    let wait = (send_time - pace_now).min(gap);
                    std::thread::sleep(wait);
                    paced += wait;
                    // 欠账封顶两个帧间隔：突发最多摊到两帧，不会一直憋着清账。
                    send_time = send_time.min(Instant::now() + gap * 2);
                }
                if emitted % 300 == 0 {
                    println!("   已发 {emitted} 帧，{:.1} fps", emitted as f64 / started.elapsed().as_secs_f64().max(0.001));
                }
            }
            if drained == 0 && last_unit.elapsed() > Duration::from_millis(250) {
                // 画面静止（x11grab vfr 不产帧）：这段空闲要从"编码器跟不上"的
                // 分母里扣掉，否则空闲几十秒后一统计就误判降帧——空闲掉到个位数
                // 帧率是设计行为（官方 idle repeat），不是编码器吃不下。
                win_start += last_unit.elapsed() - Duration::from_millis(250);
            }
            if drained > 0 {
                gate.note_change(now);
            }
            // 空闲保活：官方被控端静止时按 still_fps 重发上一帧（"idle repeat"）。
            // x11grab 空闲不产帧，重发上一个 AU 让客户端解码器活着、状态条 frm
            // 不至于显示几万毫秒。多 AUD 的 AU（起始码成串）不重发，避免双起始码。
            if self_capture && backend_nvenc && still_fps > 0 {
                if let Some(last) = last_au.as_ref() {
                    let aus = last.windows(4).filter(|w| *w == [0, 0, 0, 1]).count();
                    if aus <= 1 && now.duration_since(last_emit) >= Duration::from_millis(1_000 / still_fps as u64) {
                        if frames_tx.blocking_send(Encoded { data: last.clone(), duration: Duration::from_millis(1_000 / still_fps as u64) }).is_ok() {
                            last_emit = now;
                        }
                    }
                }
            }
            if self_capture {
                // 抓屏在 ffmpeg 进程里按 -framerate 自配速，本端只取成品帧。
                if !downgraded && fps > 15 && settings.connected.load(Ordering::Relaxed) && win_start.elapsed() >= Duration::from_secs(3) {
                    let sustained = win_frames as f64 / win_start.elapsed().saturating_sub(paced).as_secs_f64().max(0.001);
                    if sustained < f64::from(fps) * 0.85 {
                        let target = (sustained.floor() as u32).max(15).min(fps - 1);
                        println!("   持续出帧率 {sustained:.1} fps < 设定 {fps} fps → 时间线配速降到 {target} fps（不重启编码器：重启 = 掉帧 + IDR 尖峰）");
                        downgrade_from = fps;
                        fps = target;
                        downgraded = true;
                        timeline = RtpTimeline::new(fps);
                    }
                }
                if win_start.elapsed() >= Duration::from_secs(3) {
                    win_start = Instant::now();
                    win_frames = 0;
                    win_bytes = 0;
                    paced = Duration::ZERO;
                }
                continue;
            }
            // 按 fps 配速喂帧：循环 tick 比帧间隔短，不配速会喂进两倍帧。
            if now.duration_since(last_feed) < Duration::from_nanos(1_000_000_000 / fps.max(1) as u64) {
                continue;
            }
            last_feed = now;
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
            let fed = if composite_cursor {
                // 只有显式开画面合成时才克隆（多一次 14.7 MB 拷贝）。
                let mut out = frame.clone();
                if let (Some(sprite), Some(pos)) = (sprite.as_ref(), screen.cursor_position()) {
                    screen.composite_cursor(&mut out, sprite, pos);
                }
                out
            } else {
                frame
            };
            let e0 = Instant::now();
            match pipe.as_mut().map(|hw| hw.feed(&fed)) {
                Some(Ok(())) => {
                    acc_encode += e0.elapsed();
                }
                Some(Err(e)) => {
                    eprintln!("   NVENC 喂帧失败：{e}（本次会话退回软编，30 s 后再试硬编）");
                    planner.note_failure();
                    fallback_to_software(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop);
                win_start = Instant::now();
                win_frames = 0;
                win_bytes = 0;
                paced = Duration::ZERO;
                    nvenc_retry_at = Instant::now() + Duration::from_secs(30);
                }
                None => {
                    println!("   NVENC 管道没了（切软编）");
                    fallback_to_software(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop);
                win_start = Instant::now();
                win_frames = 0;
                win_bytes = 0;
                paced = Duration::ZERO;
                    nvenc_retry_at = Instant::now() + Duration::from_secs(30);
                }
            }
            // 自适应降帧（官方二进制有同款 "adjust fps to" 逻辑）：编码器吃不下设定
            // 帧率时，喂帧配速会按名义 fps 把帧灌进 stdin 排队——队列就是延迟和卡顿。
            // 连续 3 s 出帧率低于设定 85% 就把配速降到实测值（不重启编码器：喂帧管线
            // 的重建会断流；真正要降的是喂帧节奏，rebuild 对 stdin 管线没必要）。
            if !downgraded && fps > 15 && settings.connected.load(Ordering::Relaxed) && win_start.elapsed() >= Duration::from_secs(3) {
                // 编码器吞吐必须剔除配速睡眠：配速是发送策略，不是编码器吃不下。
                let sustained = win_frames as f64 / win_start.elapsed().saturating_sub(paced).as_secs_f64().max(0.001);
                if sustained < f64::from(fps) * 0.85 {
                    let target = (sustained.floor() as u32).max(15).min(fps - 1);
                    println!("   持续出帧率 {sustained:.1} fps < 设定 {fps} fps → 配速降到 {target} fps（不重建后端）");
                    downgrade_from = fps;
                    fps = target;
                    downgraded = true;
                    timeline = RtpTimeline::new(fps);
                    win_start = Instant::now();
                    win_frames = 0;
                    win_bytes = 0;
                    paced = Duration::ZERO;
                }
            }
            if win_start.elapsed() >= Duration::from_secs(3) {
                win_start = Instant::now();
                win_frames = 0;
                win_bytes = 0;
                paced = Duration::ZERO;
            }
            continue;
        }

        // 软编兜底路径：门控 + 本端抓屏 + 逐块比对。
        match gate.decide(now) {
            Action::Idle => continue,
            Action::Keepalive | Action::Capture => {}
        }
        if keyframe.swap(false, Ordering::Relaxed) {
            planner.note_keyframe_request();
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
        let mut out = frame.clone();
        if composite_cursor {
            if let (Some(sprite), Some(pos)) = (sprite.as_ref(), screen.cursor_position()) {
                screen.composite_cursor(&mut out, sprite, pos);
            }
        }
        let plan = planner.plan(gate.is_moving());
        let e0 = Instant::now();
        match encoder.as_mut().map(|encoder| encoder.encode_bgra(&out, plan.force_idr)) {
            Some(Ok((au, kind))) => {
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
                if frames_tx.blocking_send(Encoded { data: au, duration: timeline.duration_after(Instant::now()) }).is_err() {
                    return;
                }
                acc_send += s0.elapsed();
            }
            Some(Err(e)) => {
                eprintln!("   编码失败：{e}");
                planner.note_failure();
            }
            None => {
                eprintln!("   编码器没了（重建失败）");
                return;
            }
        }
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::next_reconnect_delay;

    #[test]
    fn first_failure_backs_off_two_seconds() {
        assert_eq!(next_reconnect_delay(0, 0), 2);
    }

    #[test]
    fn consecutive_failures_double_up_to_thirty() {
        assert_eq!(next_reconnect_delay(1, 0), 4);
        assert_eq!(next_reconnect_delay(2, 0), 8);
        assert_eq!(next_reconnect_delay(3, 0), 16);
        assert_eq!(next_reconnect_delay(4, 0), 30);
        assert_eq!(next_reconnect_delay(9, 0), 30);
    }

    #[test]
    fn a_long_live_run_resets_the_backoff() {
        // 上一轮活过 10 秒说明服务器可达，只是会话断了：回到基础退避。
        assert_eq!(next_reconnect_delay(5, 12), 2);
        assert_eq!(next_reconnect_delay(5, 9), 30);
    }
}
