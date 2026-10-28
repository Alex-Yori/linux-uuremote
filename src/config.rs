//! 本机配置：路径、读取、原子写入（chmod 600）。
//!
//! 文件：`$UU_CONFIG`，否则 `~/.config/linux-uuremote/config.json`。
//! 环境变量 `UU_*` 覆盖文件值（见 `api::Credentials::load`）。
//! 登录态缺失时**只读借用**参照实现的 `~/.config/uuremote/config.json`，
//! 让云连接测试可以先不扫码；写入永远只写本项目自己的文件。

use std::path::PathBuf;

/// 本项目配置文件路径（写入目标）。
pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("UU_CONFIG") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("linux-uuremote/config.json")
}

/// 参照实现（uuremote-server）的配置文件，仅作只读借用。
pub fn fallback_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("uuremote/config.json")
}

/// 读本项目配置；不存在返回 None。损坏的文件是错误，不静默忽略。
pub fn read() -> Result<Option<serde_json::Value>, String> {
    read_at(&path())
}

pub fn read_at(p: &PathBuf) -> Result<Option<serde_json::Value>, String> {
    match std::fs::read(p) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("invalid config {}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read config {}: {e}", p.display())),
    }
}

/// 本项目配置优先；缺凭证时借用参照实现的文件，返回 (值, 是否借用)。
pub fn read_with_fallback() -> Result<(Option<serde_json::Value>, bool), String> {
    let own = read()?;
    if credentials_complete(own.as_ref()) {
        return Ok((own, false));
    }
    if std::env::var_os("UU_CONFIG").is_none() {
        if let Some(fb) = read_at(&fallback_path())? {
            if credentials_complete(Some(&fb)) {
                return Ok((Some(fb), true));
            }
        }
    }
    Ok((own, false))
}

/// 判断一组凭证字段是否齐（与 `api::Credentials::load` 同一判据）。
pub fn credentials_complete(file: Option<&serde_json::Value>) -> bool {
    let get = |k: &str| {
        std::env::var(format!("UU_{}", k.to_uppercase()))
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| file.and_then(|f| f.get(k)).and_then(|v| v.as_str()).map(str::to_owned))
            .unwrap_or_default()
    };
    ["user_id", "device_id", "client_id", "system_id", "token", "csrf"]
        .iter()
        .all(|k| !get(k).is_empty())
}

/// 原子写入（tmp + rename），权限 0600。token 是机密，文件不能进 git。
pub fn write(cfg: &serde_json::Value) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let p = path();
    let dir = p.parent().ok_or("config path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = p.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    f.write_all(serde_json::to_string_pretty(cfg).unwrap_or_default().as_bytes())
        .map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("replace {}: {e}", p.display()))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 两个测试都动 UU_CONFIG 环境变量：并行跑会互相踩，串行化。
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        &LOCK
    }

    #[test]
    fn fallback_only_kicks_in_when_own_file_lacks_credentials() {
        let _g = env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join(format!("uu-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("UU_CONFIG", dir.join("own.json"));
        // own 不存在、字段不全的两个阶段都不算完整凭证
        assert!(!credentials_complete(None));
        assert!(!credentials_complete(Some(&json!({"user_id": "u"}))));
        assert!(credentials_complete(Some(&json!({
            "user_id": "u", "device_id": "d", "client_id": "c",
            "system_id": "s", "token": "t", "csrf": "x"
        }))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_is_private_and_roundtrips() {
        use std::os::unix::fs::PermissionsExt;
        let _g = env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join(format!("uu-cfg-w-{}", std::process::id()));
        std::env::set_var("UU_CONFIG", dir.join("config.json"));
        write(&json!({"token": "secret"})).unwrap();
        let meta = std::fs::metadata(dir.join("config.json")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(read().unwrap().unwrap()["token"], json!("secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
