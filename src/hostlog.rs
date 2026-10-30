//! 宿主日志功能：带时间戳的日志文件（默认 ~/.cache/uuremote/host.log，
//! UU_HOST_LOG 可改路径），同时打 stdout（进 journald）。
//!
//! 为什么要单独的文件日志：延迟/frm 这类客户端口径只有把双端数据落盘才能对账——
//! 客户端回传的周期统计 JSON、扩展协商结果、发送侧管线滞留，全得看得见。
//! 每条追加时打开文件（调用频率 ≤1/s，成本可忽略），超 16MB 自动截断。

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn log_path() -> String {
    if let Ok(p) = std::env::var("UU_HOST_LOG") {
        return p;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.cache/uuremote/host.log")
}

/// 东八区 HH:MM:SS.mmm（本机时区固定 Asia/Shanghai，日志读起来不用换算）。
fn stamp() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let day_ms = ms % 86_400_000;
    let secs = day_ms / 1000;
    let hh = (secs / 3600 + 8) % 24;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{hh:02}:{mm:02}:{ss:02}.{:03}", day_ms % 1000)
}

const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

/// 记一行：stdout + 日志文件（带时间戳）。文件写失败不影响运行（stdout 仍在）。
pub fn log(line: &str) {
    println!("   {line}");
    write_to_file(&log_path(), &format!("{} {line}\n", stamp()));
}

/// 记错误行（stderr + 日志文件）。
pub fn log_err(line: &str) {
    eprintln!("   {line}");
    write_to_file(&log_path(), &format!("{} E {line}\n", stamp()));
}

fn write_to_file(path: &str, line: &str) {
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        if f.metadata().map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
            let _ = f.set_len(0);
        }
        let _ = f.write_all(line.as_bytes());
    }
}

/// 截断长文本进日志（周期统计 JSON 一条几 KB，留头不留尾）。
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…(+{}B)", &s[..cut], s.len() - cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_appends_timestamped_line_to_file() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("uu-hostlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.log");
        std::env::set_var("UU_HOST_LOG", &path);
        log("测试一行 latency=50");
        log_err("错误行");
        std::env::remove_var("UU_HOST_LOG");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        // 时间戳格式 HH:MM:SS.mmm
        let first = lines[0].to_string();
        assert!(lines[0].starts_with(|c: char| c.is_ascii_digit()), "{first}");
        assert_eq!(lines[0].chars().nth(8), Some('.'), "缺少毫秒位: {first}");
        assert!(lines[0].contains("测试一行 latency=50"));
        assert!(lines[1].contains(" E 错误行"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stamp_is_24h_clock_with_millis() {
        let s = stamp();
        assert_eq!(s.len(), 12, "{s}");
        assert!(s.matches(':').count() == 2);
        assert!(s.chars().nth(8) == Some('.'));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate("abc", 10), "abc");
        let t = truncate("中文中文中文", 5);
        assert!(t.starts_with("中") && t.contains("…(+"), "{t}");
    }
}
