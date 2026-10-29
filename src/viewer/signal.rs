//! 控制端信令：Engine.IO4 + Socket.IO，房间 token 走 X-NRD-AUTH 头。
//! 与参照实现同一套握手：0=open → 40=connect → 40{sid}；事件 42、应答 43、
//! 二进制事件 45N/46N（附件帧替换 _placeholder），ping 2 → pong 3。
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use std::{collections::VecDeque, time::Duration};

pub struct Signal {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    next_ack: u64,
    pending: VecDeque<Value>,
}

impl Signal {
    pub async fn connect(urls: &[String], token: &str, timeout: Duration) -> Result<Self, String> {
        for url in urls {
            if let Ok(Ok(signal)) = tokio::time::timeout(timeout, Self::one(url, token)).await { return Ok(signal); }
        }
        Err("信令连接失败（所有信令服务器都没连上）".into())
    }

    async fn one(url: &str, token: &str) -> Result<Self, String> {
        let mut request = format!("{}/socket.io/?transport=websocket&EIO=4", url.trim_end_matches('/'))
            .into_client_request()
            .map_err(|_| "信令地址不合法")?;
        request.headers_mut().insert("X-NRD-AUTH", token.parse().map_err(|_| "房间 token 不合法")?);
        let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(request))
            .await
            .map_err(|_| "信令连接超时")?
            .map_err(|_| "信令连接失败")?;
        let mut s = Self { ws, next_ack: 10, pending: VecDeque::new() };
        let mut opened = false;
        loop {
            match s.ws.next().await {
                Some(Ok(Message::Text(t))) if t.starts_with('0') && !opened => {
                    serde_json::from_str::<Value>(&t[1..]).map_err(|_| "Engine.IO open 包不合法")?;
                    opened = true;
                    s.send(Message::Text("40".into())).await?;
                }
                Some(Ok(Message::Text(t))) if t.starts_with("40") && opened => return Ok(s),
                Some(Ok(Message::Text(t))) if t.starts_with('2') => s.send(Message::Text(format!("3{}", &t[1..]))).await?,
                Some(Ok(Message::Ping(b))) => s.send(Message::Pong(b)).await?,
                _ => return Err("信令握手失败".into()),
            }
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), String> {
        self.ws.send(message).await.map_err(|_| "信令发送失败".to_string())
    }

