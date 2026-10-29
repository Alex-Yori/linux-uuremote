//! UU 官方 REST 客户端：请求签名、设备注册、扫码登录、建房取信令凭证。
//!
//! 协议事实来自参照实现（`uuremote-server` 0.2.21/0.2.35，本机在跑、云端连接可用）与
//! 静态逆向（官方 Mac 客户端 Codable 模型、libstreamer 日志串），不是猜的：
//! - `BASE_URL = https://api.nrd.nie.163.com`（产品 API 家族）
//! - 每个请求带一组 `x-param-*` 头；`x-param-sign` =
//!   `hex(HMAC-SHA256(sign_key, METHOD + path + sorted("k=v" joined by '&') + body))`，
//!   只把 `x-param-*`（排除 sign 本身）纳入签名；未登录时不存在的 id 不发送也不签名。
//! - 扫码：`POST /api/v1/qrcode/gen/login`（空 body）→ `qrcode_jump_url` + `status_query_ticket`；
//!   `POST /api/v1/qrcode/login/status` 用 ticket 长轮询（60 s 请求超时，status 1..4）；
//!   `POST /api/v1/login/by_qrcode` → `token`（JWT 内含 `csrf` 声明）+ `user_id`。
//!   过期/异常（HTTP 400 code 1190）按官方策略停下等用户刷新，不自动重生成。
//! - 设备注册 `POST /api/v1/device/windows/init`（匿名、硬件摘要）→ `device_id`。
//! - 建房 `POST /api/v1/room/create` → `signaling_server` + 房间 `token`（JWT `exp`）。
//!
//! token 与 ticket 是机密：永不打印、永不进日志。

use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// 官方 PC 客户端内置的请求签名 key（`x-param-plat=1`），所有用户相同；
/// 配置 `sign_key` / `UU_SIGN_KEY` 可覆盖（以防网易换 key）。
pub const DEFAULT_SIGN_KEY: &str = "alWiSzXZTLu3WfFnw13uBru3";
pub const BASE_URL: &str = "https://api.nrd.nie.163.com";
/// 登录 token 无效或过期的 API code（"Token无效或已过期"，HTTP 422）。
pub const TOKEN_INVALID: i64 = 1120;
/// 二维码失效的 API code。
pub const QR_EXPIRED: i64 = 1190;

/// 账号绑定与签名 key。永不提交：从私有配置文件或 `UU_*` 环境变量加载。
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Credentials {
    pub user_id: String,
    pub device_id: String,
    pub client_id: String,
    pub system_id: String,
    pub token: String,
    pub csrf: String,
    /// x-param-sign 的 HMAC-SHA256 key。
    pub sign_key: String,
}

/// 机密结构：Debug 不泄露内容。
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials { .. }")
    }
}

impl Credentials {
    /// `UU_*` 环境变量覆盖文件值（UU_USER_ID / UU_DEVICE_ID / UU_CLIENT_ID /
    /// UU_SYSTEM_ID / UU_TOKEN / UU_CSRF / UU_SIGN_KEY）。
    pub fn load(file: Option<&Value>) -> Result<Self, String> {
        let get = |key: &str| -> String {
            std::env::var(format!("UU_{}", key.to_uppercase()))
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| file.and_then(|f| f.get(key)).and_then(|v| v.as_str()).map(str::to_owned))
                .unwrap_or_default()
        };
        let creds = Self {
            user_id: get("user_id"),
            device_id: get("device_id"),
            client_id: get("client_id"),
            system_id: get("system_id"),
            token: get("token"),
            csrf: get("csrf"),
            sign_key: Some(get("sign_key"))
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| DEFAULT_SIGN_KEY.into()),
        };
        let missing: Vec<&str> = [("user_id", &creds.user_id), ("device_id", &creds.device_id), ("client_id", &creds.client_id), ("system_id", &creds.system_id), ("token", &creds.token), ("csrf", &creds.csrf)]
            .iter()
            .filter(|(_, v)| v.is_empty())
            .map(|(k, _)| *k)
            .collect();
        if missing.is_empty() {
            Ok(creds)
        } else {
            Err(format!("missing credential settings: {}", missing.join(", ")))
        }
    }

    /// 写回配置文件用：只写非空字段，保留文件里的其他键由调用方合并。
    pub fn to_json(&self) -> Value {
        let mut v = json!({});
        for (k, val) in [("user_id", &self.user_id), ("device_id", &self.device_id), ("client_id", &self.client_id), ("system_id", &self.system_id), ("token", &self.token), ("csrf", &self.csrf)] {
            if !val.is_empty() {
                v[k] = json!(val);
            }
        }
        if !self.sign_key.is_empty() && self.sign_key != DEFAULT_SIGN_KEY {
            v["sign_key"] = json!(self.sign_key);
        }
        v
    }
}

