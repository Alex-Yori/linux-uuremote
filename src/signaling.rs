//! socket.io（Engine.IO v4 + Socket.IO v5）信令客户端。
//!
//! 协议事实直接继承参照实现（`uuremote-server`，本机在跑、云端连接可用），不是通用
//! socket.io 教科书——实测差异都在这：
//! - WSS URL = `{signaling_server}/socket.io/?transport=websocket&EIO=4`，
//!   **房间 token 走 HTTP 头 `X-NRD-AUTH`（裸 token，无 Bearer 前缀）**，命名空间
//!   CONNECT 不带 payload——token 放 payload 会被 403。
//! - 握手：等 open 帧 '0'（内含 pingInterval/pingTimeout）→ 客户端发 '40'（无 payload）
//!   → 服务器回 '40{...}' 即 CONNECTED；'44' = CONNECT_ERROR。
//! - 事件帧 '42' + JSON `[event, payload?]`；带 ack 的请求帧是 `42<ack_id>[event]`
//!   （服务器用 '43<ack_id>[data]` 回 ack）。心跳：服务器发 '2'，客户端回 '3'；
//!   Engine.IO v4 语义——pingInterval+pingTimeout 窗口内没收到 ping 即判死
//!   （半开 TCP 会让读侧永远阻塞，宿主会静默离线，必须有判死窗口）。
//! - 被控端事件面（参照实现源码 + 安卓 Java 包 jadx 静态逆向双重印证）：
//!   `be-controlled`（服务器推送，常为 binary event '45' + ConnectOptions protobuf
//!   附件）、`soac`（WebRTC offer/answer JSON，ICE 走 SDP 不是独立 candidate 事件）、
//!   `bmsg_push`（房间成员变更）、`released`/`left`/`leave`（对端离开）。
//!   客户端主动事件：`room_info`（带 ack id）、`refresh_reconnect_key`
//!   （官方 RefreshSignalTokenLoop：首 30 s、成功每 300 s、失败 30 s 重试）、`soac`。

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

pub const EVENT_ROOM_INFO: &str = "room_info";
pub const EVENT_REFRESH_RECONNECT_KEY: &str = "refresh_reconnect_key";
pub const EVENT_BE_CONTROLLED: &str = "be-controlled";
pub const EVENT_SOAC: &str = "soac";
pub const EVENT_BMSG_PUSH: &str = "bmsg_push";
pub const EVENT_RELEASED: &str = "released";
pub const EVENT_LEFT: &str = "left";

/// 解析后的 Engine.IO / Socket.IO 帧。
#[derive(Debug, PartialEq)]
pub enum Frame {
    /// '0'：会话建立，带服务器心跳参数（毫秒）。
    Open { sid: String, ping_interval_ms: u64, ping_timeout_ms: u64 },
    Ping,
    Pong,
    Close,
    /// '40'：命名空间 CONNECT / CONNECT_ACK（服务器回 '40{}' 即已连上）。
    NamespaceConnect(Option<Value>),
    /// '41'：命名空间 DISCONNECT。
    NamespaceDisconnect,
    /// '42'（可带 ack id）：事件。
    Event(String, Option<Value>),
    /// '43'：ACK 回调。
    Ack(Option<u64>, Value),
    /// '45'/'46'：binary 事件/ack 的 header；附件随后以 binary 帧到达。
    BinaryEvent(String, Option<Value>),
    /// '44'：CONNECT_ERROR。
    ConnectError(Option<Value>),
    /// 其他或畸形帧。
    Other(String),
}