    /// control 事件：头 JSON + protobuf 附件（ConnectOptions）。等 "success" 应答拿 ICE 配置。
    pub async fn control(&mut self, header: Value, app_data: Vec<u8>, timeout: Duration) -> Result<Value, String> {
        self.next_ack += 1;
        let id = self.next_ack;
        self.send(Message::Text(format!("451-{id}{}", serde_json::json!(["control", header])))).await?;
        self.send(Message::Binary(app_data)).await?;
        tokio::time::timeout(timeout, async {
            loop {
                match self.packet().await? {
                    Packet::Ack(ack, value) if ack == id => {
                        let a = value.as_array().ok_or("control 应答格式不对")?;
                        if a.len() != 2 || a[0].as_str() != Some("success") || !a[1].is_object() {
                            return Err("control 被拒绝（多半是被控端正被别人控制）".into());
                        }
                        return Ok(a[1].clone());
                    }
                    Packet::Event(value) => {
                        if self.pending.len() >= 64 { return Err("control 之前堆了太多事件".into()); }
                        self.pending.push_back(value);
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| "control 应答超时")?
    }

    async fn packet(&mut self) -> Result<Packet, String> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(t))) if t.starts_with('2') => self.send(Message::Text(format!("3{}", &t[1..]))).await?,
                Some(Ok(Message::Ping(b))) => self.send(Message::Pong(b)).await?,
                Some(Ok(Message::Text(t))) if t.starts_with("43") => {
                    let begin = t.find('[').ok_or("应答帧不合法")?;
                    let id = t[2..begin].parse().map_err(|_| "应答 id 不合法")?;
                    let value = serde_json::from_str(&t[begin..]).map_err(|_| "应答内容不合法")?;
                    return Ok(Packet::Ack(id, value));
                }
                Some(Ok(Message::Text(t))) if t.starts_with("42") => {
                    let begin = t.find('[').ok_or("事件帧不合法")?;
                    let value = serde_json::from_str(&t[begin..]).map_err(|_| "事件内容不合法")?;
                    return Ok(Packet::Event(value));
                }
                // 二进制事件（45N-[...]/46N-[...]）后跟 N 个附件帧；附件替换进 _placeholder。
                // 被控端的 answer 可能带 gzip_sdp 附件——base64 填回后照样走解压分支。
                Some(Ok(Message::Text(t))) if t.starts_with("45") || t.starts_with("46") => {
                    let dash = t.find('-').ok_or("二进制帧头不合法")?;
                    let count: usize = t[2..dash].parse().map_err(|_| "附件数量不合法")?;
                    if count > 8 { return Err("附件太多".into()); }
                    let begin = t.find('[').ok_or("二进制帧体不合法")?;
                    let mut value: Value = serde_json::from_str(&t[begin..]).map_err(|_| "二进制帧 JSON 不合法")?;
                    let mut attachments = Vec::with_capacity(count);
                    while attachments.len() < count {
                        match self.ws.next().await {
                            Some(Ok(Message::Binary(mut b))) => {
                                if b.len() > 2 && b[0] == 4 && b[1] == 0x1f { b.remove(0); } // 旧版 0x04 前缀
                                if b.len() > 4 * 1024 * 1024 { return Err("附件过大".into()); }
                                attachments.push(base64::engine::general_purpose::STANDARD.encode(&b));
                            }
                            Some(Ok(Message::Text(t))) if t.starts_with('2') => self.send(Message::Text(format!("3{}", &t[1..]))).await?,
                            Some(Ok(_)) => {}
                            _ => return Err("信令断开".into()),
                        }
                    }
                    fn fill(v: &mut Value, a: &[String]) {
                        match v {
                            Value::Object(o) if o.get("_placeholder") == Some(&Value::Bool(true)) => {
                                if let Some(s) = o.get("num").and_then(Value::as_u64).and_then(|n| a.get(n as usize)) { *v = Value::String(s.clone()); }
                            }
                            Value::Object(o) => for x in o.values_mut() { fill(x, a) },
                            Value::Array(l) => for x in l { fill(x, a) },
                            _ => {}
                        }
                    }
                    fill(&mut value, &attachments);
                    if t.starts_with("46") {
                        let id = t[dash + 1..begin].parse().map_err(|_| "二进制应答 id 不合法")?;
                        return Ok(Packet::Ack(id, value));
                    }
                    return Ok(Packet::Event(value));
                }
                Some(Ok(Message::Text(t))) if t == "41" || t == "1" => return Err("房间已关闭（对方断开了）".into()),
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Err("信令断开".into()),
                Some(Ok(Message::Text(t))) => eprintln!("   信令杂帧：前缀={} {}字节", t.chars().take(3).collect::<String>(), t.len()),
                Some(Ok(Message::Binary(b))) => eprintln!("   信令杂帧：二进制 {}字节", b.len()),
                _ => {}
            }
        }
    }

    pub async fn event(&mut self) -> Result<Value, String> {
        if let Some(v) = self.pending.pop_front() { return Ok(v); }
        loop {
            if let Packet::Event(v) = self.packet().await? { return Ok(v); }
        }
    }

    pub async fn soac(&mut self, value: Value) -> Result<(), String> {
        self.send(Message::Text(format!("42{}", serde_json::json!(["soac", value])))).await
    }
    pub async fn close(&mut self) { let _ = self.ws.close(None).await; }
}

enum Packet { Ack(u64, Value), Event(Value) }
