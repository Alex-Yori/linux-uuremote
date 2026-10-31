//! 控制端：join → 信令 control → offer/answer/candidates → 输入/设置 RPC → 保活。
//! SDL 线程与 tokio 线程之间只走两条 std mpsc 通道（帧下行、命令上行）+ 一组原子量。
pub mod icons;
pub mod input;
pub mod media;
pub mod png;
pub mod rtc;
pub mod signal;
pub mod wire;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use self::rtc::{configuration, Host, Peer};
use self::signal::Signal;

#[derive(Deserialize)]
pub struct RoomData {
    #[serde(default)] pub token: String,
    #[serde(default)] pub signaling_list: Vec<String>,
    #[serde(default)] pub ws_connect_timeout_ms: i32,
}
#[derive(Deserialize)]
struct JoinBody { data: Option<RoomData> }

/// SDL → tokio 的命令。
pub enum Cmd {
    MouseMove { fx: f64, fy: f64 },
    /// 游戏鼠标档：SDL relative 模式采到的位移，按 Windows notch 语义原样投给被控端。
    MouseRelative { dx: i32, dy: i32 },
    MouseButton { button: i32, pressed: bool },
    Scroll { dx: i32, dy: i32 },
    Key { vk: i32, pressed: bool },
    /// 原样投给 CONTROL 通道的 protobuf（画质/帧率/分辨率/真彩 RPC）。
    Rpc(Vec<u8>),
    Text(String),
}

pub struct Session {
    peer: Peer,
    signal: Signal,
    candidates: mpsc::Receiver<Value>,
}

