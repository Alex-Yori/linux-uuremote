//! 虚拟声卡路由：把默认输出切到自有的 null sink，让「本地静音 + 远端有声」同时成立。
//!
//! 场景：这台笔记本本地扬声器平时 MUTED——直接抓默认 monitor 会连远端一起静音。
//! 参照实现（fork）的做法是加载 `uu-remote-sink`（PipeWire null-audio-sink 节点或
//! Pulse module-null-sink）、把默认输出切过去并搬移正在播的流，抓它的 monitor；
//! 会话结束恢复原默认。崩溃残留下次启动自动回收（持久化 Route + flock）。
//! 机密纪律：不读不打印音频内容，只做路由。

use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

const SINK: &str = "uu-remote-sink";
pub const MONITOR_SOURCE: &str = "uu-remote-sink.monitor";
const MONITOR: &str = MONITOR_SOURCE;

#[derive(serde::Serialize, serde::Deserialize)]
struct Route {
    previous: String,
    module: Option<u32>,
    #[serde(default)]
    pipewire: bool,
}

struct State {
    route: Option<Route>,
    _lock: std::fs::File,
}

fn path() -> std::path::PathBuf {
    crate::config::path().with_file_name("virtual_audio.json")
}

fn run(program: &str, args: &[&str]) -> Result<String, ()> {
    let out = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }
    String::from_utf8(out.stdout).map(|s| s.trim().to_owned()).map_err(|_| ())
}

fn pactl(args: &[&str]) -> Result<String, ()> {
    run("pactl", args)
}

/// pactl 不可用但有 wpctl = 纯 PipeWire 装法（走 pw-cli/wpctl 路径）。
fn pipewire_only() -> bool {
    pactl(&["info"]).is_err() && run("wpctl", &["status"]).is_ok()
}

/// node.name 匹配的节点 id；`null_sink` 限定 factory（认领只认自建的 null sink）。
fn pw_nodes(dump: &str, name: &str, null_sink: bool) -> Vec<u32> {
    let Ok(serde_json::Value::Array(objects)) = serde_json::from_str(dump) else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|o| {
            let props = o.get("info")?.get("props")?;
            if props.get("node.name")?.as_str()? != name {
                return None;
            }
            if null_sink && props.get("factory.name").and_then(|v| v.as_str()) != Some("support.null-audio-sink") {
                return None;
            }
            u32::try_from(o.get("id")?.as_u64()?).ok()
        })
        .collect()
}

fn pw_node(name: &str, null_sink: bool) -> Option<u32> {
    pw_nodes(&run("pw-dump", &["Node"]).ok()?, name, null_sink)
        .first()
        .copied()
}

fn default_sink(pipewire: bool) -> Result<String, ()> {
    if !pipewire {
        return pactl(&["get-default-sink"]);
    }
    run("wpctl", &["inspect", "@DEFAULT_AUDIO_SINK@"])?.lines().find_map(|line| {
        line.trim()
            .trim_start_matches("* ")
            .strip_prefix("node.name = ")
            .map(|v| v.trim_matches('"').to_owned())
    })
    .ok_or(())
}

/// 默认输出是不是静音（决定要不要开虚拟路由）。pactl 输出跟 LC_MESSAGES 走
/// （`Mute: 1` / `Mute: yes` / 中文 `Mute: 是`），前缀后按三语肯定值判；失败退 wpctl。
pub fn default_sink_muted() -> bool {
    pactl(&["get-sink-mute", "@DEFAULT_SINK@"])
        .map(|value| {
            let tail = value.rsplit(':').next().unwrap_or("").trim().to_lowercase();
            matches!(tail.as_str(), "1" | "yes" | "true" | "是")
        })
        .or_else(|_| run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).map(|v| v.contains("MUTED")))
        .unwrap_or(false)
}

fn set_default(pipewire: bool, name: &str) -> Result<(), ()> {
    if !pipewire {
        return pactl(&["set-default-sink", name]).map(|_| ());
    }
    run("wpctl", &["set-default", &pw_node(name, false).ok_or(())?.to_string()]).map(|_| ())
}

fn owned_modules(list: &str) -> Vec<u32> {
    list.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.parse().ok()?;
            if fields.next()? != "module-null-sink" {
                return None;
            }
            fields
                .next()?
                .split_whitespace()
                .any(|arg| arg == "sink_name=uu-remote-sink")
                .then_some(id)
        })
        .collect()
}

fn sink_id(name: &str) -> Option<String> {
    pactl(&["list", "short", "sinks"]).ok()?.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let id = fields.next()?;
        (fields.next()? == name).then(|| id.to_owned())
    })
}

fn move_inputs(from: &str, to: &str) {
    let Some(id) = sink_id(from) else { return };
    if let Ok(list) = pactl(&["list", "short", "sink-inputs"]) {
        for line in list.lines() {
            let mut fields = line.split_whitespace();
            if let (Some(input), Some(sink)) = (fields.next(), fields.next()) {
                if sink == id {
                    let _ = pactl(&["move-sink-input", input, to]);
                }
            }
        }
    }
}

fn persist(route: &Route) -> Result<(), ()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temp = path().with_extension("json.new");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|_| ())?;
    file.write_all(&serde_json::to_vec(route).map_err(|_| ())?).map_err(|_| ())?;
    file.sync_all().map_err(|_| ())?;
    std::fs::rename(&temp, path()).map_err(|_| ())
}

