# Linux 被控端七项性能/协议问题修复 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。本次按 Yori 要求**不使用子代理**，内联执行。

**目标：** 修复 uuremote-host 被控端的 7 个问题：连接时长 30s+、码率在几百 Kbps 与几 Mbps 间振荡、延迟恒 50ms、帧间隔 ≥50ms（应 ~20ms）、画面大变动时掉帧、空闲/变动时的码率-帧率策略、键盘长按只生效一次。

**架构：** 全部改动集中在被控端管线：`control.rs`（画质档→码率天花板映射）、`nvenc.rs`（ffmpeg NVENC 参数）、`src/keyrepeat.rs`（新建，长按状态机）、`input.rs`+`uinput.rs`（长按注入）、`bin/uuremote-host.rs`（capture_loop 配速/自适应/重建风暴、连接阶段计时与能力上报时机）、`transport.rs`（ICE 收集等待）。

**技术栈：** Rust（tokio、webrtc-rs 0.10、x11rb、libc/uinput）、ffmpeg h264_nvenc（VBR+CQ）、`cargo test`。

**参考金标准：**
- fork 参照实现 `~/rea-work/uuremote/fork-uu-remote-linux`（GOP 0.5s 进程内编码、pacer、画质档映射）。
- 逆向 Windows 官方 `~/rea-work/uuremote/win-full/$_37_/streamer.dll`（RTP 扩展、rc_mode/vbr/cbr 字符串）；官方被控端在 **PeerConnection Connected 时**经信令发 device_capability / sender_para_info / ice_finished（fork `src/webrtc.rs:333-376` 照抄了这个时机）。

## 症状 → 根因（本机取证结论）

| # | 症状 | 根因（证据） | 修法 |
|---|------|-------------|------|
| 1 | 连接 30s+ | 三条能力上报（device_capability/sender_para_info/ice_finished）在 ControlOpen 后才发，还夹着等首帧 geom_rx + 15s is_connected 兜底循环（`bin/uuremote-host.rs:826-880`）；官方是 Connected 即发。另外 answer 前等 ICE 收集 1s（`transport.rs:500`） | 任务 5：Connected 即发三条 + 阶段耗时日志 |
| 2 | 码率几百 K↔几 M 振荡 | ① 每次 PLI/关键帧请求**重启 ffmpeg 进程**（`uuremote-host.rs:1209-1217`），重启风暴杀流+IDR 尖峰；② 自适应降帧也重建编码器（`:1270-1280`）；③ 画质档映射与原画 30M 期望对不上（`control.rs:221-230` quality=4→20M、5→0） | 任务 1/3/4：映射修正 + 重建限流 + 去掉自适应重建 |
| 3 | 延迟恒 50 | 帧间隔就是 50ms（见 4），接收端能看到的真实管线延迟被配速撑大；配速下限 10Mbps 在 LAN 上纯属多余（LAN 零丢包） | 任务 4：零丢包旁路配速 |
| 4 | 帧间隔 ≥50ms 甚至 100+ | ① 大 AU 按 2.5×码率串行化 + 欠账封顶两帧 → 一坨 IDR 憋出 100ms+ 间隔（`:1240-1262`）；② ffmpeg 空闲不产帧时窗口把"空闲秒数"当 fps 分母，误触发降帧重建（`:1266-1284`） | 任务 4 |
| 5 | 大变动时掉帧 | 重建风暴（任务 3）+ 自适应把空闲误判成编码器跟不上（任务 4） | 任务 3/4 |
| 6 | 空闲可掉个位数帧率 ✓，但变动时码率应达 高清8M/超清14M/原画30M 而实际只有几百 K | 画质档天花板映射错位（原画=quality 4/5→20M/0），且空闲误降帧把 fps 钉死 | 任务 1（映射+日志验证真实 enum）+ 任务 4 |
| 7 | 键盘长按只生效一次 | uinput 注入的键内核**不做**自动重复（实测 `rep_test`：EV_REP 设备 3s 内 value=2 事件 = 0）；`kbd_heartbeat` 目前只打日志（`uuremote-host.rs:528-532`） | 任务 2：宿主侧长按重复状态机 + 心跳也注入 |

## 文件结构

- 修改 `src/control.rs`：`preset_bitrate_kbps`（:221-230）按官方档表映射（原画 30M/超清 14M/高清 8M/标准 2M/流畅 1M），新增 `preset_cq`。
- 修改 `src/nvenc.rs`：`preset_cq`（:68-77）改调 `control::preset_cq`；命令行加 `-strict_gop 1`（GOP 间码率平稳，正好治振荡）。
- 创建 `src/keyrepeat.rs`：纯逻辑长按重复状态机（可离线单测）。
- 修改 `src/lib.rs`：`pub mod keyrepeat;`。
- 修改 `src/uinput.rs`：`Keyboard::open()`（:110-118）声明 EV_REP 属性（治标但无害：部分消费者读 REP 默认值）。
- 修改 `src/input.rs`：`X11Input` 挂 `KeyRepeater`，press/release 记账、每次 apply 先排空到期的重复注入。
- 修改 `src/bin/uuremote-host.rs`：RPC 画质日志、keyframe 重建限流、fps 变更不重建 NVENC、自适应排除空闲间隙、零丢包旁路配速、空闲 AU 保活、连接阶段计时、Connected 即发能力三条。
- 修改 `src/transport.rs`：ICE 收集等待 1s→300ms（本机 candidate 即时可得，其余走 trickle）。
- 测试：`src/control.rs`、`src/keyrepeat.rs`、`src/input.rs` 内 `#[cfg(test)]`；实机验收脚本（journalctl 指标）。

