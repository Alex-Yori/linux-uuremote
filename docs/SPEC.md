# UU 远程被控端行为规格（静态逆向初版）

本文是本项目的设计依据：**要让 Linux 被控端行为对齐官方**，使官方 Windows / macOS / 安卓客户端能把这台 Linux 当普通被控端连。

证据来源：官方安卓手机版 APK V4.44.0（`com.netease.uure.163.com` 系产品，包名 `com.netease.uuremote`，code 444000，sha256 `d7dc65be…`）的 jadx 反编译 + `lib/arm64-v8a/libstreamer.so`（16 MB，日志格式串未 strip）+ 开源参照 `VallierDev/uu-remote-linux`。

所有结论都是**静态观察**，未经动态验证；未验证项标 `[待验]`。逆向工件在 `~/rea-work/uuremote/`（APK、jadx 输出、JNI 表、模块地图、fork 参照）。

## 1. 引擎形态

- Java 层是薄包装；被控端与控制器共用一颗 native 引擎（`libstreamer.so`，源码树名 `nrd-streamer`，即**魔改 WebRTC**）。安卓自身也是官方被控端（MediaProjection 采集），所以这颗 `.so` 就是「官方被控端」的可移植规格，不必先逆 Windows。
- 符号表：JNI 导出 53 个（`Java_com_remote_streamer_*`），另有内部命名空间导出 `ExchangeControlledInterface` / `ExchangeControllerInterface` 和 `streamer::device::*`（`SurfaceTextureOnFrameAvailable`、`MediaProjectionStopped`、`DisplayRotationChanged`、`VirtualDisplayPaused/Resumed/Stopped`、`OnGetRecordData/OnGetPlayoutData`）——被控端边界就在 `ExchangeControlledInterface`。
- 提取到 **889 个源文件路径**，等于一份模块地图：自研层 `streamer/src/{device,renderer}` + `vcpkg`（`ngtcp2 v1.10.0`、`api/data_channel_interface.h`）。

## 2. 信令

- **socket.io over WebSocket**：`.so` 里有 `/socket.io/`、`h[{}] socket.io connected send remain packet {}`。参照实现用 `tokio-tungstenite` + socket.io，**这条已对齐** ✅。
- 信令服务器地址**不硬编码在引擎里**，由上层注入：`StreamerRoomConfig{ signalServers[], token, appData, timeout, signalReconnectDelay, reportUrl, reportToken, reportServerAddress }`（产品 API 属 `api.nrd.nie.163.com` 家族）。引擎侧日志：`create room signal server size :{} token length: {}`、`insert signal connection header {}:{}`、`signal connection connecting timeout {}ms`、`refresh reconnect key failed`。
- 通道语义（被控端 JNI，18 个）：`Initialize / CreateRoom / StartVideoCapture / StartAudioCapture / SendSignalData / SendControlData / SendTextData / SendBinaryData / SendFileData / Stop* / ClearRoom / DestroyRoom / CheckAlive / GetConnectionStats / GetEncodeCapability / GetStreamerVersion / Finalize`。四条数据道：**Signal / Control / Text / Binary**（+ File）。
- 回调面（`StreamerControlledCallback`）：`onNewControl / onRoomState / onConnectionState / onReceiveRpcRequest / onReceiveRpcResponse / onReceiveDataChannel / onReceiveControlData / onReceiveTextData / onReceiveBinaryData / onReceiveFileData / onQosStats(StatsInfo) / onControlledPush / onEventReport / onCaptureFailed / onConnectionErrorCode`。

## 3. 传输层（与参照实现的真实分歧）

官方：
- 媒体走标准 RTP/SRTP（WebRTC video/audio 模块齐全：Opus/G722/G711/RED/CNG、simulcast、`WebRTC-LinkPressureDetection`）。
- **数据通道不是 SCTP**：被换成 **KCP + Reed-Solomon FEC** —— `webrtc/media/sctp/dckcp_transport.cc`、`dckcp_fec_generator.cc`、`dckcp_fec_receiver.cc`，指标名 `dc_msg_bytes_proto_kcp_{recv,sent}`、`dc_pro_kcp_rsfec_{recv,send,valid}_bytes`。降级路径有明文：`cm256 init failed; disabling FEC entirely. NACK retransmission still operates.`
- 另带 **ngtcp2 1.10.0 = QUIC/HTTP3 + BBR**（`bbr restart from idle`、QUIC 帧 JSON `{"frame_type":"stream"/"datagram"/"max_data"/"path_challenge"…}`）。用途 `[待验]`（推测用于文件传输/端口映射或备用路径）。

