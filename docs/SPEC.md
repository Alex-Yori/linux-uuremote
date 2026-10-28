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
