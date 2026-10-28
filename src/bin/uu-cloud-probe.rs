//! uu-cloud-probe —— 连接层验收工具：建房 → WSS 握手（X-NRD-AUTH）→ room_info ack
//! → 等客户端拉流（be-controlled / soac offer）。
//!
//! 这是"信令通了吗"的最小可跑证据；不碰 WebRTC 本体。输出只有事件名、状态与
//! 形状摘要，token/ticket/SDP 内容永不打印。

use linux_uuremote::api::{ApiClient, Credentials, RoomError};
use linux_uuremote::config;
use linux_uuremote::signaling::{self, Received, Signaling};
use linux_uuremote::wire;

#[tokio::main]
async fn main() {
    let wait_s: u64 = std::env::args()
        .position(|a| a == "--wait")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    if let Err(e) = run(wait_s).await {
        eprintln!("probe 失败：{e}");
        std::process::exit(1);
    }
}

async fn run(wait_s: u64) -> Result<(), String> {
    let (file, _borrowed) = config::read_with_fallback()?;
    let creds = Credentials::load(file.as_ref())?;
    let api = ApiClient::new(creds.clone());

    println!("1) 建房…");
    let room = api.create_room().await.map_err(|e| match e {
        RoomError::LoginExpired => "登录 token 已失效：重跑 uu-login 扫码。".to_string(),
        RoomError::Other(msg) => msg,
    })?;
    let server = room.signaling_server.clone();
    println!("   signaling_server = {server}");

    println!("2) WSS 握手（X-NRD-AUTH）…");
    let mut sig = Signaling::connect(&server, &room.token, room.ws_connect_timeout_ms).await?;
    println!("   CONNECT_ACK ✓（命名空间已连）");

    println!("3) room_info…");
    sig.send_with_ack(10, signaling::EVENT_ROOM_INFO, None).await?;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), sig.recv()).await;
    match frame.map_err(|_| "room_info ack 超时（10 s）")?? {
        Received::Ack(_id, v) => println!("   room_info ack ✓：{}", wire::json_shape(&v, 2)),
        Received::Event(e, p) => println!("   先收到事件 {e}：{}", p.as_ref().map(|p| wire::json_shape(p, 2)).unwrap_or_default()),
        Received::BinaryEvent(e, _, att) => println!("   先收到 binary 事件 {e}（附件 {} 段）", att.len()),
        Received::Closed => return Err("信令在 room_info 前关闭".into()),
    }
    println!("   —— 信令链路已通 ——");

    println!("4) 等客户端拉流（be-controlled / soac，≤{wait_s}s）…");
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(wait_s));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                println!("   超时：没有客户端来拉流。信令通、登记就位——拿 App 扫这台设备即会触发 be-controlled。");
                return Ok(());
            }
            r = sig.recv() => match r? {
                Received::Event(e, p) => match e.as_str() {
                    signaling::EVENT_SOAC => {
                        let empty = serde_json::Value::Null;
                        let payload = p.as_ref().unwrap_or(&empty);
                        let inner = payload.get("data").unwrap_or(payload);
                        let kind = inner.get("type").and_then(serde_json::Value::as_str).unwrap_or("?");
                        println!("   soac {kind}：{}", wire::json_shape(inner, 3));
                        if kind == "answer" { return Ok(()); }
                    }
                    signaling::EVENT_BE_CONTROLLED => println!("   be-controlled(json)：{}", p.as_ref().map(|p| wire::json_shape(p, 3)).unwrap_or_default()),
                    signaling::EVENT_BMSG_PUSH => println!("   bmsg_push：{}", p.as_ref().map(|p| wire::json_shape(p, 2)).unwrap_or_default()),
                    signaling::EVENT_RELEASED | signaling::EVENT_LEFT => println!("   {e}（对端离开）"),
                    other => println!("   {other}: {}", p.as_ref().map(|p| wire::json_shape(p, 1)).unwrap_or_default()),
                },
                Received::BinaryEvent(e, header, att) => {
                    println!("   {e}(binary)：header={} 附件 {} 段 {}",
                        header.as_ref().map(|h| wire::json_shape(h, 2)).unwrap_or_default(),
                        att.len(),
                        att.first().map(|b| wire::wire_shape(b, 3)).unwrap_or_default());
                    if e == signaling::EVENT_BE_CONTROLLED {
                        println!("   —— 客户端拉流请求到达；下一步是 soac answer（WebRTC 层，未实现）——");
                        return Ok(());
                    }
                }
                Received::Ack(id, v) => println!("   ack {id:?}: {}", wire::json_shape(&v, 2)),
                Received::Closed => println!("   信令关闭"),
            }
        }
    }
}
