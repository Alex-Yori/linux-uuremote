//! uu-login —— 扫码登录 UU 账号，把凭证写进私有配置文件（0600）。
//!
//! 流程（与参照实现的官方链路一致）：
//! 1. 没有 client_id/system_id 就生成并保存（一次注册终身复用）。
//! 2. 没有 device_id 就 `POST /api/v1/device/windows/init`（匿名、硬件摘要）。
//! 3. 没有 token 就扫码：`/api/v1/qrcode/gen/login` → 终端画二维码（PNG 存工作区）→
//!    `/api/v1/qrcode/login/status` 用 status_query_ticket 长轮询 → 确认后
//!    `/api/v1/login/by_qrcode` 换账号凭证（token JWT 内含 csrf）。
//!
//! 二维码过期按官方策略停下提示刷新（--force 重新生成）；票据与 token 永不打印。

use base64::Engine as _;
use linux_uuremote::api::{self, api_data, ApiClient, Credentials, QrPollAction};
use linux_uuremote::config;
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let force = std::env::args().any(|a| a == "--force");
    let new_device = std::env::args().any(|a| a == "--new-device");
    if let Err(e) = run(force, new_device).await {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
}

async fn run(force: bool, new_device: bool) -> Result<(), String> {
    let (file, borrowed) = config::read_with_fallback()?;
    if borrowed {
        println!("借用参照实现的登录态（{}）；写入只进 {}。", config::fallback_path().display(), config::path().display());
    }
    let mut creds = Credentials::load(file.as_ref()).unwrap_or_default();
    // --new-device：账号凭证照用，身份全部换新，注册成第二台设备。
    // 不这么做的话云端按 machine_guid 去重，本项目和 uuremote-server 会认成同一台，
    // Windows 端就永远只看到参照实现那个设备。
    let mut guid = std::env::var("UU_MACHINE_GUID").unwrap_or_default();
    if new_device {
        creds.device_id = String::new();
        creds.client_id = api::random_uuid();
        creds.system_id = api::random_uuid().to_ascii_uppercase();
        guid = api::random_uuid();
        std::env::set_var("UU_MACHINE_GUID", guid.clone());
        println!("新设备身份：client_id/system_id/machine_guid 全部重新生成。");
    }
    if creds.client_id.is_empty() {
        creds.client_id = api::random_uuid();
    }
    if creds.system_id.is_empty() {
        creds.system_id = api::random_uuid();
    }

    let mut client = ApiClient::new(creds.clone());
    if creds.device_id.is_empty() {
        let screen = screen_summary();
        let device_id = api::register_device(&client, &screen).await?;
        creds.device_id = device_id;
        client = ApiClient::new(creds.clone());
        println!("device_id 注册完成（长度 {}）。", creds.device_id.len());
    }

    if creds.token.is_empty() || force {
        login_by_qr(&client).await?;
        // by_qrcode 的响应直接带账号字段；重新加载一次拿到 user_id/token。
        creds = Credentials::load(last_qr_result().as_ref()).unwrap_or(creds);
    }
    if creds.token.is_empty() {
        return Err("尚未登录：运行不带 --force 的 uu-login 扫码。".into());
    }
    if creds.csrf.is_empty() {
        creds.csrf = api::csrf_from_token(&creds.token).unwrap_or_default();
        if creds.csrf.is_empty() {
            return Err("token 里没有 csrf 声明：可能不是账号 token。".into());
        }
    }

    let mut merged = merge_into_config(file.as_ref(), &creds.to_json());
    if !guid.is_empty() {
        merged["machine_guid"] = json!(guid);
    }
    let p = config::write(&merged)?;
    println!("凭证已保存到 {}（0600）：user_id/device_id/client_id/system_id/token/csrf 齐备。", p.display());
    Ok(())
}