/// 建房返回的信令凭证。Debug 不泄露 token。
#[derive(Clone, Deserialize)]
pub struct RoomData {
    pub signaling_server: String,
    pub token: String,
    pub signaling_list: Option<Vec<String>>,
    pub ws_connect_timeout_ms: Option<u64>,
}

impl std::fmt::Debug for RoomData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RoomData { .. }")
    }
}

#[derive(Debug)]
pub enum RoomError {
    LoginExpired,
    Other(String),
}
impl std::fmt::Display for RoomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoomError::LoginExpired => f.write_str("login token invalid or expired"),
            RoomError::Other(e) => f.write_str(e),
        }
    }
}

/// JWT payload 里的 csrf 声明。
pub fn csrf_from_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("csrf")?.as_str().map(str::to_owned)
}

/// 房间 token 的剩余寿命（秒）；只报寿命，绝不返回 token 内容。
pub fn room_token_lifetime(token: &str, now: u64) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    if payload.len() > 16384 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let remaining = claims.get("exp")?.as_i64()?.saturating_sub(i64::try_from(now).ok()?);
    (remaining.unsigned_abs() <= 7 * 24 * 3600).then_some(remaining)
}

/// 已签名 REST 客户端（reqwest + rustls）。
pub struct ApiClient {
    pub creds: Credentials,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(creds: Credentials) -> Self {
        Self {
            creds,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(12))
                .build()
                .expect("reqwest client"),
        }
    }

    /// 网易 NRD 请求签名：`x-param-*`（排除 sign）按 key 排序、`k=v` 用 `&` 连接，
    /// 串成 `METHOD + path + params + body` 做 HMAC-SHA256，hex 输出。
    pub fn sign_request(&self, method: &str, path: &str, params: &BTreeMap<String, String>, body: &str) -> String {
        let params_str = params
            .iter()
            .filter(|(k, _)| k.starts_with("x-param-") && *k != "x-param-sign")
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let string_to_sign = format!("{}{}{}{}", method.to_uppercase(), path, params_str, body);
        let mut mac = HmacSha256::new_from_slice(self.creds.sign_key.as_bytes()).expect("HMAC accepts any key length");
        mac.update(string_to_sign.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn build_headers(&self, method: &str, path: &str, body: &str) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs().to_string();
        let mut params: BTreeMap<String, String> = BTreeMap::new();
        params.insert("x-param-plat".into(), "1".into());
        params.insert("x-param-vn".into(), "4.42.0.2770".into());
        params.insert("x-param-vc".into(), "2770".into());
        params.insert("x-param-pkgn".into(), "com.netease.gameviewer".into());
        params.insert("x-param-chn".into(), "gwqd".into());
        params.insert("x-param-lang".into(), "zh-CN".into());
        params.insert("x-param-cnt".into(), "CN".into());
        // 登录前有些标识还不存在：不发也不签。
        for (key, value) in [("x-param-client-id", &self.creds.client_id), ("x-param-device-id", &self.creds.device_id), ("x-param-system-id", &self.creds.system_id), ("x-param-user-id", &self.creds.user_id)] {
            if !value.is_empty() {
                params.insert(key.into(), value.clone());
            }
        }
        params.insert("x-param-ts".into(), ts);

        let sign = self.sign_request(method, path, &params, body);
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        headers.insert("User-Agent", HeaderValue::from_static("UURemote/4.42.0 (Windows NT 10.0; Win64; x64)"));
        if !self.creds.token.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", self.creds.token)) {
                headers.insert("Authorization", v);
            }
            if let Ok(v) = HeaderValue::from_str(&format!("nrd_access_token={}; nrd_csrf_access_token={}", self.creds.token, self.creds.csrf)) {
                headers.insert("Cookie", v);
            }
            if let Ok(v) = HeaderValue::from_str(&self.creds.csrf) {
                headers.insert("X-CSRF-TOKEN", v);
            }
        }
        for (k, v) in params {
            if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
                headers.insert(name, val);
            }
        }
        headers.insert("X-Param-SIGN", HeaderValue::from_str(&sign).unwrap());
        headers
    }

    /// 签名 JSON POST（可选空 body），带显式超时；返回 (HTTP status, 解析后的 body)。
    pub async fn post(&self, path: &str, body: Option<&Value>, timeout: Duration) -> Result<(u16, Value), String> {
        let body_str = match body { Some(b) => serde_json::to_string(b).unwrap_or_default(), None => String::new() };
        let resp = self.http
            .post(format!("{BASE_URL}{path}"))
            .timeout(timeout)
            .headers(self.build_headers("POST", path, &body_str))
            .body(body_str)
            .send()
            .await
            .map_err(|e| format!("POST {path}: {e}"))?;
        let status = resp.status().as_u16();
        let value = resp.json().await.unwrap_or(Value::Null);
        Ok((status, value))
    }

    pub async fn post_json(&self, path: &str, body: &Value) -> Result<(u16, Value), String> {
        self.post(path, Some(body), Duration::from_secs(12)).await
    }

    /// 签名 GET：设备列表这类只读接口（参照实现的 viewer 用 GET，POST 会 405）。
    pub async fn get(&self, path: &str, timeout: Duration) -> Result<(u16, Value), String> {
        let resp = self.http
            .get(format!("{BASE_URL}{path}"))
            .timeout(timeout)
            .headers(self.build_headers("GET", path, ""))
            .send()
            .await
            .map_err(|e| format!("GET {path}: {e}"))?;
        let status = resp.status().as_u16();
        let value = resp.json().await.unwrap_or(Value::Null);
        Ok((status, value))
    }

    /// `GET /api/v1/device/list`：账号下绑定过的设备原样返回（含在线状态字段）。
    pub async fn device_list(&self) -> Result<Vec<Value>, String> {
        let (status, body) = self.get("/api/v1/device/list", Duration::from_secs(12)).await?;
        let data = api_data("device list", status, &body)?;
        let mut out = Vec::new();
        // 参照实现把设备放在 data.my_binded_devices / data.others_shared_devices。
        for key in ["my_binded_devices", "others_shared_devices"] {
            if let Some(list) = data.get(key).and_then(Value::as_array) {
                out.extend(list.iter().cloned());
            }
        }
        if out.is_empty() {
            if let Some(list) = data.get("devices").and_then(Value::as_array) {
                out.extend(list.iter().cloned());
            }
        }
        Ok(out)
    }

    /// `POST /api/v1/room/create`：取信令服务器与房间 token。登录过期单独成类。
    pub async fn create_room(&self) -> Result<RoomData, RoomError> {
        let (status, body) = self
            .post_json("/api/v1/room/create", &json!({}))
            .await
            .map_err(RoomError::Other)?;
        let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == TOKEN_INVALID {
            return Err(RoomError::LoginExpired);
        }
        if status == 200 && code == 0 {
            if let Some(data) = body.get("data") {
                let room: RoomData = serde_json::from_value(data.clone()).map_err(|e| RoomError::Other(format!("room data decode failed: {e}")))?;
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                return Ok(room_with_lifetime_log(room, now));
            }
        }
        Err(RoomError::Other(format!(
            "create_room http {status} code {code}: {}",
            body.get("msg").and_then(Value::as_str).unwrap_or("")
        )))
    }
}

