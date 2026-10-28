# linux-uuremote

网易 **UU 远程** 的 Linux **被控端**，从零写的独立实现。

不是 [VallierDev/uu-remote-linux](https://github.com/VallierDev/uu-remote-linux) 的 fork——那个项目当**参照物**读，代码一行没搬。设计依据是从 UU 官方安卓被控端引擎里逆向出来的行为规格（见 [docs/SPEC.md](docs/SPEC.md)），目标是让官方 Windows / macOS / 安卓客户端把这台 Linux 当普通被控端连，行为对齐官方端。

> **命名**：`linux-uuremote` 刻意区别于本机正在跑的参照实现——它的 deb 包与 crate 名都叫 `uu-remote-linux`（本机装的是 0.2.21），而装出来的可执行文件是 `uuremote-server` / `uuremote-viewer`，由 `uuremote-native.service` 拉起。本项目包名 `linux-uuremote`、当前产物 `uu-capture-probe`，将来宿主 daemon 叫 `uuremote-host`，跟它们的包名、crate 名、进程名都不撞。

## 当前状态

两个能跑的模块：采集与出帧门控（`uu-capture-probe`，验证了核心设计**内容自适应出帧**），以及连接层（`uu-login` + `uu-cloud-probe`，已在真机与网易云信令**全链路联通**）。传输与编码在路线图里，还没写。

| 模块 | 状态 |
|---|---|
| 内容自适应出帧门控（`src/gating.rs`） | ✅ 已实现，6 个单元测试通过 |
| 内容变化检测（`src/content.rs`，粗格采样比对） | ✅ 已实现，7 个单元测试通过；实测能识别出 75.9% 的无效帧 |
| X11 采集（XDamage 脏区 + XFixes 光标 + MIT-SHM 回读） | ✅ 已实现，实测跑通，失败自动降级 `GetImage` |
| 脏矩形抓取（服务端切 tile） | ✅ 已实现，`--dirty`；但合成器桌面给的脏框是整屏，见「实测」 |
| 光标精灵取回与合成（`src/cursor.rs`） | ✅ 已实现，7 个单元测试；`--verify` 端到端判据双服务器 PASS |
| 读回正确性验证（`--verify`，双服务器 PASS） | ✅ 见下面「实测」 |
| 脏框合并（`src/diff.rs`，块→矩形，可限条数） | ✅ 已实现，12 个单元测试通过 |
| 稀疏采样扫描（`src/scan.rs`，采样格 + 参照帧拼接） | ✅ 已实现，7 个单元测试通过；降级路径 |
| 批量抓取（`grab_tiles`，一次请求多矩形各写 offset） | ✅ 已实现，`--verify` 的 `batch_painted` / `batch_matches_full` 通过 |
| 可控测试载荷（`uu-damage-load`，`--fps` 可调） | ✅ 已实现，脏区/内容测量可复现 |
| 云 API 与登录（`src/api.rs`：签名/设备注册/扫码/JWT csrf） | ✅ 已实现，10 个单元测试；`uu-login` 真机跑通（扫码 + 借用参照实现登录态两条路） |
| 信令（Engine.IO v4 + Socket.IO v5，`src/signaling.rs`） | ✅ 已实现，6 个单元测试；`uu-cloud-probe` 真机联通：建房 → `X-NRD-AUTH` 握手 → room_info ack → bmsg_push |
| wire 结构检查（`src/wire.rs`，protobuf/JSON 只报形状不报内容） | ✅ 已实现，5 个单元测试 |
| 编码（NVENC / VAAPI / x264 软编） | ⬜ 未实现，probe 先量采集侧 |
| 传输（soac offer/answer + ICE + DTLS/SRTP + 数据通道） | ⬜ 未实现，信令侧事件面已摸清（`be-controlled`/`soac`/`bmsg_push`） |
| 输入注入 / 剪贴板 / 文件传输 / 端口映射 | ⬜ 未实现 |

## 为什么先做这个

之前用 ffmpeg `x11grab` + NVENC 做被控端时踩到的两个问题——静态桌面也一直在烧码率、全屏动态时码率飙到 100 Mbps 以上然后塌到 1 fps——根因是**恒定帧率采集**：屏幕不动也照设定帧率一直抓一直编。

官方被控端不是这么干的。引擎里有明显的动静分档帧率和脏区追踪（日志字符串直接印着 `create video frame dropper moving_fps:{} (raw:{}) still_fps:{}`、`dirty_bbox=[…]`），实测静态桌面只发 **3 fps / 231 Kbps**。所以第一个模块就是把这套决策逻辑写出来并且量出来。

## 构建与测试

```bash
cargo build
cargo test                 # 42 个单元测试：出帧门控 + 内容检测 + 块比对合并 + 采样扫描 + 光标合成（都不依赖显示器）
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

### 光标必须单独抓再合成进画面（本机 :1 与 Xvfb，2026-10-28）

抓 root 像素**不含光标**（X 光标由服务端绘制在画面之上），而官方把光标合成进视频，所以本项目自己抓精灵再合成：`XFixesGetCursorImage` 拿 ARGB 精灵 + `QueryPointer` 拿热点位置，`src/cursor.rs` 做 source-over 混合（纯逻辑，7 个单测）。

实测（`--cpu-diff --cursor`，载荷 = 64x48 矩形 30 fps）：

```
精灵 24x24（mutter）/ 16x16（Xvfb），取回 0.04–0.06 ms，混合 0.005–0.006 ms/帧
光标区域编码器输入 0.09–0.11 MB/s（内容脏区是 0.9–1.7 MB/s）—— 代价可忽略
mutter 的 cursor_serial 每秒换 ~60 次（合成器每帧重建光标对象），按 serial 去重后仍是
~60 次/s 重取精灵，每次 0.05 ms，接受；不重取会丢动画光标
参考帧保持「无光标」：光标只合成进发出的那份拷贝，否则光标像素永远被当成内容脏区
```

`--verify` 新增端到端判据：把**真实服务端精灵**混合到一个黑画布上，变化像素必须 >0 且不超过 alpha 掩码份额（`comp_ratio <= expected + 0.05`）。本机 `:1` 实测 `comp_ratio=0.245/0.510`（暗半透明像素叠在黑上仍读 0，所以判据是「有变化且不超掩码」而不是逐像素相等）；Xvfb 默认光标同样通过。

### 脏区该怎么算：读回不是瓶颈，编码输入才是（本机 :1，2560x1440）

用 `uu-damage-load`（64x48 矩形以 30 fps 来回扫）当载荷，把两条候选路同机对比（XDamage 那条已在上面证明无效）：

```
--cpu-diff   整屏 SHM 读回 + 逐块精确比对
  diff_block 16x16：读回 5.4-6.1 ms，比对 1.2 ms，循环 6.6-9.0 ms，抓到率 92-131 fps
  diff_block 32x32：比对 1.67 ms     8x8：比对 1.59 ms   → 块大小几乎不影响比对代价
  dirty_share = 0.0003（16x16）/ 0.0005（32x32）/ 0.0002（8x8）
  编码器输入 0.39 MB/s，整屏直喂是 1309 MB/s —— 差 3300 倍
--incremental  稀疏采样扫描 + 只抓变化矩形（16x9 格 / 16x16 采样）
  scan_ms 12.9-14.5，scan_fps 60-69，read_mb_s 10.7（整屏读回是 1300）
  代价：missed_share 最多 0.0031 —— 采样漏掉的像素要靠 1s 一次整屏重同步补
```

一次请求的服务端开销约 0.08 ms，**跟矩形多大几乎无关，只跟矩形条数有关**：

| 采样形状 | 条数 | scan_ms | scan_fps |
|---|---|---|---|
| 32x18 格 / 8x8 采样 | 576 | 43.6 | 18 |
| 16x9 格 / 16x16 采样 | 144 | 12.9 | 63 |
| 8x6 格 / 32x32 采样 | 48 | 3.5 | 171 |
| 1x18 整宽条 / 2560x8 | 18 | 2.3 | 219 |

三条结论：
1. **1440p 下读回不是瓶颈**：整屏 SHM 读回 6.1 ms，加上逐块精确比对 1.2 ms，单帧 8 ms，够 120 fps 还有余量。稀疏采样省的只是读回字节（10 MB/s vs 1300 MB/s），却要付「漏检」的代价。
2. **真正省的是编码输入**：精确比对能把编码器输入从 1309 MB/s 压到 0.39 MB/s，而这一步不需要任何采样近似。
3. **所以默认管线是「整屏 SHM 读回 + 逐块精确比对 + 合并成脏框 → 只把脏框喂编码器」**；`--incremental` 的稀疏采样留作读回昂贵时的降级路径（远程 X、4K、或共享内存带宽被编码器占满时）。比对块取 **16x16**：代价最低且与 H.264 宏块对齐。

`--verify` 会证明读回路径确实拿到真实画面，而不是空缓冲：先抓一帧参照、映射一个 override-redirect 纯红测试窗、两条路径各抓一次，然后打判据——`painted_changed`（根窗口抓取确实反映真实画面，合成器下也成立）、`painted_shm` / `painted_tile`（整屏与服务端切出的 tile 里确实是画上去的像素）、`rect_identical`（两条路径逐字节一致）、`tile_matches_full`（tile 与整屏同一区域一致）、`batch_painted`（一次请求抓多条矩形时，画上去的那条必须整条是红的，这才证明每条矩形各写自己的 offset）与 `batch_matches_full`。本机 `:1` 与 `Xvfb` 实测全通过。

`--no-read` 只测决策不读像素（量门控本身的开销）；`--still-after-ms` 控制静止判定滞后；`--secs` 控制时长；`--dirty` 只抓脏框；`--content-gate` 用内容比对而不是 XDamage 驱动门控；`--damage-level bbox|nonempty|raw` 换 XDamage 报告级别；`--cpu-diff --diff-block 16x16` 量「整屏读回 + 精确比对」这条路；`--cpu-diff --cursor` 再叠光标取回与合成；`--incremental --sample-grid 16x9 --sample-tile 16x16 --resync-ms 1000 --max-dirty-rects 8` 量稀疏采样这条路。

`uu-damage-load` 是配套的可控测试载荷：自己开一个 override-redirect 窗口，在里面画一个来回扫动的小矩形，让「脏区多大、画面真变了多少」这类测量可复现，而不是依赖桌面当时在干什么。

```bash
./target/release/uu-damage-load --display :1 --secs 20 --window 320x240 --rect 64x48 --fps 30
```

> 注意：`DISPLAY` 在 agent shell 里是空的，跑之前要
> `export DISPLAY=:1 XAUTHORITY=/run/user/1000/gdm/Xauthority`（或直接 `--display :1`）。

### 连接层真机联通（2026-10-28）

连接层按参照实现（本机在跑、云端连接可用的 0.2.x 实现）的**线上行为**重写，
不是按通用 socket.io 文档猜的。三处与直觉不同的事实，全部实测印证：

1. **房间 token 走 HTTP 头 `X-NRD-AUTH`**（裸 token，无 `Bearer` 前缀）。
   按教科书把 token 放进命名空间 CONNECT payload 实测被 **403** 拒掉。
2. **URL 是 `?transport=websocket&EIO=4`**（Engine.IO v4），不是 EIO=3。
3. **握手方向**：服务器发 open `'0'`（带 pingInterval/pingTimeout）后，客户端发
   `'40'`（不带 payload），服务器回 `'40{}'` 即 CONNECTED；带 ack 的请求帧是
   `42<ack_id>["event"]`，服务器用 `43<ack_id>[data]` 回。心跳是服务器发 `'2'`、
   客户端回 `'3'`；`pingInterval+pingTimeout` 窗口内没收到 ping 必须判死
   （半开 TCP 会让宿主静默离线，参照实现有同样的 watchdog）。

被控端事件面（参照实现源码 + 安卓 Java 包 jadx 双重印证）：服务器推
`be-controlled`（常为 binary `'45'` + ConnectOptions protobuf 附件）、
`soac`（WebRTC offer/answer，ICE 走 SDP 交换而不是独立 candidate 事件）、
`bmsg_push`（房间成员）、`released`/`left`；客户端主动发 `room_info`（带 ack id）
与 `refresh_reconnect_key`（官方 30 s/300 s/失败 30 s 重试的 token 刷新环）。

真机验收（`uu-cloud-probe`，凭证借用参照实现的登录态）：

```
1) 建房…
   signaling_server = wss://sig-3303-e.nrd.nie.163.com/