参照实现：`webrtc` crate 0.10 = **标准 SCTP over DTLS**，无 KCP、无 QUIC。
含义：官方↔官方走 KCP，官方↔参照实现走标准 SCTP —— 互操作能成，但**拥塞/FEC 语义不同**，这是「Linux 被控端在某些网络下表现不如官方」最可能的结构性原因。`[待验：抓包确认官方客户端连非官方宿主时协商的是哪条]`

## 4. 视频行为（本项目第一个模块的依据）

官方被控端是**内容自适应出帧**，不是恒定帧率：

- `create video frame dropper moving_fps:{} (raw:{}) still_fps:{}` → 抓帧率 `raw` 与输出帧率分两档：**动静分档**（静止降到 `still_fps`，活动用 `moving_fps`）。这解释了实测「官方静态桌面只发 3 fps」，而 ffmpeg 路径按设定帧率一直抓一直编。
- `[VideoPreprocessSource] … dirty_bbox=[{},{},{},{}], quality={}, frame_type={}, raw_size={}x{}` → 预处理阶段带**脏矩形**与 `frame_type`。
- WebRTC 自适应仍在：`bw_adapted_fps`、`cpu_adapted_fps`、`Adjusting allocation, fps = `、`Removing resolution cap due to no consistent animation detection`（动画检测才降分辨率）、`Downgrade counts: fps: {…}`、`not requesting refresh frame because of soon happening idle repeat`。
- 手动切帧率痕迹：`after_manual_switch_fps`、`after_switch_1_fps`、`after_switch_2_fps`、`default display fps :{}`。
- 编码走**硬件**：`libmediandk.so` + `AMediaCodec_*`（含 `setInputSurface`、`createPersistentInputSurface`），并支持 `av1`；`GetEncodeCapability()` 向客户端声明能力；失败有回退：`encoder requested fallback, implementation={}, … frame_type={}, error={}`。

被控端可被客户端配置的字段（一条日志全列出，等于官方配置面清单）：
`hdr / cursor / frame_quality / capture_target / chroma_sampling / source_id / frame_type / watermark / pixel_format / color_encoding / hdr_meta`。
参照实现只覆盖 `codec / fps / quality / custom_bps / chroma`（`src/capture_setting.rs`）——`hdr / watermark / pixel_format / color_encoding / capture_target` 属**未实现面**（可能是死字段，也可能官方客户端真的会发）`[待验]`。

## 5. 与参照实现对照

| 面 | 官方 | 参照实现（fork 0.2.21） | 本项目 |
|---|---|---|---|
| 信令 | socket.io/WSS，服务器由上层注入 | socket.io/WSS | socket.io/WSS |
| 静止不出帧 | frame dropper `still_fps` + dirty_bbox | XDamage 只用于**进程内**编码器 | 门控作用于整条采集路径 ✅ 已实现 |
| 数据通道 | KCP + RS-FEC | 标准 SCTP | 先 SCTP，后评估 KCP |
| QUIC/BBR | ngtcp2 1.10 + BBR | 无 | 未实现 |
| 光标 | `cursor` 配置字段 | XFixes 直抓 | QueryPointer + XFixes ✅ |
| 编码 | AMediaCodec 硬编 + av1 + 能力声明 + fallback | NVENC/VAAPI/inproc | NVENC 主路径 + 能力声明 + fallback |
| HDR/水印/像素格式/色彩编码 | 可配置 | 未实现 | 未实现 `[待验]` |

## 6. 下一步

1. IDA 分析 `libstreamer.so`：目标函数 = frame dropper（`still_fps` 决策）、`ExchangeControlledInterface`、KCP transport 建包、capture→preprocess 链。
2. **动态验证**（需手机 + adb/frida；或 Windows 客户端 + 本机宿主）：hook `SendSignalData` / `onReceiveControlData` / `onReceiveRpcRequest`，拿真实 JSON。宿主与被控在同一局域网时协议两侧都能抓。
3. RPC 字段号对齐：参照实现已知「field 1 = fps 选项索引（1=30, 2=60）、field 18 = fpsCount」，官方对应结构从 RPC 报文确认。
4. 若确认官方客户端连非官方宿主时用的是标准 SCTP，则 KCP 差异不阻塞；否则考虑补 KCP 数据道。

## 7. Windows 包证据（2026-10-28 补充）

官方 Windows 包已下载并**在 Linux 上解包**（不需要 Windows 机器）：

```
https://api.nrd.nie.163.com/api/v1/release/dl/1?channel=gwqd
→ 92,252,208 B，PE32 NSIS 自解压，sha256 4f1b86d768478ddb…
7z x 直接解开（解出的载荷在 ~/rea-work/uuremote/win/）
```