/// 帧解码：前缀按 Socket.IO 固定形态匹配（'43'/'42' 后可能紧跟 ack 数字，
/// 不能按"数字前缀"切 kind）。
pub fn decode_frame(msg: &str) -> Frame {
    if let Some(rest) = msg.strip_prefix("40") {
        return Frame::NamespaceConnect(if rest.trim().is_empty() { None } else { serde_json::from_str(rest).ok() });
    }
    if msg == "41" {
        return Frame::NamespaceDisconnect;
    }
    if let Some(rest) = msg.strip_prefix("42") {
        // '42' 后可跟 ack id 数字，JSON 从 '[' 起。
        let begin = rest.find('[').unwrap_or(0);
        let ack_id = rest[..begin].parse::<u64>().ok();
        let _ = ack_id; // 发出的 ack 才会收到对应 '43'；收到的 42 的 id 无需保留
        return match serde_json::from_str::<Vec<Value>>(&rest[begin..]) {
            Ok(arr) if !arr.is_empty() => Frame::Event(
                arr[0].as_str().unwrap_or("").to_string(),
                arr.get(1).cloned(),
            ),
            _ => Frame::Other(msg.to_string()),
        };
    }
    if let Some(rest) = msg.strip_prefix("43") {
        let begin = rest.find('[').unwrap_or(0);
        return match serde_json::from_str::<Value>(&rest[begin..]) {
            Ok(v) => Frame::Ack(rest[..begin].parse::<u64>().ok(), v),
            Err(_) => Frame::Other(msg.to_string()),
        };
    }
    if let Some(rest) = msg.strip_prefix("44") {
        return Frame::ConnectError(if rest.trim().is_empty() { None } else { serde_json::from_str(rest).ok() });
    }
    if msg.starts_with("45") || msg.starts_with("46") {
        let body = msg.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
        return match serde_json::from_str::<Vec<Value>>(body) {
            Ok(arr) if !arr.is_empty() => Frame::BinaryEvent(
                arr[0].as_str().unwrap_or("").to_string(),
                arr.get(1).cloned(),
            ),
            _ => Frame::Other(msg.to_string()),
        };
    }
    match msg {
        "2" => Frame::Ping,
        "3" => Frame::Pong,
        "1" => Frame::Close,
        _ if msg.starts_with('0') => {
            let v: Value = serde_json::from_str(&msg[1..]).unwrap_or(Value::Null);
            Frame::Open {
                sid: v.get("sid").and_then(Value::as_str).unwrap_or("").to_string(),
                ping_interval_ms: v.get("pingInterval").and_then(Value::as_u64).unwrap_or(25_000),
                ping_timeout_ms: v.get("pingTimeout").and_then(Value::as_u64).unwrap_or(20_000),
            }
        }
        _ => Frame::Other(msg.to_string()),
    }
}

/// '42' 事件帧编码（无 ack id，对齐参照实现 emit_event）。
pub fn encode_event(event: &str, payload: Option<&Value>) -> String {
    let arr = match payload {
        Some(p) => json!([event, p]),
        None => json!([event]),
    };
    format!("42{arr}")
}

/// 带 ack id 的事件帧（`42<id>["event",payload?]`，对齐 room_info/refresh 的线上形态）。
pub fn encode_event_with_ack(ack_id: u64, event: &str, payload: Option<&Value>) -> String {
    let arr = match payload {
        Some(p) => json!([event, p]),
        None => json!([event]),
    };
    format!("42{ack_id}{arr}")
}

pub fn ws_url(signaling_server: &str) -> String {
    format!("{}/socket.io/?transport=websocket&EIO=4", signaling_server.trim_end_matches('/'))
}

/// 一次收帧的结果。
#[derive(Debug, PartialEq)]
pub enum Received {
    Event(String, Option<Value>),
    Ack(Option<u64>, Value),
    /// binary 事件的 header（附件字节已收进 bytes）。
    BinaryEvent(String, Option<Value>, Vec<Vec<u8>>),
    Closed,
}

/// 已握手的信令连接。
pub struct Signaling {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ping_window: Duration,
}