---

### 任务 1：画质档 → 码率天花板映射修正（问题 6/2）

**文件：**
- 修改：`src/control.rs:221-230`（`preset_bitrate_kbps`）
- 修改：`src/nvenc.rs:68-77`（`preset_cq`）
- 修改：`src/bin/uuremote-host.rs`（apply_rpc 画质变更分支，加原始 enum 日志）

- [ ] **步骤 1：编写失败的测试**（`src/control.rs` 测试模块末尾追加）

```rust
#[test]
fn preset_bitrates_follow_official_windows_tiers() {
    // Yori 的 Windows 客户端档位期望：流畅≈1M 标准≈2M 高清 8M 超清 14M 原画 30M。
    assert_eq!(preset_bitrate_kbps(1, 0), 1_000);
    assert_eq!(preset_bitrate_kbps(2, 0), 2_000);
    assert_eq!(preset_bitrate_kbps(3, 0), 8_000);
    assert_eq!(preset_bitrate_kbps(4, 0), 14_000);
    assert_eq!(preset_bitrate_kbps(5, 0), 30_000);
    assert_eq!(preset_bitrate_kbps(6, 25_000_000), 25_000);
    assert_eq!(preset_bitrate_kbps(9, 0), 0); // 自动：不设天花板
}

#[test]
fn preset_cq_follows_quality_tiers() {
    assert_eq!(crate::control::preset_cq(5), 17); // 原画
    assert_eq!(crate::control::preset_cq(4), 21); // 超清
    assert_eq!(crate::control::preset_cq(3), 26); // 高清
    assert_eq!(crate::control::preset_cq(2), 30); // 标准
    assert_eq!(crate::control::preset_cq(1), 34); // 流畅
    assert_eq!(crate::control::preset_cq(9), 18); // 自动/自定义
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --lib preset_bitrates preset_cq`
预期：FAIL（4→14_000 与 5→30_000 断言不过；`preset_cq` 还不存在）

- [ ] **步骤 3：编写实现**

`src/control.rs` 替换 `preset_bitrate_kbps`（:221-230），并在其后新增 `preset_cq`：

```rust
/// 画质档 → 码率天花板 kbps（对齐官方 Windows 客户端档位表：原画 30M、超清 14M、
/// 高清 8M、标准 2M、流畅 1M；自定义取客户端给的 bps；自动 0 = 不设天花板）。
pub fn preset_bitrate_kbps(quality: u64, custom_bps: u64) -> u32 {
    match quality {
        1 => 1_000,
        2 => 2_000,
        3 => 8_000,
        4 => 14_000,
        5 => 30_000,
        6 => (custom_bps / 1_000).clamp(1, 100_000) as u32,
        _ => 0,
    }
}

/// 画质档 → NVENC 常量质量 CQP（质量优先时这才是画质旋钮，码率只是天花板）。
pub fn preset_cq(quality: u64) -> u32 {
    match quality {
        5 => 17,
        4 => 21,
        3 => 26,
        2 => 30,
        1 => 34,
        _ => 18,
    }
}
```

注意：原代码把 5 当"自动"。官方客户端的枚举没有对照表可查，**以实机日志为准**：本任务同时在 RPC 处打原始值日志（步骤 4），若实机显示 4=超清 5=原画 则本映射成立；若 5=自动、6=原画，按日志改表（一处集中，改完重跑测试）。

`src/nvenc.rs:68-77` 的 `preset_cq` 改为转发（保持对外 API 不变，host 里十几处调用不动）：

```rust
/// 画质档 → NVENC CQP（映射表在 control.rs，与码率天花板同一真源）。
pub fn preset_cq(quality: u64) -> u32 {
    crate::control::preset_cq(quality)
}
```

`src/bin/uuremote-host.rs` 两处画质落地（:282 与 :638 附近 `let kbps = preset_bitrate_kbps(quality, custom);`）各加一行原始值日志：

```rust
println!("   画质档原始值：quality={quality} custom_bps={custom} → {kbps} kbps / CQP {}", linux_uuremote::nvenc::preset_cq(quality));
```

