# uu-remote-linux（重写版）

网易 **UU 远程** 的 Linux **被控端**，从零写的独立实现。

不是 [VallierDev/uu-remote-linux](https://github.com/VallierDev/uu-remote-linux) 的 fork——那个项目当**参照物**读，代码一行没搬。设计依据是从 UU 官方安卓被控端引擎里逆向出来的行为规格（见 [docs/SPEC.md](docs/SPEC.md)），目标是让官方 Windows / macOS / 安卓客户端把这台 Linux 当普通被控端连，行为对齐官方端。

> **名字提醒**：本机现在跑着的那个参照实现，可执行文件也叫 `uu-remote-linux`（`uuremote-native.service`）。本项目目录同名，但产物叫 `uu-capture-probe`（后续宿主 daemon 会另起名字），不会互相撞。

## 当前状态

第一个能跑的模块是**采集与出帧门控**（`uu-capture-probe`），它已经实测验证了本项目最核心的那条设计——**内容自适应出帧**。其余模块在路线图里，还没写。

| 模块 | 状态 |
|---|---|
| 内容自适应出帧门控（`src/gating.rs`） | ✅ 已实现，5 个单元测试通过 |
| X11 采集（XDamage 脏区 + XFixes 光标 + GetImage 回读） | ✅ 已实现，实测跑通 |
| 实测对照（gating vs 恒定帧率） | ✅ 见下面「实测」 |
| 编码（NVENC / VAAPI / x264 软编） | ⬜ 未实现，probe 先量采集侧 |
| 信令（socket.io / WSS） | ⬜ 未实现 |
| 传输（ICE/STUN/TURN + RTP/SRTP + 数据通道） | ⬜ 未实现 |
| 输入注入 / 剪贴板 / 文件传输 / 端口映射 | ⬜ 未实现 |

## 为什么先做这个

之前用 ffmpeg `x11grab` + NVENC 做被控端时踩到的两个问题——静态桌面也一直在烧码率、全屏动态时码率飙到 100 Mbps 以上然后塌到 1 fps——根因是**恒定帧率采集**：屏幕不动也照设定帧率一直抓一直编。

官方被控端不是这么干的。引擎里有明显的动静分档帧率和脏区追踪（日志字符串直接印着 `create video frame dropper moving_fps:{} (raw:{}) still_fps:{}`、`dirty_bbox=[…]`），实测静态桌面只发 **3 fps / 231 Kbps**。所以第一个模块就是把这套决策逻辑写出来并且量出来。

## 构建与测试

```bash
cargo build
cargo test                 # 出帧门控的状态机测试（不依赖显示器）
cargo build --release      # 测量用 release：debug 的回读耗时是 release 的 3–4 倍
```

依赖只有 `x11rb`（`damage` / `shm` / `xfixes` 三个 feature），本机 rustc 1.99 实测可编译。

## 实测探针

在真实 X11 桌面上跑，两种模式对照：

```bash
# 内容自适应（本项目的方式）
./target/debug/uu-capture-probe --display :1 --fps 60 --still-fps 3 --duration 20

# 基线（ffmpeg x11grab 那种恒定帧率）
./target/debug/uu-capture-probe --display :1 --fps 60 --duration 20 --baseline
```

每 2 秒一行，输出：桌面状态（moving / still）、实际采集帧率、keepalive 帧率、每秒 XDamage 事件数、每秒光标事件数、脏区占全屏比例、回读 MB/s、单次回读毫秒。结束时打 `SUMMARY` 汇总。

### 实测结果（release 构建）

测量务必用 `cargo build --release`：debug 构建里单次回读 58 ms（release 15–17 ms），会把结论误导成「`GetImage` 上限只有 16 fps」。

**空屏对照**（`Xvfb` 1280x1024，没有任何东西在画，各 10 秒）——这是最能说明问题的一组：

```
gating   t=8.0s  state=still  capture_fps= 0.0  keepalive_fps= 3.0  damage_ev_s= 0.0  read_mb_s=  0.0
baseline t=8.0s  state=moving capture_fps=60.0  keepalive_fps= 0.0  damage_ev_s= 0.0  read_mb_s=299.9
```

屏幕完全不动时，恒定帧率路径仍然每秒读 300 MB 像素、照 60 fps 出帧；门控路径读 **0 MB/s**、只按 `still_fps` 发 3 次 keepalive。回读约 **100×**、出帧 **20×** 的差距，正是官方「静态桌面 3 fps / 两百多 Kbps」的来源。

**真实桌面**（本机 `:1`，2560x1440，各 12–14 秒）：

```
gating   SUMMARY captures=626 capture_fps=44.68 keepalives=0  read_mb_s=628.28 still_share=0.0%
baseline SUMMARY captures=544 capture_fps=45.31 keepalives=0  read_mb_s=637.21 still_share=0.0%
damage_ev_s = 30–43 / s   avg_grab_ms = 15–17
```

两种模式几乎一样，因为这台桌面**一直在重绘**（Hermes 桌面窗口动画 + 光标），门控全程停在 moving 档，从没进入 still——这是正确行为，不是门控失效。真实收益出现在空闲时刻（上一组）。

顺带量到 `GetImage` 同步回读的成本：1280x1024 单次 4.4 ms、2560x1440 单次 15–17 ms，也就是 1440p 下恒定 60 fps 会被回读卡在 ~55 fps。这决定了路线图顺序：**先换 XShm 回读 + 脏区编码**，再谈编码器。

`--no-read` 只测决策不读像素（量门控本身的开销）；`--still-after-ms` 控制静止判定滞后；`--duration` 控制时长。

> 注意：`DISPLAY` 在 agent shell 里是空的，跑之前要
> `export DISPLAY=:1 XAUTHORITY=/run/user/1000/gdm/Xauthority`（或直接 `--display :1`）。

## 设计要点

- **门控与采集分离**：`gating.rs` 是纯决策逻辑（不碰 I/O），可确定性测试；`capture::x11` 只负责「屏幕变了没、哪里变了」。
- **脏区来自 XDamage**：`DamageNotify` 的报告矩形就是官方那个 `dirty_bbox`，顺带拿到脏区大小可以做后面的脏区编码。
- **光标单独轮询**：XDamage 不报光标（光标不是窗口内容），而官方端把光标合成进画面（配置面里有 `cursor` 字段），所以光标位移必须也算脏——用 `XFixesGetCursorPosition` 廉价轮询补上。
- **滞后防抖**：`still_after` 给一个滞后窗口，稀疏变化不会在动静两档之间来回抖。
- **`still_fps = 0`** 表示静止时一个帧都不发，留给「真正零输出」的场景。

## 路线图

1. **编码层**：NVENC（本机 3080 Ti 主路径）→ VAAPI → x264 兜底；能力探测 + 失败回退，对齐官方的 `GetEncodeCapability()` / `encoder requested fallback`。
2. **XShm 快速回读**：`GetImage` 是同步大回读，实测单次 4.4 ms（1280x1024）/ 15–17 ms（2560x1440），换 `XShmGetImage` 省掉这段往返。feature 已在 Cargo.toml 里开好。
3. **脏区编码**：只编脏矩形，而不是每帧全屏。
4. **信令与传输**：socket.io 信令（这条与参照实现一致）+ ICE/DTLS/SRTP；官方数据通道是 KCP+RS-FEC，参照实现是标准 SCTP——先按标准 SCTP 打通，再评估是否要补 KCP。
5. **被控端配置面**：官方可配字段清单见 SPEC；本项目打算先支持 `fps / quality / codec / chroma / cursor`，其余按需。

## 与参照实现的主要差异

| 面 | 官方端 | 参照实现（fork） | 本项目 |
|---|---|---|---|
| 出帧 | 动静分档 + 脏区 | 恒定帧率（XDamage 只用于进程内编码器） | 动静分档 + 脏区，且对整条采集路径生效 |
| 数据通道 | KCP + RS-FEC（另带 QUIC/BBR） | 标准 SCTP | 先 SCTP，后评估 KCP |
| 信令 | socket.io / WSS | socket.io / WSS | socket.io / WSS |
| 编码 | MediaCodec 硬编 + 能力声明 + 回退 | NVENC / VAAPI / 进程内 | NVENC 主路径 + 能力声明 + 回退 |

## 许可

MIT。