/// 恢复：默认被用户改过就不覆盖；原输出可能没了（拔卡）——照样卸掉自建 sink。
fn recover(route: &Route) -> Result<(), ()> {
    let pw = route.pipewire;
    if default_sink(pw).is_ok_and(|s| s == SINK) && set_default(pw, &route.previous).is_err() {
        println!("   虚拟音频：原默认输出不可用，交给声音服务自选新默认");
    }
    if pw {
        for id in pw_nodes(&run("pw-dump", &["Node"])?, SINK, true) {
            run("pw-cli", &["destroy", &id.to_string()])?;
        }
    } else {
        move_inputs(SINK, &route.previous);
        for id in owned_modules(&pactl(&["list", "short", "modules"])?) {
            pactl(&["unload-module", &id.to_string()])?;
        }
    }
    let _ = std::fs::remove_file(path());
    Ok(())
}

fn state() -> &'static Mutex<Option<State>> {
    STATE.get_or_init(|| {
        use std::os::unix::fs::OpenOptionsExt;
        let init = (|| {
            std::fs::create_dir_all(path().parent().ok_or(())?).map_err(|_| ())?;
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path().with_extension("lock"))
                .map_err(|_| ())?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(());
            }
            let route = if path().exists() {
                let old: Route = serde_json::from_slice(&std::fs::read(path()).map_err(|_| ())?).map_err(|_| ())?;
                if recover(&old).is_err() {
                    Some(old)
                } else {
                    None
                }
            } else {
                None
            };
            Ok(State { route, _lock: lock })
        })();
        Mutex::new(init.ok())
    })
}

static STATE: OnceLock<Mutex<Option<State>>> = OnceLock::new();

/// 启动即调用：回收上次崩溃残留的虚拟路由。
pub fn init() {
    let _ = state();
}

/// 开虚拟路由（已是自建的则幂等）。失败 = 走默认 monitor 的旧路。
pub fn enable() -> bool {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else { return false };
    if state.route.is_some() {
        return pactl(&["get-default-sink"]).is_ok_and(|s| s == SINK);
    }
    let result = (|| {
        let pipewire = pipewire_only();
        let previous = default_sink(pipewire)?;
        if previous == SINK {
            return Err(());
        }
        // 别的实例/手工建的同名 sink：不抢。
        if if pipewire { pw_node(SINK, false).is_some() } else { sink_id(SINK).is_some() } {
            return Err(());
        }
        let mut route = Route { previous, module: None, pipewire };
        persist(&route)?; // 先落盘再加载：load 与 id 落盘之间崩了也能回收。
        state.route = Some(Route { previous: route.previous.clone(), module: None, pipewire });
        let id = if pipewire {
            // object.linger=true：pw-cli 退出后节点还在，recover() 负责销毁。
            run("pw-cli", &["create-node", "adapter", "{ factory.name=support.null-audio-sink node.name=uu-remote-sink \
                node.description=UU-Remote media.class=Audio/Sink object.linger=true audio.position=[FL FR] }"])?;
            let mut node = None;
            for _ in 0..20 {
                node = pw_node(SINK, true);
                if node.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            node.ok_or(())?
        } else {
            pactl(&["load-module", "module-null-sink", "sink_name=uu-remote-sink", "sink_properties=device.description=UU-Remote"])?
                .parse()
                .map_err(|_| ())?
        };
        route.module = Some(id);
        persist(&route)?;
        state.route = Some(route);
        set_default(pipewire, SINK)?;
        if !pipewire {
            let previous = state.route.as_ref().ok_or(())?.previous.clone();
            move_inputs(&previous, SINK);
        }
        Ok(())
    })();
    if result.is_err() {
        if let Some(route) = state.route.as_ref() {
            if recover(route).is_ok() {
                state.route = None;
            }
        }
        println!("   虚拟音频路由不可用（走默认 monitor）");
    }
    result.is_ok()
}

pub fn disable() {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = guard.as_mut() {
        if let Some(route) = state.route.as_ref() {
            if recover(route).is_ok() {
                state.route = None;
            } else {
                println!("   虚拟音频恢复不完整（回收记录保留）");
            }
        }
    }
}

pub fn active() -> bool {
    state().lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|s| s.route.is_some())
}

/// 编码器的抓流 source：路由开着 = 自建 monitor；否则显式覆盖或默认 monitor。
pub fn source() -> String {
    if active() {
        return MONITOR.to_owned();
    }
    std::env::var("UU_AUDIO_SOURCE").unwrap_or_else(|_| "@DEFAULT_MONITOR@".into())
}

/// 正常退出时恢复路由；SIGKILL 下次启动回收。
pub struct Cleanup;
impl Drop for Cleanup {
    fn drop(&mut self) {
        disable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_only_claims_exact_null_sink() {
        let modules = "1\tmodule-null-sink\tsink_name=uu-remote-sink\t0\n2\tmodule-null-sink\tsink_name=uu-remote-sink-other\t0\n3\tmodule-other\tsink_name=uu-remote-sink\t0\n";
        assert_eq!(owned_modules(modules), vec![1]);
    }

    #[test]
    fn pipewire_recovery_only_claims_owned_null_sink() {
        let dump = format!(
            "[{{\"id\":5,\"info\":{{\"props\":{{\"node.name\":\"uu-remote-sink\",\"factory.name\":\"support.null-audio-sink\"}}}}}}, \
             {{\"id\":6,\"info\":{{\"props\":{{\"node.name\":\"uu-remote-sink\",\"factory.name\":\"api.alsa.pcm.sink\"}}}}}}, \
             {{\"id\":7,\"info\":{{\"props\":{{\"node.name\":\"other\",\"factory.name\":\"support.null-audio-sink\"}}}}}}, {{\"id\":8}}]"
        );
        assert_eq!(pw_nodes(&dump, "uu-remote-sink", true), vec![5]);
        assert_eq!(pw_nodes(&dump, "uu-remote-sink", false), vec![5, 6]);
        let old: Route = serde_json::from_str("{\"previous\":\"a\",\"module\":3}").unwrap();
        assert!(!old.pipewire);
    }
}