/// 建房结果只打寿命，不打 token。
fn room_with_lifetime_log(room: RoomData, now: u64) -> RoomData {
    println!("room_token_remaining_s={:?}", room_token_lifetime(&room.token, now));
    room
}

/// 统一的成功判定：HTTP 200 且 code==0 → `data`；错误消息不回显服务器内容（可能含凭证）。
pub fn api_data(path: &str, status: u16, body: &Value) -> Result<Value, String> {
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if status == 200 && code == 0 {
        return Ok(body.get("data").cloned().unwrap_or(Value::Null));
    }
    Err(format!("{path}: http {status} code {code}"))
}

// ---------------- 设备身份（硬件摘要） ----------------

fn read_file(p: &str) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// /dev/urandom 的 v4 UUID。
pub fn random_uuid() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// /etc/machine-id 转 GUID——Windows MachineGuid 的最近 Linux 等价物。
///
/// `UU_MACHINE_GUID` 覆盖：同一台机器想把本项目注册成**独立设备**时必须有别的 GUID，
/// 否则云端按 GUID 去重，会和参照实现（uuremote-server）认成同一台，
/// 客户端列表里就只看到一个在线设备。
pub fn machine_guid() -> String {
    if let Ok(guid) = std::env::var("UU_MACHINE_GUID") {
        if !guid.is_empty() {
            return guid;
        }
    }
    let id = read_file("/etc/machine-id");
    if id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()) {
        format!("{}-{}-{}-{}-{}", &id[..8], &id[8..12], &id[12..16], &id[16..20], &id[20..])
    } else {
        random_uuid()
    }
}