（:638 处变量名是 `p.custom_bps.unwrap_or(0)`，日志里用同一个表达式。）

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --lib preset` → PASS（含旧测试 `qos_stats_reports_encoder_names_and_tier_bitrates`；若它断言 4→20_000 则同步改为 14_000）。
运行：`cargo build --release --bins` → 0 error。

- [ ] **步骤 5：Commit**

```bash
git add src/control.rs src/nvenc.rs src/bin/uuremote-host.rs
git commit -m "host: 画质档码率天花板对齐官方档表（原画30M/超清14M/高清8M），CQP 映射集中到 control.rs，RPC 打原始画质 enum"
```

---

### 任务 2：键盘长按（问题 7）

**文件：**
- 创建：`src/keyrepeat.rs`
- 修改：`src/lib.rs`（加 `pub mod keyrepeat;`）
- 修改：`src/input.rs`（X11Input 挂状态机）
- 修改：`src/bin/uuremote-host.rs:528-532`（kbd_heartbeat 也注入一次重复）
- 修改：`src/uinput.rs:110-118`（声明 EV_REP 属性）

**实测依据：** 内核 uinput 自动重复不可用（scratch/rep_test：EV_REP 设备按住 3s，value=2 事件 0 个），所以重复必须由宿主自己做；心跳消息同时注入，客户端若发心跳则天然同步。

- [ ] **步骤 1：编写失败的测试**（新建 `src/keyrepeat.rs`，先只放测试与类型骨架）

```rust
//! 键盘长按重复：uinput 注入的键内核不会自动重复（实测 EV_REP 无效），
//! 官方客户端靠 kbd_heartbeat 表达"还按着"，宿主必须自己按系统键盘节奏重发按下。
use std::time::{Duration, Instant};

pub struct KeyRepeater {
    delay: Duration,
    period: Duration,
    held: Vec<(u16, Instant)>,
}

impl KeyRepeater {
    pub fn new(delay: Duration, period: Duration) -> Self {
        Self { delay, period, held: Vec::new() }
    }
    pub fn press(&mut self, code: u16, at: Instant) { /* TODO 本步骤先不实现 */ }
    pub fn release(&mut self, code: u16) { /* TODO */ }
    pub fn due(&mut self, now: Instant) -> Vec<u16> { /* TODO */ }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn repeat_starts_after_delay_then_every_period() {
        let t = base();
        let mut r = KeyRepeater::new(Duration::from_millis(500), Duration::from_millis(33));
        r.press(14, t); // Backspace
        assert!(r.due(t + Duration::from_millis(499)).is_empty());
        assert_eq!(r.due(t + Duration::from_millis(500)), vec![14]);
        assert!(r.due(t + Duration::from_millis(532)).is_empty());
        assert_eq!(r.due(t + Duration::from_millis(533)), vec![14]);
        assert_eq!(r.due(t + Duration::from_millis(566)), vec![14]);
    }

    #[test]
    fn release_stops_repeat_and_repress_restarts_delay() {
        let t = base();
        let mut r = KeyRepeater::new(Duration::from_millis(500), Duration::from_millis(33));
        r.press(14, t);
        assert_eq!(r.due(t + Duration::from_millis(600)), vec![14]);
        r.release(14);
        assert!(r.due(t + Duration::from_millis(700)).is_empty());
        r.press(14, t + Duration::from_millis(800));
        assert!(r.due(t + Duration::from_millis(1_299)).is_empty());
        assert_eq!(r.due(t + Duration::from_millis(1_300)), vec![14]);
    }

