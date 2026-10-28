# linux-uuremote

网易 **UU 远程** 的 Linux **被控端**，从零写的独立实现。

不是 [VallierDev/uu-remote-linux](https://github.com/VallierDev/uu-remote-linux) 的 fork——那个项目当**参照物**读，代码一行没搬。设计依据是从 UU 官方安卓被控端引擎里逆向出来的行为规格（见 [docs/SPEC.md](docs/SPEC.md)），目标是让官方 Windows / macOS / 安卓客户端把这台 Linux 当普通被控端连，行为对齐官方端。

> **命名**：`linux-uuremote` 刻意区别于本机正在跑的参照实现——它的 deb 包与 crate 名都叫 `uu-remote-linux`（本机装的是 0.2.21），而装出来的可执行文件是 `uuremote-server` / `uuremote-viewer`，由 `uuremote-native.service` 拉起。本项目包名 `linux-uuremote`、当前产物 `uu-capture-probe`，将来宿主 daemon 叫 `uuremote-host`，跟它们的包名、crate 名、进程名都不撞。

## 当前状态

第一个能跑的模块是**采集与出帧门控**（`uu-capture-probe`），它已经实测验证了本项目最核心的那条设计——**内容自适应出帧**。其余模块在路线图里，还没写。

| 模块 | 状态 |
|---|---|
| 内容自适应出帧门控（`src/gating.rs`） | ✅ 已实现，6 个单元测试通过 |
| 内容变化检测（`src/content.rs`，粗格采样比对） | ✅ 已实现，7 个单元测试通过；实测能识别出 75.9% 的无效帧 |
| X11 采集（XDamage 脏区 + XFixes 光标 + MIT-SHM 回读） | ✅ 已实现，实测跑通，失败自动降级 `GetImage` |
| 脏矩形抓取（服务端切 tile） | ✅ 已实现，`--dirty`；但合成器桌面给的脏框是整屏，见「实测」 |
| 读回正确性验证（`--verify`，双服务器 PASS） | ✅ 见下面「实测」 |
| 可控测试载荷（`uu-damage-load`） | ✅ 已实现，脏区/内容测量可复现 |
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
cargo test                 # 13 个单元测试：出帧门控状态机 + 内容变化检测（都不依赖显示器）
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

**空屏对照**（`Xvfb` 1280x1024，没有任何东西在画，各 10 秒）：

```
gating   t=8.0s  state=still  capture_fps= 0.0  keepalive_fps= 3.0  damage_ev_s= 0.0  read_mb_s=  0.0
baseline t=8.0s  state=moving capture_fps=60.0  keepalive_fps= 0.0  damage_ev_s= 0.0  read_mb_s=299.9
```

屏幕完全不动时，恒定帧率路径仍然每秒读 300 MB 像素、照 60 fps 出帧；门控路径读 **0 MB/s**、只按 `still_fps` 发 3 次 keepalive。回读约 **100×**、出帧 **20×** 的差距，正是官方「静态桌面 3 fps / 两百多 Kbps」的来源。

但这组要加一条限定，是后来实测才发现的：**`Xvfb` 没有合成器，子窗口绘画根本不会给 root 产生 damage**——用 `uu-damage-load` 在它上面画了 172 个矩形，`damage_ev_s` 仍是 0.0。所以这组证明的是「没有报告就不出帧」，而不是「合成器桌面上屏幕不动就不出帧」。合成器桌面的真实情况见下面那组。

**真实桌面**（本机 `:1`，2560x1440，各 12–14 秒）：

```
gating   SUMMARY captures=626 capture_fps=44.68 keepalives=0  read_mb_s=628.28 still_share=0.0%
baseline SUMMARY captures=544 capture_fps=45.31 keepalives=0  read_mb_s=637.21 still_share=0.0%
damage_ev_s = 30–43 / s   avg_grab_ms = 15–17
```

两种模式几乎一样，因为这台桌面**一直在重绘**（Hermes 桌面窗口动画 + 光标），门控全程停在 moving 档，从没进入 still——这是正确行为，不是门控失效。真实收益出现在空闲时刻（上一组）。

顺带量到读回路径的成本，以及换 MIT-SHM 后的收益（release，各 6 秒）：

| 读回路径 | 1280x1024 (Xvfb) | 2560x1440 (真桌面 `:1`) | 真桌面 baseline 抓到率 |
|---|---|---|---|
| `GetImage`（同步大回读） | 4.4 ms/帧 | 15–17 ms/帧 | 47.9 fps |
| `XShmGetImage`（本项目） | **1.2 ms/帧**（服务端 0.7 + 拷贝 0.5） | **7.4 ms/帧**（服务端 6.1 + 拷贝 1.3） | **60.0 fps** |

也就是 1440p 下省掉 2.2×、Xvfb 下 3.7×，并且 **60 Hz 网格不再被读回卡住**——之前 1440p 恒定 60 fps 只能抓到 48 fps。剩下的 6.1 ms 是 X 服务端把像素写进共享内存的开销，与连接是否走本地无关，只能靠「只抓脏矩形」继续降——但脏区得用对来源，见下面。

### 脏区来源与内容比对（本机 :1，合成器桌面）

这组实测推翻了「脏区直接来自 XDamage」这个原假设：

```
damage 级别 bbox / raw / nonempty 三种都试：
  画面里只有一个 64x48 的矩形在动时，bbox_max_share 全是 1.000（整屏）
非合成服务器（Xvfb）：画了 172 个矩形，damage_ev_s = 0.0 —— 子窗口绘画不动 root
内容比对（32x18 粗格采样）：
  damage 驱动采集 45.1 fps，其中 visual_change_fps 只有 10.9 —— identical_share = 75.9%
```

三条结论：
1. **合成器每帧重画整个 root**，所以 XDamage 给的脏框永远是整屏；靠它做脏区优化在这类桌面上收益为 0。
2. **没有合成器时子窗口绘画完全不报 root damage**，所以「XDamage 判动静」在无合成器的服务器上会一直判静止。
3. **damage 报的「变了」不等于画面变了**：这台桌面 75.9% 的采集帧与上一帧逐像素相同（主要是光标移动与合成器重绘），也就是四分之三的读回和后续编码是白做的。

所以脏区应当**由内容比对推导**，而不是由 XDamage 推导。抓取机制本身已经验证是划算的：服务端切出的 256x256 tile 只要 1.1–5.8 ms，整屏是 12.4 ms，而 tile 内容正确性由 `--verify` 的 `painted_tile` / `tile_matches_full` 两项把关（实测 1.000 / true）。

`--verify` 会证明读回路径确实拿到真实画面，而不是空缓冲：先抓一帧参照、映射一个 override-redirect 纯红测试窗、两条路径各抓一次，然后打判据——`painted_changed`（根窗口抓取确实反映真实画面，合成器下也成立）、`painted_shm` / `painted_tile`（整屏与服务端切出的 tile 里确实是画上去的像素）、`rect_identical`（两条路径逐字节一致）、`tile_matches_full`（tile 与整屏同一区域一致）。本机 `:1` 与 `Xvfb` 实测全通过。

`--no-read` 只测决策不读像素（量门控本身的开销）；`--still-after-ms` 控制静止判定滞后；`--secs` 控制时长；`--dirty` 只抓脏框；`--content-gate` 用内容比对而不是 XDamage 驱动门控；`--damage-level bbox|nonempty|raw` 换 XDamage 报告级别。

`uu-damage-load` 是配套的可控测试载荷：自己开一个 override-redirect 窗口，在里面画一个来回扫动的小矩形，让「脏区多大、画面真变了多少」这类测量可复现，而不是依赖桌面当时在干什么。

```bash
./target/release/uu-damage-load --display :1 --secs 20 --window 320x240 --rect 64x48
```

> 注意：`DISPLAY` 在 agent shell 里是空的，跑之前要
> `export DISPLAY=:1 XAUTHORITY=/run/user/1000/gdm/Xauthority`（或直接 `--display :1`）。

## 设计要点

- **门控与采集分离**：`gating.rs` 与 `content.rs` 是纯决策逻辑（不碰 I/O），可确定性测试；`capture::x11` 只负责「把像素拿回来」。
- **脏区有两个来源**：XDamage 报告矩形（对齐官方 `dirty_bbox` 的字面含义）与内容比对推出的变化区域。实测合成器桌面前者永远是整屏，所以脏区应当由后者推导，前者保留当作提示与降级用。
- **光标单独轮询**：XDamage 不报光标（光标不是窗口内容），而官方端把光标合成进画面（配置面里有 `cursor` 字段），所以光标位移必须也算脏——用 `XFixesGetCursorPosition` 廉价轮询补上。注意我们自己抓的 root 像素里**不含光标**，光标区域要单独抓。
- **滞后防抖**：`still_after` 给一个滞后窗口，稀疏变化不会在动静两档之间来回抖。
- **`still_fps = 0`** 表示静止时一个帧都不发，留给「真正零输出」的场景。

## 路线图

1. **内容推导的脏区**：把 `content.rs` 的粗格比对升级成「哪些格变了 → 合并成脏框 → 只抓那块」。`grab_rect` 与服务端切 tile 已就绪并验证正确，缺的是脏框的来源：合成器桌面 XDamage 永远给整屏，所以收益只能来自内容比对。光标区域要单独抓并合成进画面（我们抓的 root 像素不含光标）。
2. **编码层**：NVENC（本机 3080 Ti 主路径）→ VAAPI → x264 兜底；能力探测 + 可配置黑名单 + 失败回退，对齐官方的 `GetEncodeCapability()` / `encoder requested fallback`。编码器可直接消费共享内存映射，省掉现在 1.3 ms 的拷出。
3. **信令与传输**：socket.io 信令（这条与参照实现一致）+ ICE/DTLS/SRTP；官方数据通道是 KCP+RS-FEC，参照实现是标准 SCTP——先按标准 SCTP 打通，再评估是否要补 KCP。
4. **被控端配置面**：官方可配字段清单见 SPEC；本项目打算先支持 `fps / quality / codec / chroma / cursor`，其余按需。
5. **无合成器的降级路径**：没有合成器时 root damage 不报子窗口变化（实测 `damage_ev_s=0`），要判动静得走 XComposite overlay 或逐窗口 damage。

已完成：**XShm 快速回读**（`ShmCreateSegment` fd 传递 + mmap，失败按会话降级回 `GetImage`）、**服务端切 tile**、**内容变化检测**、**可控测试载荷 `uu-damage-load`**。

## 与参照实现的主要差异

| 面 | 官方端 | 参照实现（fork） | 本项目 |
|---|---|---|---|
| 出帧 | 动静分档 + 脏区 | 恒定帧率（XDamage 只用于进程内编码器） | 动静分档 + 脏区，且对整条采集路径生效 |
| 数据通道 | KCP + RS-FEC（另带 QUIC/BBR） | 标准 SCTP | 先 SCTP，后评估 KCP |
| 信令 | socket.io / WSS | socket.io / WSS | socket.io / WSS |
| 编码 | MediaCodec 硬编 + 能力声明 + 回退 | NVENC / VAAPI / 进程内 | NVENC 主路径 + 能力声明 + 回退 |

## 许可

MIT。