被控端侧载荷（按用途分类）：

| 载荷 | 作用 |
|---|---|
| `GameViewerServer.exe` 7.4 MB | 被控端宿主进程 |
| `GameViewerService.exe` 2.2 MB | 系统服务（常驻、提权） |
| `GameViewer.exe` 29 MB | 主程序（控制端 + 被控端 UI，Qt5） |
| `streamer.dll` 5.9 MB | 与安卓 `libstreamer.so` 同族的引擎（`nrd-streamer`） |
| `StreamerCodecDetector.exe` 1.4 MB | 独立进程做**编码器能力探测** |
| `drivers/GameViewerIddDriver` | **虚拟显示器**（Windows Indirect Display Driver） |
| `drivers/uuycinput` + `uuycinputmf` | 虚拟输入注入 |
| `drivers/nevaudio` + `Nevaudiosdk.dll` | 虚拟音频（被控端把声音送进链路） |
| `drivers/ViGEmBus` | 虚拟手柄（把远端输入变成 XInput 设备） |
| `nrd_renderer_ipc.dll` | 渲染进程间通信 |
| `nemu-api.dll` / `remote-nemu-api.dll` | 模拟器（MuMu）侧集成 |
| `uuyc-mux.exe` / `uuyc-cli.exe` / `Upgrade.exe` / `PsExec64.exe` | 多路复用、CLI、升级、提权执行 |

`streamer-config.json`（解出来只有 763 B，是一条**按设备能力开关**的策略）：

```json
{
  "disable_h265_codec": [{
    "enable": true, "roles": ["controller"], "platforms": ["windows"],
    "graphic_cards": ["GTX\\s*6","GT\\s*6","GTX\\s*7","GT\\s*7","GTX\\s*8","GT\\s*8","GTX\\s*9","GT\\s*9","NVIDIA\\s*QUADRO"]
  }],
  "set_max_decoder_cap": [{
    "enable": true, "platforms": ["android"], "brands": ["Lenovo"], "models": ["TB371FC"]
  }]
}
```

含义：官方把「哪些设备不许用 H.265 / 要限解码能力」写成随包下发的正则黑名单，而不是硬编码。本项目的编码能力声明应当走同一条路：**探测 + 可配置黑名单 + 回退**，而不是假定 NVENC 永远可用。

另外 Windows 被控端靠**虚拟显示器驱动**（Idd）取画面，而 Linux 侧我们用 XDamage 直接抓真实桌面——所以官方那些「分辨率/帧率随时切换」「多显示器」的能力，Linux 要等价就得靠 XRandR（改分辨率）或另开虚拟输出，这是两端结构性差异，不是实现质量问题。


## 8. 采集侧本机实测（2026-10-28 补充）

用 `uu-capture-probe` 在本机量出来的数，用来确认「官方行为能不能在 Linux 采集侧复现」。

**空屏对照**（`Xvfb` 1280x1024，无人绘画，各 10 秒，release 构建）：

| 模式 | 出帧 | 回读 |
|---|---|---|
| 门控（本项目） | 0 fps 正常帧 + 3 fps keepalive | **0 MB/s** |
| 恒定帧率（ffmpeg x11grab 式） | 60 fps | **299.9 MB/s** |

也就是「静态桌面只发 3 fps」这条官方行为，靠 XDamage 判动静 + 静止档出帧，在 Linux 上等价成立；差距约回读 100×、出帧 20×。

**读回路径成本**（决定路线图顺序）：

| 路径 | 1280x1024 | 2560x1440 | 真桌面恒定 60 fps 时的抓到率 |
|---|---|---|---|
| `GetImage` | 4.4 ms | 15–17 ms | 47.9 fps（被读回卡住） |
| `XShmGetImage` | 1.2 ms | 7.4 ms | 60.0 fps |

结论：1440p 要跑满官方上限 60 fps，XShm 是必需的；剩余 6.1 ms 是服务端把整屏像素写进共享内存，只能靠**只抓脏矩形**继续降。

**读回正确性**（`--verify`，本机 `:1` GNOME 与 `Xvfb` 各一次，三项判据全 1.000）：
根窗口抓取在合成器（mutter）下确实反映真实画面（`painted_changed=1.0`）；MIT-SHM 的共享映射里确实是画上去的像素，且与 `GetImage` 逐字节一致。

实现约束（踩过才写下来的）：
- 像素格式护栏必须读 `setup().pixmap_formats`：非 32 bpp packed 会整幅撕裂，宁可报错。
- MIT-SHM 用 `ShmCreateSegment`（fd 传递，`minor >= 1`）而非 SysV `ShmAttach`，Xorg 与 Xvfb 都工作，且不需要进程持有 SysV 段。
- 验证用的测试窗必须 `override_redirect`，否则窗口管理器会把它挪走，量出来是假失败。
**脏区来源（2026-10-28 追加实测，推翻原假设）**