/// 扫码登录主循环；成功后把 by_qrcode 的 data 存进程内供上层取。
async fn login_by_qr(client: &ApiClient) -> Result<(), String> {
    let (status, body) = client.post("/api/v1/qrcode/gen/login", None, Duration::from_secs(12)).await?;
    let gen = api_data("gen/login", status, &body)?;
    let url = gen.get("qrcode_jump_url").and_then(Value::as_str).ok_or("gen/login 没有 qrcode_jump_url")?;
    let ticket = api::qr_ticket(&gen).ok_or("gen/login 没有状态票（status_query_ticket/token 都没有）")?;

    // 终端画二维码（半块字符，手机可直接扫）；链接同时给出。
    let code = qrcode::QrCode::new(url.as_bytes()).map_err(|e| format!("二维码生成失败：{e}"))?;
    println!("{}", code.render::<qrcode::render::unicode::Dense1x2>().min_dimensions(24, 24).build());
    println!("请用 UU Remote App 扫上面的码，或打开链接：");
    println!("{url}");

    // 长轮询：每次查询 60 s 超时（官方 qrcodeStatusRequest 语义）。
    let mut last_status = 1i64;
    loop {
        let (status, body) = client
            .post(
                "/api/v1/qrcode/login/status",
                Some(&json!({"token": ticket, "qrcode_jump_url": url, "login_status": last_status})),
                Duration::from_secs(60),
            )
            .await
            .unwrap_or((0, Value::Null));
        match api::qr_poll_action(last_status, status, &body) {
            QrPollAction::Wait(s) => last_status = s,
            QrPollAction::Scanned => {
                println!("已扫码，等待确认……");
                last_status = 2;
            }
            QrPollAction::Refresh(delay) => {
                tokio::time::sleep(delay).await;
                return Err("二维码已刷新周期（重跑 uu-login 重新生成）。".into());
            }
            QrPollAction::Failed(msg) => return Err(msg.into()),
            QrPollAction::Confirmed => break,
        }
    }

    let (status, body) = client
        .post_json("/api/v1/login/by_qrcode", &json!({"token": ticket, "qrcode_jump_url": url}))
        .await?;
    let data = api_data("login/by_qrcode", status, &body)?;
    let token = data.get("token").and_then(Value::as_str).ok_or("by_qrcode 没有 token")?;
    let csrf = api::csrf_from_token(token).ok_or("token 没有 csrf 声明")?;
    let nickname = data.get("nickname").and_then(Value::as_str).unwrap_or("?");
    println!("登录成功：用户 {nickname}（user_id 长度 {}），token 寿命 {:?} s。", data.get("user_id").and_then(Value::as_str).unwrap_or("").len(), jwt_exp(token));
    LAST_QR.with(|v| *v.borrow_mut() = Some(data.clone()));
    let _ = csrf;
    Ok(())
}

thread_local! {
    static LAST_QR: std::cell::RefCell<Option<Value>> = const { std::cell::RefCell::new(None) };
}
fn last_qr_result() -> Option<Value> {
    LAST_QR.with(|v| v.borrow().clone())
}

/// JWT exp 剩余秒数（只报寿命）。
fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let exp = serde_json::from_slice::<Value>(&bytes).ok()?.get("exp")?.as_i64()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(exp - now)
}

/// 把凭证字段合并进现有配置（保留其他键）。
fn merge_into_config(file: Option<&Value>, creds: &Value) -> Value {
    let mut out = file.cloned().unwrap_or_else(|| json!({}));
    if let Some(map) = out.as_object_mut() {
        for (k, v) in creds.as_object().unwrap() {
            map.insert(k.clone(), v.clone());
        }
    }
    out
}

/// 设备注册的屏幕摘要：有显示用真实分辨率，否则按参照实现退回 1920x1080。
fn screen_summary() -> String {
    linux_uuremote::capture::x11::X11Screen::open(None, linux_uuremote::capture::x11::DamageLevel::BoundingBox)
        .map(|s| format!("{}x{}", s.width(), s.height()))
        .unwrap_or_else(|_| "1920x1080".into())
}
