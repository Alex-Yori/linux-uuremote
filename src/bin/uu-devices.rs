//! uu-devices —— 查账号下的设备列表，并标出云端认定的在线状态。
//!
//! 用途：客户端说"看不到这台机器在线"时，这是唯一客观证据。云端用什么字段表达在线
//! （实测是 `status: CONNECTED|DISCONNECTED` 与 `publisher_availability_status`），这里照抄出来。
//! 只打结构与截断后的 id，不打 token；alias 里可能有机器名，属于本机信息，可以打。

use linux_uuremote::api::{ApiClient, Credentials};
use linux_uuremote::config;
use serde_json::Value;

/// 把长字符串截断，避免把 token/URL 整段打进日志。
fn short(value: &Value, width: usize) -> String {
    match value {
        Value::String(s) => {
            if s.len() > width {
                format!("{}…", s.chars().take(width).collect::<String>())
            } else {
                s.clone()
            }
        }
        Value::Null => "null".into(),
        other => other.to_string().chars().take(width).collect(),
    }
}

/// 一条设备记录里所有像"状态"的字段。
fn status_fields(entry: &Value) -> Vec<(String, String)> {
    let Some(map) = entry.as_object() else { return Vec::new() };
    map
        .iter()
        .filter(|(key, _)| {
            ["online", "status", "state", "available", "availability", "controllable", "running", "connect", "version", "update"]
                .iter()
                .any(|needle| key.contains(needle))
        })
        .map(|(key, value)| (key.clone(), short(value, 40)))
        .collect()
}

/// 在整棵树里找我们的 device_id（云端可能把它放在别的列表里）。
fn contains_id(value: &Value, id: &str) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| (key.contains("device_id") && value.as_str() == Some(id)) || contains_id(value, id)),
        Value::Array(list) => list.iter().any(|item| contains_id(item, id)),
        _ => false,
    }
}

fn main() {
    let raw = std::env::args().any(|a| a == "--raw");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    match rt.block_on(run(raw)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("查询失败：{e}");
            std::process::exit(1);
        }
    }
}

async fn run(raw: bool) -> Result<(), String> {
    let (file, borrowed) = config::read_with_fallback()?;
    println!("登录态：{}", if borrowed { "借用参照实现的配置" } else { "本机私有配置" });
    let mut creds = Credentials::load(file.as_ref())?;
    // --as-device：拿同一个账号 token、冒充别的 device_id 去问列表。
    // 这能回答"Windows 客户端那侧到底看不看得到我们"——它自己的 current_device 是它自己。
    if let Some(as_device) = std::env::args().find(|a| a.starts_with("--as-device=")).map(|a| a.trim_start_matches("--as-device=").to_owned()) {
        println!("冒充 device_id = {as_device}（同一账号 token）");
        creds.device_id = as_device;
    }
    let ours = creds.device_id.clone();
    println!("本机 device_id = {ours}  user_id = {}…", creds.user_id.chars().take(8).collect::<String>());
    let api = ApiClient::new(creds);
    let (status, body) = api.get("/api/v1/device/list", std::time::Duration::from_secs(12)).await?;
    println!("GET /api/v1/device/list → HTTP {status} code={:?}", body.get("code").and_then(Value::as_i64));
    if let Some(msg) = body.get("msg").and_then(Value::as_str) {
        println!("   msg = {}", short(&Value::String(msg.to_owned()), 80));
    }
    let data = body.get("data").cloned().unwrap_or(Value::Null);
    if raw {
        println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default().chars().take(6000).collect::<String>());
    }
    // 云端把设备放在哪个键下：全部列出，别只认一个。
    if let Some(map) = data.as_object() {
        for (key, value) in map {
            match value {
                Value::Array(list) => println!("   {key}: {} 条", list.len()),
                other => println!("   {key} = {}", short(other, 40)),
            }
        }
    }
    let mut entries = Vec::new();
    for key in ["my_binded_devices", "others_shared_devices", "devices", "shared_devices", "history_devices"] {
        if let Some(list) = data.get(key).and_then(Value::as_array) {
            entries.extend(list.iter().cloned());
        }
    }
    for entry in &entries {
        let id = entry.get("device_id").and_then(Value::as_str).unwrap_or("?");
        let name = entry.get("alias").and_then(Value::as_str).or_else(|| entry.get("name").and_then(Value::as_str)).unwrap_or("(无名)");
        let mark = if id == ours { " ← 本机" } else { "" };
        println!("{name}  id={id}  platform={:?}{mark}", entry.get("platform").and_then(Value::as_i64));
        for (key, value) in status_fields(entry) {
            println!("   {key} = {value}");
        }
    }
    if !contains_id(&data, &ours) {
        println!("!! 云端这份列表里没有本机 device_id {ours}：Windows/手机客户端根本不会显示这台机器。");
        println!("!! 修法：重新注册并绑定（跑 uu-login 走扫码，让它带着 device_id 完成绑定）。");
    }
    println!("（共 {} 条设备记录）", entries.len());
    Ok(())
}