/// 主板标识：云端 device/windows/init 把它列为必填（少了直接 422 "Field required"）。
pub fn base_board() -> String {
    let v = [read_file("/sys/class/dmi/id/board_vendor"), read_file("/sys/class/dmi/id/board_name")].join(" ");
    if v.trim().is_empty() {
        "Unknown".to_owned()
    } else {
        v.trim().to_owned()
    }
}

pub fn os_name() -> String {
    read_file("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".into())
}

pub fn cpu_name() -> String {
    read_file("/proc/cpuinfo")
        .lines()
        .find_map(|l| l.strip_prefix("model name").and_then(|r| r.split_once(':')))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| std::env::consts::ARCH.into())
}

/// lspci 的显卡行；无 lspci 时退回 /sys/class/drm 的 PCI id。
pub fn video() -> Vec<String> {
    if let Ok(out) = std::process::Command::new("lspci").output() {
        let list: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("VGA compatible controller") || l.contains("3D controller") || l.contains("Display controller"))
            .filter_map(|l| l.splitn(2, ": ").nth(1).map(str::to_owned))
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    let mut list = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/sys/class/drm") {
        for e in dir.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("card") && !n.contains('-') {
                let dev = e.path().join("device");
                list.push(format!(
                    "PCI {}:{}",
                    read_file(&dev.join("vendor").to_string_lossy()).trim_start_matches("0x"),
                    read_file(&dev.join("device").to_string_lossy()).trim_start_matches("0x")
                ));
            }
        }
    }
    if list.is_empty() {
        list.push("Unknown".into());
    }
    list
}

/// 第一个物理网卡的 MAC，大写连字符形式（对齐 Windows）。
pub fn mac() -> String {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().join("device").exists())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .iter()
        .map(|n| read_file(&format!("/sys/class/net/{n}/address")))
        .find(|m| !m.is_empty() && m != "00:00:00:00:00:00")
        .map(|m| m.to_uppercase().replace(':', "-"))
        .unwrap_or_default()
}