2) WSS 握手（X-NRD-AUTH）…
   CONNECT_ACK ✓（命名空间已连）
3) room_info…
   room_info ack ✓：[1x{iceServers:[0],publisher:{9 keys},reconnect_buffer:15,room_id:str(24),…}]
   —— 信令链路已通 ——
4) 等客户端拉流（be-controlled / soac，≤20s）…
   bmsg_push：{data:{alias:str(5),controllable:true,…},type:str(19)}
   leave: {ice_id:str(0),ntestraceid:str(69)}
   超时：没有客户端来拉流。信令通、登记就位——拿 App 扫这台设备即会触发 be-controlled。
```

所有输出只报**形状**（字段名、类型、长度、小整数），token/ticket/SDP 内容永不打印
（`wire.rs` 的 `json_shape` / `wire_shape`）。`uu-login` 支持两条路：终端半块字符
二维码扫码（官方 `status_query_ticket` 长轮询、过期按官方策略停），或只读借用
参照实现的 `~/.config/uuremote/config.json` 登录态；写入永远只进
`~/.config/linux-uuremote/config.json`（0600）。

## 设计要点

- **门控与采集分离**：`gating.rs` 与 `content.rs` 是纯决策逻辑（不碰 I/O），可确定性测试；`capture::x11` 只负责「把像素拿回来」。
- **脏区有两个来源**：XDamage 报告矩形（对齐官方 `dirty_bbox` 的字面含义）与内容比对推出的变化区域。实测合成器桌面前者永远是整屏，所以脏区应当由后者推导，前者保留当作提示与降级用。
- **光标单独抓 + 合成**：XDamage 不报光标（光标不是窗口内容），而官方端把光标合成进画面（配置面里有 `cursor` 字段）。位置用 `QueryPointer` 轮询（位移算脏），精灵用 `XFixesGetCursorImage` 取（按 `cursor_serial` 去重），发帧前 source-over 合成进画面拷贝；参考帧保持无光标。
- **滞后防抖**：`still_after` 给一个滞后窗口，稀疏变化不会在动静两档之间来回抖。
- **`still_fps = 0`** 表示静止时一个帧都不发，留给「真正零输出」的场景。

## 路线图

1. **编码层**（脏区与光标都已定型）：NVENC（本机 3080 Ti 主路径）→ VAAPI → x264 兜底；能力探测 + 可配置黑名单 + 失败回退，对齐官方 `GetEncodeCapability()` / `encoder requested fallback`。编码器直接吃 `merge_blocks` 出的脏框 + 光标精灵矩形，并可消费共享内存映射省掉 1.3 ms 拷出。
2. **传输**：信令已通（见上），下一步是 `soac` 的 offer/answer（SDP）+ ICE + DTLS/SRTP；官方数据通道是 KCP+RS-FEC，参照实现是标准 SCTP——先按标准 SCTP 打通，再评估是否要补 KCP。
3. **被控端配置面**：官方可配字段清单见 SPEC；本项目打算先支持 `fps / quality / codec / chroma / cursor`，其余按需。
4. **无合成器的降级路径**：没有合成器时 root damage 不报子窗口变化（实测 `damage_ev_s=0`），要判动静得走 XComposite overlay 或逐窗口 damage。

已完成：**XShm 快速回读**（`ShmCreateSegment` fd 传递 + mmap，失败按会话降级回 `GetImage`）、**服务端切 tile 与批量多矩形抓取**、**逐块精确比对与脏框合并（`diff.rs`）**、**稀疏采样降级路径（`scan.rs`）**、**内容变化检测**、**光标取回与合成（`cursor.rs`）**、**可控测试载荷 `uu-damage-load`**、**连接层（`api.rs`/`signaling.rs`/`config.rs`/`wire.rs`，真机联通）**。

## 与参照实现的主要差异

| 面 | 官方端 | 参照实现（fork） | 本项目 |
|---|---|---|---|
| 出帧 | 动静分档 + 脏区 | 恒定帧率（XDamage 只用于进程内编码器） | 动静分档 + 脏区，且对整条采集路径生效 |
| 数据通道 | KCP + RS-FEC（另带 QUIC/BBR） | 标准 SCTP | 先 SCTP，后评估 KCP |
| 信令 | socket.io / WSS | socket.io / WSS | socket.io / WSS |
| 编码 | MediaCodec 硬编 + 能力声明 + 回退 | NVENC / VAAPI / 进程内 | NVENC 主路径 + 能力声明 + 回退 |

## 许可

MIT。