impl Session {
    pub async fn open(room: &RoomData, shape: &wire::Shape, device: &str, host: Arc<Host>, timeout: Duration) -> Result<Self, String> {
        let ws_timeout = if room.ws_connect_timeout_ms > 0 { Duration::from_millis(room.ws_connect_timeout_ms.max(1000) as u64) } else { timeout };
        let mut signal = Signal::connect(&room.signaling_list, &room.token, ws_timeout).await?;
        let app = wire::uuid();
        let ice = hex::encode(rand::random::<[u8; 16]>());
        let result = async {
            let ack = signal.control(shape.control_header(&app), shape.connect_options(device), timeout).await?;
            if ack.get("app_control_id").is_some_and(|v| v.as_str() != Some(app.as_str())) {
                return Err("control 应答属于别的会话".into());
            }
            // 线上观测：云端在 ack 里另发会话 ice_id，不回显我们提的那个。
            let ice = ack.get("ice_id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 128).map(str::to_owned).unwrap_or(ice);
            let remote = ack.get("client_id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("control 应答缺少对端 client_id")?;
            let config = configuration(&ack)?;
            let (tx, rx) = mpsc::channel(256);
            let peer = Peer::new(config, shape, &app, &ice, remote, tx, host, Default::default()).await?;
            // 被控端要先处理完 be-controlled 才认这个 ice_id，offer 晚一点点发（参照实现的实测教训）。
            tokio::time::sleep(Duration::from_millis(300)).await;
            match peer.offer().await {
                Ok(offer) => match signal.soac(offer).await {
                    Ok(()) => { println!("   offer 已发出，等待对端 answer…"); Ok((peer, rx)) }
                    Err(e) => { peer.close().await; Err(e) }
                },
                Err(e) => { peer.close().await; Err(e) },
            }
        }
        .await;
        match result {
            Ok((peer, candidates)) => Ok(Self { peer, signal, candidates }),
            Err(e) => { signal.close().await; Err(e) },
        }
    }

    /// 跑会话直到退出/断开。输入命令从 cmd_rx 里捞，投进 CONTROL 通道。
    pub async fn run(&mut self, cmd_rx_std: std::sync::mpsc::Receiver<Cmd>, host: Arc<Host>) -> Result<(), String> {
        // std 接收端不能在选择循环里反复 move 进 spawn_blocking；用一条专门的阻塞
        // 线程把它桥成 tokio 通道。SDL 退出时 Sender dropped → recv 返回 Err → 会话结束。
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            while let Ok(cmd) = cmd_rx_std.recv() {
                if cmd_tx.send(cmd).is_err() { break; }
            }
        });
        let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        let mut ping_id: u64 = 0;
        // 本机剪贴板推流节拍（只推本机来源的变化，远端写回来的不再弹回）。
        let mut clip_tick = tokio::time::interval(Duration::from_millis(250));
        let mut clip_last: Option<u64> = None;
        let mut clip_first = true;
        let clip_ids = crate::clipboard::ChunkIds::default();
        let mut clip_seq: u64 = 2_000_000_000;
        let deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline, if !host.stats.echo_seen.load(Ordering::Acquire) => {
                    return Err("30 秒内没等到被控端 echo（多半没连上或被拒绝）".into());
                }
                cmd = cmd_rx.recv() => {
                    // None = SDL 退出（发送端全 drop）→ 会话正常收尾。
                    let Some(cmd) = cmd else { return Ok(()); };
                            let payload = match cmd {
                                Cmd::MouseMove { fx, fy } => json!({"action": "mouse_move_absolute", "abs_x": fx, "abs_y": fy}).to_string().into_bytes(),
                                Cmd::MouseRelative { dx, dy } => json!({"action": "mouse_move_relative", "delta_x": dx, "delta_y": dy}).to_string().into_bytes(),
                                Cmd::MouseButton { button, pressed } => json!({"action": if pressed { "mouse_press" } else { "mouse_release" }, "button": button}).to_string().into_bytes(),
                                Cmd::Scroll { dx, dy } => json!({"action": "mouse_scroll", "delta_x": dx, "delta_y": dy}).to_string().into_bytes(),
                                Cmd::Key { vk, pressed } => json!({"action": if pressed { "kbd_press" } else { "kbd_release" }, "key": vk}).to_string().into_bytes(),
                                Cmd::Rpc(bytes) => bytes,
                                Cmd::Text(text) => text.into_bytes(),
                            };
                            let _ = self.peer.send_control(&payload).await;
                }
                _ = heartbeat.tick(), if host.stats.control_open.load(Ordering::Acquire) => {
                    // 官方客户端每秒的键盘保活（被控端拿它判断键盘占用）。
                    let _ = self.peer.send_control(br#"{"heartbeat":"kbd_heartbeat"}"#).await;
                }
                _ = ping.tick(), if host.stats.control_open.load(Ordering::Acquire) => {
                    ping_id += 1;
                    let seq = 1_000_000_000 + ping_id;
                    host.pings.lock().unwrap_or_else(|e| e.into_inner()).insert(seq, std::time::Instant::now());
                    let _ = self.peer.send_control(&wire::echo_request(seq)).await;
                }
                _ = clip_tick.tick(), if host.clipboard.as_ref().is_some_and(|cb| cb.ready) && host.stats.control_open.load(Ordering::Acquire) => {
                    // 本机 CLIPBOARD 变化 → Message 17 分段推给被控端（长文本同一 content_id）。
                    let Some(cb) = host.clipboard.clone() else { continue };
                    let snap = cb.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let push = snap.text.clone().filter(|_| clip_first || (Some(snap.version) != clip_last && !snap.from_remote));
                    let Some(text) = push else { continue };
                    clip_first = false;
                    clip_last = Some(snap.version);
                    match crate::clipboard::chunks(&text) {
                        Ok(parts) => {
                            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                            let id = clip_ids.next();
                            let mut ok = true;
                            for (i, part) in parts.iter().enumerate() {
                                clip_seq += 1;
                                let msg = crate::clipboard::encode_clipboard_message(clip_seq, ts, id, part, i + 1 < parts.len());
                                if self.peer.send_control(&msg).await.is_err() { ok = false; break; }
                            }
                            // 内容属用户数据，日志只报字节数与段数。
                            if ok { println!("   剪贴板出站：本机 {} 字节（{} 段）→ 被控端", text.len(), parts.len()); }
                        }
                        Err(e) => println!("   剪贴板推流跳过：{e}"),
                    }
                }
                candidate = self.candidates.recv() => if let Some(candidate) = candidate { self.signal.soac(candidate).await? },
                event = self.signal.event() => {
                    let event = event?;
                    let name = event.get(0).and_then(Value::as_str).unwrap_or("-");
                    match name {
                        "soac" => {
                            if let Some(body) = event.get(1) {
                                println!("   收到信令 soac: {:?}", body.get("data").and_then(|d| d.get("type").or_else(|| body.get("type"))).and_then(Value::as_str).unwrap_or("-"));
                                self.peer.receive(body).await?;
                            }
                        }
                        "released" | "left" | "leave" => {
                            // 别的 peer 的事件不能把这个会话拆了（参照实现的教训）。
                            if self.peer.matches_route(event.get(1).unwrap_or(&Value::Null)) {
                                return Err("被控端会话已结束（对方释放或断开）".into());
                            }
                        }
                        other => println!("   信令事件：{other}"),
                    }
                }
            }
        }
    }

    pub async fn close(&mut self) {
        self.peer.close().await;
        self.signal.close().await;
    }
}

/// POST /api/v1/room/join/by_device/{id} → 房间 token + 信令列表。
pub async fn join(api: &crate::api::ApiClient, device: &str) -> Result<RoomData, String> {
    if std::env::var("UU_VIEWER_DEBUG").is_ok_and(|v| v == "1") {
        println!("   join 调试：device={device} req_device={} client={} csrf={} token={}",
            api.creds.device_id, api.creds.client_id, api.creds.csrf.len(), api.creds.token.len());
    }
    let (status, body) = api.post_json(&format!("/api/v1/room/join/by_device/{device}"), &json!({"force_join": false})).await?;
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let msg = body.get("msg").and_then(Value::as_str).unwrap_or("");
        // 2002 = 被控端正在被远控中。
        return Err(format!("join 失败 code={code} {msg}"));
    }
    let _ = status;
    let parsed: JoinBody = serde_json::from_value(body).map_err(|_| "join 应答格式不对")?;
    let room = parsed.data.ok_or("join 应答没有 data")?;
    if room.token.is_empty() || room.signaling_list.is_empty() { return Err("服务器没给房间 token 或信令地址".into()); }
    Ok(room)
}