impl Signaling {
    /// 连上并完成握手：open → '40' → 等服务器 '40{}' CONNECT_ACK。
    /// 房间 token 走 `X-NRD-AUTH` 头；连接超时对齐参照实现 ws_connect_timeout_ms（≥5 s）。
    pub async fn connect(server: &str, room_token: &str, connect_timeout_ms: Option<u64>) -> Result<Self, String> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let timeout = Duration::from_millis(connect_timeout_ms.unwrap_or(5000).max(5000));
        let mut req = ws_url(server)
            .as_str()
            .into_client_request()
            .map_err(|e| format!("bad ws request: {e}"))?;
        if !room_token.is_empty() {
            req.headers_mut().insert(
                "X-NRD-AUTH",
                room_token.parse().map_err(|_| "token 含非法字符，进不了 HTTP 头".to_string())?,
            );
        }
        let (mut ws, _) = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(req))
            .await
            .map_err(|_| format!("signaling connect timeout after {}ms", timeout.as_millis()))?
            .map_err(|e| format!("signaling connect failed: {e}"))?;

        // 等 open 帧 → 回 '40'。
        let mut ping_window = Duration::from_secs(45);
        loop {
            let msg = ws.next().await.ok_or("signaling closed before open")?.map_err(|e| e.to_string())?;
            match msg {
                Message::Text(t) => {
                    let frame = decode_frame(t.as_str());
                    if t.as_str().starts_with('0') {
                        if let Frame::Open { ping_interval_ms, ping_timeout_ms, .. } = frame {
                            ping_window = Duration::from_millis(
                                ping_interval_ms.saturating_add(ping_timeout_ms).clamp(5_000, 300_000),
                            );
                        }
                        ws.send(Message::Text("40".into())).await.map_err(|e| e.to_string())?;
                    } else if frame == Frame::Ping {
                        ws.send(Message::Text("3".into())).await.map_err(|e| e.to_string())?;
                    }
                }
                Message::Close(_) => return Err("signaling closed before open".into()),
                _ => {}
            }
            // 服务器 '40{}' = CONNECTED。
            if peek_connected(&mut ws, ping_window).await?.is_some() {
                break;
            }
            // 非 connect 帧继续循环（open 已处理）。
        }
        Ok(Self { ws, ping_window })
    }

    /// 发事件帧。
    pub async fn send(&mut self, event: &str, payload: Option<&Value>) -> Result<(), String> {
        self.ws.send(Message::Text(encode_event(event, payload).into())).await.map_err(|e| e.to_string())
    }

    /// 发带 ack id 的事件帧（room_info / refresh_reconnect_key 形态）。
    pub async fn send_with_ack(&mut self, ack_id: u64, event: &str, payload: Option<&Value>) -> Result<(), String> {
        self.ws.send(Message::Text(encode_event_with_ack(ack_id, event, payload).into())).await.map_err(|e| e.to_string())
    }

    /// 收下一帧并维持心跳：服务器 '2' 立即回 '3'；ping_window 内无任何帧判死。
    /// binary 事件会顺带收完其附件帧（socket.io v5：header 后逐帧附件）。
    pub async fn recv(&mut self) -> Result<Received, String> {
        loop {
            let msg = tokio::time::timeout(self.ping_window, self.ws.next())
                .await
                .map_err(|_| "信令心跳窗口内没有任何帧（判死）".to_string())?
                .ok_or("signaling closed")?
                .map_err(|e| e.to_string())?;
            match msg {
                Message::Text(t) => match decode_frame(t.as_str()) {
                    Frame::Event(e, p) => return Ok(Received::Event(e, p)),
                    Frame::Ack(id, v) => return Ok(Received::Ack(id, v)),
                    Frame::BinaryEvent(name, header) => {
                        let mut attachments = Vec::new();
                        // 附件帧紧随 header；'46'/结束帧到达即止。EIO4 附件是裸 binary 帧，
                        // 但服务端偶尔带旧版 0x04 前缀——带着它 protobuf 解不开，
                        // ConnectOptions（初始画质/解码能力）就全丢，客户端菜单与采集目标都对不上。
                        loop {
                            match tokio::time::timeout(Duration::from_millis(200), self.ws.next()).await {
                                Ok(Some(Ok(Message::Binary(b)))) => {
                                    let mut bytes = b.to_vec();
                                    if bytes.first() == Some(&4) && crate::wire::decode_wire(&bytes).is_err() {
                                        bytes.remove(0);
                                    }
                                    attachments.push(bytes);
                                }
                                _ => break,
                            }
                        }
                        return Ok(Received::BinaryEvent(name, header, attachments));
                    }
                    Frame::Ping => {
                        self.ws.send(Message::Text("3".into())).await.map_err(|e| e.to_string())?;
                    }
                    Frame::NamespaceDisconnect | Frame::Close | Frame::ConnectError(_) => return Ok(Received::Closed),
                    Frame::Open { ping_interval_ms, ping_timeout_ms, .. } => {
                        self.ping_window = Duration::from_millis(
                            ping_interval_ms.saturating_add(ping_timeout_ms).clamp(5_000, 300_000),
                        );
                    }
                    Frame::NamespaceConnect(_) => {}
                    Frame::Pong => {}
                    Frame::Other(_) => {}
                },
                Message::Close(_) => return Ok(Received::Closed),
                _ => {}
            }
        }
    }
}

