//! linux-uuremote —— 独立的 UU 远程 Linux 被控端。
//!
//! 采集侧（capture/gating/content/diff/scan/cursor）已实测定型；
//! 连接侧（config/api/signaling/wire）按参照实现验证过的协议事实重写。
//! 决策逻辑与 I/O 分离：gating/content/diff/scan/cursor/wire 是纯逻辑，可确定性单测。

pub mod api;
pub mod capability;
pub mod capture;
pub mod capture_setting;
pub mod config;
pub mod content;
pub mod control;
pub mod cursor;
pub mod desktop_mode;
pub mod encoder;
pub mod input;
pub mod diff;
pub mod gating;
pub mod initialization;
pub mod scan;
pub mod signaling;
pub mod transport;
pub mod audio;
pub mod virtual_audio;
pub mod nvenc;
pub mod rtp_metadata;
pub mod uinput;
pub mod wire;

/// 测试动进程环境变量时串行化（env 是进程级全局，并行测试会互踩）。
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