| 场景 | 实测 | 结论 |
|---|---|---|
| 合成器桌面（GNOME/mutter `:1`），画面里只有一个 64x48 矩形在动 | `bbox` / `raw` / `nonempty` 三种 XDamage 报告级别的 `bbox_max_share` 全是 1.000 | 合成器每帧重画整个 root，XDamage 脏框永远是整屏，靠它做脏区优化收益为 0 |
| 非合成服务器（`Xvfb`），画了 172 个矩形 | `damage_ev_s = 0.0`，门控停在 `still` | 子窗口绘画不给 root 产生 damage；无合成器时「XDamage 判动静」会一直判静止 |
| 合成器桌面，damage 驱动采集 | 采集 45.1 fps，其中 `visual_change_fps` 只有 10.9，`identical_share = 75.9%` | 「damage 报了」≠「画面变了」，四分之三的读回与编码是白做的 |

因此本项目的脏区**由内容比对推导**，XDamage 只当提示与降级依据。服务端切 tile 的正确性与成本已单独验证：256x256 tile 1.1–5.8 ms，整屏 12.4 ms，`--verify` 的 `painted_tile` / `tile_matches_full` 全通过，批量多矩形一次请求的 `batch_painted` / `batch_matches_full` 也全通过（证明每条矩形各写自己的 offset）。

**脏区来源三选一（2026-10-28 追加实测，载荷 = `uu-damage-load` 64x48 矩形 30 fps 扫动，:1 2560x1440）**

| 方案 | 每帧成本 | 抓到率 | 读回 MB/s | 编码器输入 |
|---|---|---|---|---|
| 整屏读回 + 逐块精确比对（16x16） | 读回 5.4–6.1 ms + 比对 1.2 ms | 92–131 fps | 1300 | **0.39 MB/s**（整屏直喂 1309 MB/s） |
| 同上，块 32x32 / 8x8 | 比对 1.67 / 1.59 ms | 87–95 fps | 1230–1470 | 0.62 / 0.30 MB/s |
| 稀疏采样 + 只抓变化矩形（16x9 格 / 16x16 采样） | 扫描 12.9–14.5 ms | 60–69 fps | 10.7 | 0.39 MB/s，但 `missed_share` 最高 0.0031 |

单次抓取请求的服务端开销约 0.08 ms 且**与矩形大小几乎无关，只与矩形条数有关**（576 条 43.6 ms / 144 条 12.9 ms / 48 条 3.5 ms / 18 条整宽条 2.3 ms），所以稀疏采样想快就得少条数，少条数就漏检。

定型的默认管线：**整屏 SHM 读回 + 逐块精确比对（16x16，与 H.264 宏块对齐）+ `merge_blocks` 合并 → 只把脏框喂编码器**。理由：1440p 下读回加比对只要 8 ms，够 120 fps，而省下来的 3300 倍编码输入不需要任何采样近似；稀疏采样（`scan.rs` + `grab_tiles`）留作读回昂贵时的降级路径（远程 X、4K、或共享内存带宽被编码器占满）。

另外要记一条结构性差异：**我们抓的 root 像素里不含光标**（X 光标由服务端单独绘制），而官方画面把光标合成进视频。所以光标位移必须既算脏、又要单独抓光标区域合成——只靠 root 抓取会丢光标。

**光标合成实测（2026-10-28 补充，`--cpu-diff --cursor`）**

| 项 | 实测 | 结论 |
|---|---|---|
| `XFixesGetCursorImage` | mutter 24x24 / Xvfb 16x16，取回 0.04–0.06 ms | 精灵按 `cursor_serial` 缓存、serial 变才重取 |
| mutter serial churn | ~60 次/s（合成器每帧重建光标对象） | 重取代价 ~3 ms/s，接受；不重取会丢动画光标 |
| 混合（source-over） | 0.005–0.006 ms/帧 | 光标区域编码器输入 0.09–0.11 MB/s，相对内容脏区 0.9–1.7 MB/s 可忽略 |
| 参考帧 | 保持无光标，光标只合成进发出那份拷贝 | 否则光标像素永远被误判为内容脏区 |

`--verify` 的端到端判据：真实服务端精灵混合到黑画布，变化像素 >0 且 ≤ alpha 掩码份额 +0.05；`:1` `0.245/0.510`、Xvfb `0.234/0.688` 通过（暗半透明像素叠黑仍读 0，故不逐像素相等）。