pub fn memory_mb() -> u64 {
    read_file("/proc/meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

pub fn hostname() -> String {
    let h = read_file("/proc/sys/kernel/hostname");
    if h.is_empty() {
        "Linux".into()
    } else {
        h
    }
}

/// 设备注册（匿名）：硬件摘要 → device_id。屏尺寸无显示时按参照实现退回 1920x1080。
pub async fn register_device(client: &ApiClient, screen: &str) -> Result<String, String> {
    register_device_with(client, screen, None, None, None).await
}

/// 同一账号下的"第二台机器"注册（控制端身份用）：client_id/system_id/machine_guid 可换，
/// 否则云端按 machine_guid 去重会退回本机已有的 device_id。
pub async fn register_device_with(client: &ApiClient, screen: &str, client_id: Option<&str>, system_id: Option<&str>, guid: Option<&str>) -> Result<String, String> {
    let body = json!({
        "name": hostname(),
        "client_id": client_id.unwrap_or(&client.creds.client_id),
        "system_id": system_id.unwrap_or(&client.creds.system_id),
        "machine_guid": guid.unwrap_or(&machine_guid()),
        "os": os_name(),
        "base_board": base_board(),
        "cpu": cpu_name(),
        "video": video(),
        "mac": mac(),
        "memory": memory_mb(),
        "screen": screen,
    });
    let (status, resp) = client.post_json("/api/v1/device/windows/init", &body).await?;
    let data = api_data("device init", status, &resp).map_err(|e| format!("{e} · 响应 {}", resp))?;
    data.get("device_id").and_then(Value::as_str).map(str::to_owned).ok_or("device init returned no device_id".into())
}

// ---------------- 扫码登录（纯决策部分可单测） ----------------

/// 轮询动作：等待 / 已扫 / 已确认 / 刷新（带最小间隔）/ 失败。
#[derive(Debug, PartialEq, Eq)]
pub enum QrPollAction {
    Wait(i64),
    Scanned,
    Confirmed,
    Refresh(Duration),
    Failed(&'static str),
}

/// 官方策略：status 0（过期）350 ms 后重生成、3 立刻重生成、4 完成登录；
/// 查询失败什么都不说明——不代表二维码过期。
pub fn qr_poll_action(last_status: i64, status: u16, body: &Value) -> QrPollAction {
    if body.get("code").and_then(Value::as_i64) == Some(QR_EXPIRED) {
        return QrPollAction::Failed("二维码已失效");
    }
    let Ok(data) = api_data("qrcode status", status, body) else {
        return QrPollAction::Failed("二维码状态查询失败");
    };
    match data.get("status").and_then(Value::as_i64) {
        Some(1) => QrPollAction::Wait(1),
        Some(2) if last_status != 2 => QrPollAction::Scanned,
        Some(2) => QrPollAction::Wait(2),
        Some(0) => QrPollAction::Refresh(Duration::from_millis(350)),
        Some(3) => QrPollAction::Refresh(Duration::ZERO),
        Some(4) => QrPollAction::Confirmed,
        _ => QrPollAction::Failed("二维码状态响应异常"),
    }
}

/// gen/login 的状态票：现行云与官方客户端的 `status_query_ticket`，
/// 旧服务器退回 `token` 字段。
pub fn qr_ticket(gen: &Value) -> Option<&str> {
    gen.get("status_query_ticket")
        .or_else(|| gen.get("token"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(key: &str) -> ApiClient {
        ApiClient::new(Credentials {
            client_id: "cid".into(),
            device_id: "did".into(),
            system_id: "sid".into(),
            user_id: "uid".into(),
            token: "tok".into(),
            csrf: "csrf".into(),
            sign_key: key.into(),
        })
    }

    #[test]
    fn signature_sorts_params_and_includes_method_path_body() {
        // 独立复核：RFC 4231 之外的自建向量，用同一算法在测试里重算一遍，
        // 保证排序、排除 sign、拼接格式稳定。
        let c = client("key-1");
        let mut p = BTreeMap::new();
        p.insert("x-param-ts".to_string(), "1700000000".to_string());
        p.insert("x-param-plat".to_string(), "1".to_string());
        p.insert("x-param-sign".to_string(), "ignored".to_string());
        p.insert("accept".to_string(), "not-signed".to_string());
        let signed = c.sign_request("post", "/api/v1/room/create", &p, "{}");
        // 期望串 = "POST" + path + "x-param-plat=1&x-param-ts=1700000000" + body
        let mut mac = HmacSha256::new_from_slice(b"key-1").unwrap();
        mac.update(b"POST/api/v1/room/createx-param-plat=1&x-param-ts=1700000000{}");
        assert_eq!(signed, hex::encode(mac.finalize().into_bytes()));
    }

    #[test]
    fn headers_carry_identity_csrf_and_signature() {
        let c = client("k");
        let h = c.build_headers("POST", "/api/v1/room/create", "{}");
        assert_eq!(h["x-param-plat"], "1");
        assert_eq!(h["x-param-client-id"], "cid");
        assert_eq!(h["authorization"], "Bearer tok");
        assert_eq!(h["x-csrf-token"], "csrf");
        assert!(h["cookie"].to_str().unwrap().contains("nrd_csrf_access_token=csrf"));
        assert_eq!(h["x-param-sign"].len(), 64);
    }

    #[test]
    fn unlogged_identity_headers_are_omitted_not_signed() {
        let mut creds = Credentials { sign_key: "k".into(), ..Default::default() };
        creds.token = "t".into();
        creds.csrf = "c".into();
        let c = ApiClient::new(creds);
        let h = c.build_headers("POST", "/api/v1/qrcode/gen/login", "");
        assert!(h.get("x-param-user-id").is_none());
        assert!(h.get("authorization").is_some()); // token 有就带
    }

    #[test]
    fn csrf_is_read_from_jwt_payload() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"u","csrf":"c-1"}"#);
        assert_eq!(csrf_from_token(&format!("h.{payload}.s")), Some("c-1".into()));
        assert_eq!(csrf_from_token("bad"), None);
    }

    #[test]
    fn room_token_lifetime_reports_only_remaining() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({"exp": 1000}).to_string().as_bytes());
        assert_eq!(room_token_lifetime(&format!("h.{payload}.s"), 900), Some(100));
        assert_eq!(room_token_lifetime(&format!("h.{payload}.s"), 2000), Some(-1000));
        assert_eq!(room_token_lifetime("opaque-token", 900), None);
    }

    #[test]
    fn qr_poll_keeps_waiting_scanned_and_confirmed_distinct() {
        let reply = |status| json!({"code": 0, "data": {"status": status}});
        assert_eq!(qr_poll_action(1, 200, &reply(1)), QrPollAction::Wait(1));
        assert_eq!(qr_poll_action(1, 200, &reply(2)), QrPollAction::Scanned);
        assert_eq!(qr_poll_action(2, 200, &reply(2)), QrPollAction::Wait(2));
        assert_eq!(qr_poll_action(2, 200, &reply(4)), QrPollAction::Confirmed);
        assert_eq!(qr_poll_action(1, 200, &reply(0)), QrPollAction::Refresh(Duration::from_millis(350)));
        assert_eq!(qr_poll_action(2, 200, &reply(3)), QrPollAction::Refresh(Duration::ZERO));
        assert_eq!(qr_poll_action(2, 400, &json!({"code": 1190})), QrPollAction::Failed("二维码已失效"));
    }

    #[test]
    fn qr_poll_failures_do_not_imply_expiry_or_confirmation() {
        for last in [1, 2] {
            for body in [json!(null), json!({"code": 1}), json!({"code": 0, "data": {"status": 4}})] {
                assert_eq!(qr_poll_action(last, 503, &body), QrPollAction::Failed("二维码状态查询失败"));
            }
            for data in [json!(null), json!({}), json!({"status": "4"}), json!({"status": 99})] {
                assert_eq!(qr_poll_action(last, 200, &json!({"code": 0, "data": data})), QrPollAction::Failed("二维码状态响应异常"));
            }
        }
    }

    #[test]
    fn status_ticket_prefers_status_query_ticket() {
        assert_eq!(qr_ticket(&json!({"token": "old", "status_query_ticket": "new"})), Some("new"));
        assert_eq!(qr_ticket(&json!({"token": "old"})), Some("old"));
        assert_eq!(qr_ticket(&json!({"status_query_ticket": ""})), None);
    }

    #[test]
    fn api_errors_never_echo_server_content() {
        let body = json!({"code": 1001, "msg": "fixture-token", "data": [{"loc": "fixture"}]});
        let err = api_data("x", 400, &body).unwrap_err();
        assert_eq!(err, "x: http 400 code 1001");
        assert!(!err.contains("fixture-token"));
    }

    #[test]
    fn uuids_and_guids_are_shaped() {
        let u = random_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4');
        let g = machine_guid();
        assert_eq!(g.len(), 36);
        assert!(g.contains('-'));
    }
}