    #[test]
    fn two_held_keys_repeat_independently() {
        let t = base();
        let mut r = KeyRepeater::new(Duration::from_millis(500), Duration::from_millis(33));
        r.press(14, t);
        r.press(15, t + Duration::from_millis(200));
        assert_eq!(r.due(t + Duration::from_millis(500)), vec![14]);
        assert_eq!(r.due(t + Duration::from_millis(700)), vec![15]);
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --lib keyrepeat` → 编译通过但 3 个测试 FAIL（骨架返回空/panic）。
（若 TODO 骨架编译不过，就先把三个方法写成空实现：press/release 什么都不做、due 返回 Vec::new()。）

- [ ] **步骤 3：最少实现**

```rust
impl KeyRepeater {
    /// 新按下：同码先清账（长按中再收到 press = 客户端重发，重新计时）。
    pub fn press(&mut self, code: u16, at: Instant) {
        self.held.retain(|&(c, _)| c != code);
        self.held.push((code, at));
    }

    pub fn release(&mut self, code: u16) {
        self.held.retain(|&(c, _)| c != code);
    }

    /// 此刻该重发的键（按下的键每个都到齐自己的节奏；一次调用最多各发一次）。
    pub fn due(&mut self, now: Instant) -> Vec<u16> {
        let mut out = Vec::new();
        for (_, first) in self.held.iter_mut() {
            let start = *first + self.delay;
            if now < start {
                continue;
            }
            let due_at = start + ((now - start) / self.period) * self.period;
            if *first < due_at {
                *first = due_at;
                out.push_code(self, due_at); // 见下：借用冲突，用下面的写法替代
            }
        }
        out
    }
}
```

上面 `due` 的借用写法别扭，**直接用这版**（整个方法替换）：

```rust
pub fn due(&mut self, now: Instant) -> Vec<u16> {
    let mut out = Vec::new();
    for entry in self.held.iter_mut() {
        let start = entry.1 + self.delay;
        if now < start {
            continue;
        }
        let due_at = start + ((now - start) / self.period) * self.period;
        if entry.1 < due_at {
            entry.1 = due_at;
            out.push(entry.0);
        }
    }
    out
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --lib keyrepeat` → 3 PASS。

- [ ] **步骤 5：接到 X11Input**

`src/lib.rs`：`pub mod keyrepeat;`（与现有 mod 列表同段）。

`src/input.rs`：
1. `X11Input` 结构体（:213-229）加字段 `repeater: crate::keyrepeat::KeyRepeater,`。
2. `connect()`（:231 起）构造处加：

```rust
let delay = std::env::var("UU_KEY_REPEAT_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
let period = std::env::var("UU_KEY_REPEAT_PERIOD_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(33);
// … Ok(Self { …, repeater: crate::keyrepeat::KeyRepeater::new(Duration::from_millis(delay), Duration::from_millis(period)), … })
```

（顶部确认已有 `use std::time::Duration;`，没有就加。）

3. `apply()`（:266 起）开头排空到期重复，Keyboard 分支记账：

```rust
pub fn apply(&mut self, message: &ControlMessage) -> Result<bool, String> {
    let now = std::time::Instant::now();
    let mut injected = false;
    for code in self.repeater.due(now) {
        if let Some(kb) = self.keyboard.as_mut() {
            kb.key(code, true)?;
            injected = true;
        }
    }
    match message {
        ControlMessage::Input(ControlInput::Keyboard { key, pressed }) => {
            if let Some(code) = vk_to_evdev(*key) {
                if *pressed { self.repeater.press(code, now); } else { self.repeater.release(code); }
            }
            if self.inject_uinput_key(*key, *pressed)? { return Ok(true); }
            let evs = self.key_events(*key, *pressed);
            for e in &evs { self.inject(e)?; }
            Ok(true)
        }
        ControlMessage::KeyboardHeartbeat => {
            // 客户端说"还按着"：立刻补一次重复（不等延迟——心跳本身就是节奏）。
            for code in self.repeater.held_codes() {
                if let Some(kb) = self.keyboard.as_mut() { kb.key(code, true)?; injected = true; }
            }
            Ok(injected)
        }
        …其余分支照旧，返回值改成 `Ok(true)`/`Ok(false)` 语义不变（events 为空且 !injected 时 `Ok(injected)`）。
    }
}
```

`vk_to_evdev` 提取为小工具（与 `inject_uinput_key` 共用映射，避免重复查表）：

```rust
fn vk_to_evdev(vk: i32) -> Option<u16> {
    windows_vk_to_keysym(vk).and_then(crate::uinput::keysym_to_evdev)
}
```

`KeyRepeater` 补一个只读方法（`src/keyrepeat.rs`）：

```rust
/// 当前按住的键码（心跳注入用，不动节奏）。
pub fn held_codes(&self) -> Vec<u16> {
    self.held.iter().map(|&(c, _)| c).collect()
}
```

并给它的测试加一条：

```rust
#[test]
fn held_codes_lists_held_keys_without_changing_schedule() {
    let t = base();
    let mut r = KeyRepeater::new(Duration::from_millis(500), Duration::from_millis(33));
    r.press(14, t);
    assert_eq!(r.held_codes(), vec![14]);
    assert!(r.due(t + Duration::from_millis(499)).is_empty()); // 节奏未被 held_codes 打乱
}
```

4. `release_all()`（:317 起）开头加 `self.repeater = crate::keyrepeat::KeyRepeater::new(Duration::ZERO, Duration::from_millis(1));`（会话结束清空账本，直接构造空 repeater 更简单：改成新建一个 delay/period 同值的即可，或者加 `KeyRepeater::clear()`：`self.held.clear();` —— **用 clear()**，并在 keyrepeat.rs 加：

```rust
pub fn clear(&mut self) {
    self.held.clear();
}
```

5. XTEST 兜底路径（无 uinput 时）也重复：在 `due()` 排空处，如果 `keyboard` 为 None，用 `key_events` 的 keycode 路径重发 press+release 一对（`xtest_fake_input(KEYPRESS…)` + `KEYRELEASE`，keycode 查 `self.keymap`）：

```rust
for code in self.repeater.due(now) {
    if let Some(kb) = self.keyboard.as_mut() {
        kb.key(code, true)?;
        injected = true;
    } else if let Some(keycode) = self.keycode_for_evdev(code) {
        self.conn.xtest_fake_input(KEYPRESS, 0, 0, self.root, keycode as i16, 0, 0)
            .map_err(|e| format!("XTest 重复按下失败: {e}"))?;
        self.conn.xtest_fake_input(KEYRELEASE, 0, 0, self.root, keycode as i16, 0, 0)
            .map_err(|e| format!("XTest 重复松开失败: {e}"))?;
        injected = true;
    }
}
```

`keycode_for_evdev` 在 `Keymap` 已有反查就用现成的；若没有，用 keysym 反查链：evdev→keysym（`keysym_to_evdev` 的逆表在 uinput.rs 里建 `evdev_to_keysym(code) -> Option<u32>`，遍历现有 match 区间实现），再 `self.keymap.keycode_for(ks)`。实现顺序：先 uinput 路径（本机生效），XTEST 路径仅编译正确即可（本机一定有 uinput）。

- [ ] **步骤 6：uinput 设备声明 EV_REP（无害的治标项）**

`src/uinput.rs:20-23` 常量区加：

```rust
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_5568;
const EV_REP: u16 = 0x0004;
const REP_DELAY: u16 = 0x0001;
const REP_PERIOD: u16 = 0x0002;
```

`Keyboard::open()` 的 configure 闭包里加（UI_DEV_CREATE 前）：

```rust
let _ = d.ioctl(UI_SET_PROPBIT, EV_REP.into()); // 声明重复能力；内核不代劳，仅让读取方看到默认 500/33
```

- [ ] **步骤 7：host 心跳仍打日志但不再吞掉**

`src/bin/uuremote-host.rs:528-532` 的 `ControlMessage::KeyboardHeartbeat` 分支改为把心跳转发给输入引擎：

```rust
ControlMessage::KeyboardHeartbeat => {
    if session.control_msgs <= 3 {
        println!("   kbd 心跳（转发为重复按下）");
    }
    if let Some(engine) = session.input.as_mut() {
        let _ = engine.apply(&ControlMessage::KeyboardHeartbeat);
    }
}
```

（`session.input` 是 `Option<Arc<Mutex<X11Input>>>`——按现有 Input 分支的写法取锁，形如 `engine.lock().unwrap().apply(...)`，与 `ControlMessage::Input` 分支完全一致。）

- [ ] **步骤 8：全量测试 + 编译**

运行：`cargo test --release` → 全绿（原 155 个 + 新增 4 个）。
运行：`cargo build --release --bins` → 0 error。
手工验收：起 host + uu-viewer，长按 Backspace 3 秒 → 本机文本框持续删除（`UU_KEY_REPEAT_PERIOD_MS` 可调节奏）。

- [ ] **步骤 9：Commit**

```bash
git add src/keyrepeat.rs src/lib.rs src/input.rs src/uinput.rs src/bin/uuremote-host.rs
git commit -m "host: 键盘长按——宿主重复状态机（500/33ms，env 可调）+ kbd_heartbeat 转发注入；内核 EV_REP 实测对 uinput 无效"
```

---

### 任务 3：重建风暴止血（问题 2/5 的一半）

**文件：**
- 修改：`src/bin/uuremote-host.rs` capture_loop：keyframe 分支（:1209-1217）、fps 变更分支（:1110-1140 区域）、自适应降帧分支（:1266-1284）

- [ ] **步骤 1：keyframe 请求限流**（PLI/FIR 在空闲期会连打，每次重启 ffmpeg = 掉帧+码率尖峰）

在 capture_loop 变量区（`let mut nvenc_retry_at = Instant::now();` 附近，:1113）加：

```rust
// 关键帧请求限流：PLI/FIR 连着来只重启一次，重启后的首帧本身就是 IDR。
let mut last_rebuild = Instant::now();
```

keyframe 分支（原 :1209-1217）整体替换为：

```rust
if keyframe.load(Ordering::Relaxed) {
    // 重启进程即 IDR（zerolatency 无 lookahead，塞不进 IDR）；3s 内不重复重启。
    if Instant::now() >= last_rebuild + Duration::from_secs(3) {
        keyframe.store(false, Ordering::Relaxed);
        rebuild_backend(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop, linux_uuremote::nvenc::preset_cq(settings.quality.load(Ordering::Relaxed)), settings.chroma.load(Ordering::Relaxed) == 1, display);
        win_start = Instant::now();
        win_frames = 0;
        win_bytes = 0;
        paced = Duration::ZERO;
        send_time = Instant::now();
        last_rebuild = Instant::now();
        println!("   关键帧请求 → NVENC 重启（首帧 IDR；3s 内连打只重启一次）");
    }
}
```

- [ ] **步骤 2：远程改帧率不再重建 NVENC**（x11grab 是上限不是配额；vfr 下实际出帧率天然 ≤ 设定值，重建纯属自伤）

fps 变更分支中删除这段（原 :1133-1140）：

```rust
if backend_nvenc {
    rebuild_backend(&mut pipe, &mut encoder, &mut backend_nvenc, width, height, fps, applied_bitrate, gop, linux_uuremote::nvenc::preset_cq(settings.quality.load(Ordering::Relaxed)), settings.chroma.load(Ordering::Relaxed) == 1, display);
    win_start = Instant::now();
    win_frames = 0;
    win_bytes = 0;
    paced = Duration::ZERO;
    send_time = Instant::now();
}
```

改为只记时间线/门控（前面的 gate/planner/timeline 三行保留），并打日志说明不再重建：

```rust
println!("   远程改帧率 → {fps} fps（不重建编码器：x11grab 帧率是上限，vfr 自带配速）");
```

- [ ] **步骤 3：自适应降帧只改配速、不重建**（原 :1270-1280）

把分支内的 `rebuild_backend(...)` 与 `win_*` 重置删除，改为：

```rust
if sustained < f64::from(fps) * 0.85 {
    let target = (sustained.floor() as u32).max(15).min(fps - 1);
    println!("   持续出帧率 {sustained:.1} fps < 设定 {fps} fps → 时间线配速降到 {target} fps（不重建编码器）");
    downgrade_from = fps;
    fps = target;
    downgraded = true;
    timeline = RtpTimeline::new(fps);
    last_rebuild = Instant::now();
}
```

- [ ] **步骤 4：编译 + 现有测试回归**

运行：`cargo build --release --bins` → 0 error；`cargo test --release` → 全绿。
（此任务无纯逻辑可测，行为在任务 6 实机验收里验证：空闲期 journal 不应出现连续多条"NVENC 重启"。）

- [ ] **步骤 5：Commit**

```bash
git add src/bin/uuremote-host.rs
git commit -m "host: 重建风暴止血——关键帧请求 3s 限流；改帧率/自适应降帧不再重启 NVENC 进程"
```

---

### 任务 4：配速旁路 + 空闲保活 + 自适应防误判（问题 3/4/5/6）

**文件：**
- 修改：`src/bin/uuremote-host.rs`：RemoteSettings（:82-117）、ReceiverReport 分支（:437-442）、pacer 块（:1237-1262）、自适应块（:1266-1284）、drain 循环（:1190-1260）

- [ ] **步骤 1：RemoteSettings 加丢包与连通标志**

`RemoteSettings`（:82 起）加两个字段 + new() 初始化：

```rust
/// 接收端报告的丢包（fraction_lost 0..255）：0 = 链路干净（LAN 直连），配速直接旁路。
loss: AtomicU64,
/// PeerConnection 是否已连通（空闲间隙判定的基准：没连通时不算"编码器跟不上"）。
connected: AtomicBool,
```

`new()` 里：`loss: AtomicU64::new(0), connected: AtomicBool::new(false),`。

ReceiverReport 分支（:437）改为：

```rust
Some(PeerEvent::ReceiverReport { fraction_lost, total_lost }) => {
    settings.loss.store(fraction_lost as u64, Ordering::Relaxed);
    if fraction_lost > 0 {
        println!("   视频丢包：fraction_lost={fraction_lost} total_lost={total_lost}（配速接管）");
    }
}
```

ConnectionState 分支（:405 起）在 `state == "connected"` 时加 `settings.connected.store(true, Ordering::Relaxed);`，disconnected/closed/failed 时置 false。

- [ ] **步骤 2：零丢包旁路配速**

pacer 块（原 :1243-1262，`let pace_kbps = ...` 起）替换为：

```rust
// 配速只在链路有丢包时才需要（官方 pacer 的意义是防拥塞）。LAN 直连零丢包时
// 串行化纯属自伤：一坨 IDR 按 10Mbps 下限摊 = 100ms+ 的帧间隔，正是"frm 50-100ms、
// 延迟 50ms"的直接来源。丢包一出现立刻恢复 2.5× 码率的配速。
if settings.loss.load(Ordering::Relaxed) > 0 {
    let pace_kbps = (applied_bitrate as u64 * 5 / 2).max(10_000);
    send_time += Duration::from_micros(unit_len * 8 * 1_000 / pace_kbps);
    let pace_now = Instant::now();
    if send_time > pace_now {
        let gap = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
        let wait = (send_time - pace_now).min(gap);
        std::thread::sleep(wait);
        paced += wait;
        send_time = send_time.min(Instant::now() + gap * 2);
    }
} else {
    // 旁路时把"该睡的时间"还回去，避免恢复丢包瞬间背着旧欠账。
    send_time = Instant::now();
}
```

- [ ] **步骤 3：自适应统计排除空闲间隙**

drain 循环（取帧处，`let unit = match ...` 前，:1195 附近）加间隙扣除：

```rust
let last_unit = Instant::now(); // 循环外初始化：let mut last_unit = Instant::now();
// …
if drained == 0 && last_unit.elapsed() > Duration::from_millis(250) {
    // 画面静止（x11grab vfr 不出帧）：这段空闲从"编码器跟不上"的分母里扣掉，
    // 否则空闲 10 s 后窗口一统计就误判降帧——空闲帧率掉个位数是设计行为。
    win_start += last_unit.elapsed() - Duration::from_millis(250);
}
last_unit = Instant::now();
```

（放在每次 `next_unit` 尝试之后；`drained` 已有的定义不变。注意 `win_start` 是 `Instant`，用 `win_start = win_start + gap` 形式。）

自适应条件（原 :1267）加连通门槛：

```rust
if !downgraded && fps > 15 && settings.connected.load(Ordering::Relaxed) && win_start.elapsed() >= Duration::from_secs(3) {
```

- [ ] **步骤 4：空闲 AU 保活（客户端解码器与 frm 显示不断粮）**

drain 循环之后（`if drained > 0 { gate.note_change(now); }` 旁，:1263）加：

```rust
// 空闲保活：官方被控端静止时按 still_fps 重发上一帧（"idle repeat"）。x11grab
// 空闲不产帧，重发上一个 AU 让客户端解码器活着、frm 不至于显示几万毫秒。
if self_capture && backend_nvenc {
    if let Some(last) = last_au.as_ref() {
        // 每 AU 首 4 字节是 AUD（-aud 1），数 AUD 即帧数；重发要剥掉它避免双起始码。
        let aus = last.windows(4).filter(|w| w == [0, 0, 0, 1]).count();
        if aus <= 1 && now.duration_since(last_emit) >= Duration::from_millis(1_000 / still_fps.max(1) as u64) {
            if frames_tx.blocking_send(Encoded { data: last.clone(), duration: Duration::from_millis(1) }).is_ok() {
                last_emit = now;
                win_frames += 1;
            }
        }
    }
}
```

变量区加：

```rust
let mut last_au: Option<Vec<u8>> = None;
let mut last_emit = Instant::now();
```

且在每次取出新 AU 处（`frames_tx.blocking_send` 成功后）：`last_au = Some(unit 的副本);  last_emit = Instant::now();`（注意 unit 已被 move 进 Encoded——先 `let unit_len = unit.len() as u64; let keep = unit.clone();` 再 send，然后 `last_au = Some(keep);`）。
多 AUD 的 AU（丢包重排产物）不重发，`aus <= 1` 已处理。`still_fps` 为 0 时不保活（`still_fps.max(1)` 已兜底；若 want 关保活用 `still_fps == 0` 判断跳过）。

- [ ] **步骤 5：nvenc.rs 加 `-strict_gop 1`（GOP 间码率平稳）**

`src/nvenc.rs:148` 的 `.args(["-c:v", "h264_nvenc", "-preset", "p4", "-tune", "ull", "-bf", "0"])` 追加一项：

```rust
.args(["-c:v", "h264_nvenc", "-preset", "p4", "-tune", "ull", "-bf", "0", "-strict_gop", "1"])
```

（ffmpeg 文档：strict_gop "minimize GOP-to-GOP rate fluctuations"——正治码率振荡；CQ 模式下它只约束 GOP 间 VBR 摆动，不锁质量。）

- [ ] **步骤 6：编译 + 回归**

运行：`cargo build --release --bins` → 0 error；`cargo test --release` → 全绿。

- [ ] **步骤 7：Commit**

```bash
git add src/bin/uuremote-host.rs src/nvenc.rs
git commit -m "host: LAN 零丢包旁路配速（帧间隔不再被 10Mbps 摊成 50-100ms）；自适应排除空闲间隙；空闲 AU 保活；NVENC strict_gop 平码率"
```

---

### 任务 5：连接时长（问题 1）

**文件：**
- 修改：`src/bin/uuremote-host.rs`（be-controlled 处理 :297-310、offer 处理 :311-344、ControlOpen :378-386、start_video 能力上报 :772-880）
- 修改：`src/transport.rs:500`（收集等待 1s→300ms）

- [ ] **步骤 1：阶段计时日志**（先量化，再谈修）

`Session` 结构（:60-99 区域）加字段：

```rust
/// 本次会话的连接阶段时间戳（"连接半分钟"的账要能算：be-controlled→offer→answer→connected→control_open→first_frame）。
t_be_controlled: std::time::Instant,
t_answer: Option<std::time::Instant>,
```

`Session {...}` 构造处（:309）：`t_be_controlled: std::time::Instant::now(), t_answer: None,`。

offer 分支 answer 成功后（:333 `println!("   answer 已发出…")` 处）：`session.t_answer = Some(std::time::Instant::now());`。

ControlOpen 分支（:378）加：

```rust
Some(PeerEvent::ControlOpen { label }) => {
    println!("   数据通道 open：{label}");
    if label == CONTROL_LABEL {
        if let Some(session) = session.as_mut() {
            if let Some(t) = session.t_answer {
                println!("   阶段耗时：be-controlled→answer {} ms，answer→control_open {} ms",
                    t.duration_since(session.t_be_controlled).as_millis(),
                    std::time::Instant::now().duration_since(t).as_millis());
            }
            …
        }
    }
}
```

ConnectionState `connected` 分支（:405）加一行：

```rust
println!("   阶段耗时：answer→DTLS connected {} ms", session.as_ref().and_then(|s| s.t_answer.map(|t| std::time::Instant::now().duration_since(t).as_millis())).unwrap_or(0));
```

- [ ] **步骤 2：能力三条提前到 Connected（照官方时机）**

现在三条信令事件（device_capability / sender_para_info / ice_finished）在 `start_video` 里等首帧几何 + 15s 循环。重构：**把这三条的发送挪到 ConnectionState==connected 分支**，CONTROL 初始化报文（Message 14/18/7 等）保持在 ControlOpen 后立刻发（现有顺序不动）。

具体做法：`start_video` 的 spawn（:775 起）里，把现有"等 geom_rx → 发 CONTROL 报文 → `let _ = init_done_tx.send(())` → 再等 15s 发三条信令"的后半段删除；三条信令改由主循环 connected 分支发送——把 route/client_id/ice_id 与 nvenc/refresh 等参数放进一个 `CapabilityInfo`（Session 加字段 `capability: Option<capability::ReportParams>`，start_video 里 `init_done_tx` 触发时填好（含 width/height/nvenc/cap_fps/full_chroma/tiers））。connected 分支：

```rust
if state == "connected" {
    if let Some(params) = session.as_ref().and_then(|s| s.capability.clone()) {
        emit_tx.send((capability::FORWARD_SETTING_EVENT.to_owned(), Some(capability::device_capability(&params.client_id, &params.ice_id, 0, params.refresh, params.codec_impl, false, params.full_chroma, params.codec_w, params.codec_h)))).ok();
        emit_tx.send((capability::FORWARD_SETTING_EVENT.to_owned(), Some(capability::sender_para_info(&params.client_id, &params.ice_id, "X11", params.encoder_name)))).ok();
        if !params.app_control_id.is_empty() {
            emit_tx.send((capability::ICE_FINISHED_EVENT.to_owned(), Some(capability::ice_finished(&params.ice_id, &params.app_control_id)))).ok();
        }
        println!("   能力上报已在 DTLS connected 发出（官方同款时机）");
    }
    …原有音频逻辑…
}
```

`capability::ReportParams` 在 `src/capability.rs` 新增（纯结构体，字段即上面用到的；`#[derive(Clone)]`）。几何未到（init_done 未触发）时 connected 分支不发、等下一个 connected 事件或直接跳过——首帧几何测量实测 <100ms，ControlOpen 必晚于它，所以 capability 填好时 DTLS 早已连通；若 connected 先到，在 start_video 填 capability 时补发（用一个 `AtomicBool capability_sent` 防重发）。

- [ ] **步骤 3：ICE 收集等待 1s→300ms**

`src/transport.rs:500`：`wait_gathering_complete(&self.pc, Duration::from_secs(1))` → `Duration::from_millis(300)`；注释同步改为"本机 candidate 百毫秒内齐；STUN/TURN 慢就让它走 trickle"。

- [ ] **步骤 4：编译 + 回归 + 一次实连计时**

运行：`cargo build --release --bins` → 0 error；`cargo test --release` → 全绿。
实机：`systemctl --user start uuremote-host.service`，用 uu-viewer 连本机设备（UU_VIEWER_DEVICE=aeawvwhivebfi7q5 uu-viewer），journal 里取"阶段耗时"两行：目标 be-controlled→answer < 1s、answer→connected < 2s、answer→control_open < 3s。若账落在"没收到 offer"（be-controlled→offer 巨大），那是云端/控制端侧，记录实测值报告 Yori。

- [ ] **步骤 5：Commit**

```bash
git add src/bin/uuremote-host.rs src/capability.rs src/transport.rs
git commit -m "host: 连接阶段计时日志 + 能力上报提前到 DTLS connected（官方时机）；ICE 收集等待降到 300ms"
```

---

### 任务 6：端到端实机验收（全问题回归）

**文件：** 无新文件；验收脚本产出记录到 `docs/superpowers/plans/2026-10-30-acceptance.md`（新建）。

- [ ] **步骤 1：起服务并确认 fork 冲突已停**

```bash
systemctl --user stop uuremote-native.service   # fork 官方实现，同 device_id 会踢下线（先问 Yori 再停！已在 2026-10-30 停过则跳过）
systemctl --user start uuremote-host.service
journalctl --user -u uuremote-host -f
```

- [ ] **步骤 2：uu-viewer 实连 + 指标读数**

远程窗口状态条应显示：画面大变动时帧率 ≈ 所选档（30/60）、码率向档位天花板靠（原画档大变动 ≥10M）、frm ~16-33ms、延迟个位数~十几 ms。journal 目标：`出帧速率` ≥ 设定 fps×0.85（变动期）；空闲期降到个位数且每 ~1s 一条保活帧（日志"已发 N 帧"连续）；无 3s 内连续"NVENC 重启"。

- [ ] **步骤 3：键盘长按实测**

远程窗口里长按 Backspace：本机文本框连续删除；松开即停。若官方 Windows 客户端在场，journal 出现 `kbd 心跳` → 心跳注入路径同时生效（长按更顺）。

- [ ] **步骤 4：Windows 客户端回归（Yori 手测清单）**

连接时长（计时）、画质档切换后状态条码率（高清 8M/超清 14M/原画 30M 期望值）、振荡是否消失、延迟/frm、长按删除键。若画质档原始值日志显示 enum 与映射表不符（任务 1 步骤 3 的备注），按日志改 `control.rs` 映射表并重跑测试。

- [ ] **步骤 5：把验收结果写进 `docs/superpowers/plans/2026-10-30-acceptance.md` 并 Commit**

```bash
git add docs/superpowers/plans/
git commit -m "docs: 2026-10-30 被控端七项修复实机验收记录"
```

---

## 自检

1. **规格覆盖度：** 问题 1→任务 5；2→任务 1/3/4（映射+重建限流+旁路配速+strict_gop）；3→任务 4（配速旁路）+任务 6 实测；4→任务 4；5→任务 3/4；6→任务 1/4；7→任务 2。无遗漏。
2. **占位符扫描：** 无 TODO/待定；任务 2 步骤 1 的骨架 TODO 是测试驱动过渡态，步骤 3 给出完整实现。
3. **类型一致性：** `preset_cq` 在 control.rs 定义、nvenc.rs 转发、host 调用签名不变；`KeyRepeater::press/release/due/held_codes/clear` 签名在任务 2 内自洽；`RemoteSettings.loss/.connected`、`Session.t_be_controlled/.t_answer/.capability`、`capability::ReportParams` 字段与使用处一致；`Encoded{data, duration}` 沿用现有类型。