/// open 之后等 CONNECT_ACK（'40...'）；'44' 直接报错，ping 顺带回 pong。
async fn peek_connected(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    window: Duration,
) -> Result<Option<String>, String> {
    let until = Instant::now() + window;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no CONNECT_ACK within ping window".into());
        }
        let msg = tokio::time::timeout(left, ws.next())
            .await
            .map_err(|_| "no CONNECT_ACK within ping window".to_string())?
            .ok_or("signaling closed during handshake")?
            .map_err(|e| e.to_string())?;
        match msg {
            Message::Text(t) => {
                if t.as_str().starts_with("40") {
                    return Ok(Some(t.to_string()));
                }
                if t.as_str().starts_with("44") {
                    let frame = decode_frame(t.as_str());
                    let msg = match frame {
                        Frame::ConnectError(Some(v)) => v.get("message").and_then(Value::as_str).unwrap_or("auth failed").to_string(),
                        _ => "auth failed".into(),
                    };
                    return Err(format!("namespace rejected: {msg}"));
                }
                if t == "2" {
                    ws.send(Message::Text("3".into())).await.map_err(|e| e.to_string())?;
                }
            }
            Message::Close(_) => return Err("signaling closed during handshake".into()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_frame_carries_heartbeat_parameters() {
        let frame = decode_frame(r#"0{"sid":"abc","upgrades":["polling"],"pingInterval":25000,"pingTimeout":20000}"#);
        assert_eq!(frame, Frame::Open { sid: "abc".into(), ping_interval_ms: 25000, ping_timeout_ms: 20000 });
    }

    #[test]
    fn event_frames_roundtrip_with_and_without_payload() {
        let encoded = encode_event(EVENT_SOAC, Some(&json!({"type": "offer"})));
        assert_eq!(encoded, r#"42["soac",{"type":"offer"}]"#);
        assert!(matches!(decode_frame(&encoded), Frame::Event(e, Some(_)) if e == EVENT_SOAC));
        assert_eq!(encode_event(EVENT_RELEASED, None), r#"42["released"]"#);
        assert!(matches!(decode_frame(r#"42["released"]"#), Frame::Event(e, None) if e == EVENT_RELEASED));
    }

    #[test]
    fn ack_id_frames_match_the_wire_format() {
        // 参照实现线上形态：4211["room_info"]，服务器 4311[…] 回 ack。
        assert_eq!(encode_event_with_ack(11, EVENT_ROOM_INFO, None), r#"4211["room_info"]"#);
        assert!(matches!(decode_frame(r#"4311[{"reconnect_key":"k"}]"#), Frame::Ack(Some(11), v) if v[0]["reconnect_key"] == json!("k")));
        assert!(matches!(decode_frame(r#"4212["bmsg_push",{"members":1}]"#), Frame::Event(e, Some(_)) if e == EVENT_BMSG_PUSH));
    }

    #[test]
    fn handshake_and_binary_frames_decode() {
        assert!(matches!(decode_frame("40"), Frame::NamespaceConnect(None)));
        assert_eq!(decode_frame("40{}"), Frame::NamespaceConnect(Some(json!({}))));
        assert!(matches!(decode_frame(r#"44{"message":"auth failed"}"#), Frame::ConnectError(Some(_))));
        assert_eq!(decode_frame("2"), Frame::Ping);
        assert_eq!(decode_frame("3"), Frame::Pong);
        assert_eq!(decode_frame("41"), Frame::NamespaceDisconnect);
        // be-controlled 作为 binary 事件：header 帧 + 随后附件。
        assert!(matches!(
            decode_frame(r#"45["be-controlled",{"ice_id":"i"}]"#),
            Frame::BinaryEvent(e, Some(_)) if e == EVENT_BE_CONTROLLED
        ));
        assert_eq!(decode_frame("garbage"), Frame::Other("garbage".into()));
    }

    #[test]
    fn url_is_the_eio4_dialect_with_no_token_in_query() {
        assert_eq!(
            ws_url("wss://sig-3303-e.nrd.nie.163.com/"),
            "wss://sig-3303-e.nrd.nie.163.com/socket.io/?transport=websocket&EIO=4"
        );
    }
}
