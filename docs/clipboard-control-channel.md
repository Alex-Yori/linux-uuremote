# 剪贴板（CONTROL 数据通道）实现规格

来源：`fork-uu-remote-linux/src/{clipboard.rs,clipboard_protocol.rs,clipboard_rich.rs,webrtc.rs}`。
通道：`CONTROL_DATA_CHANNEL`（代码同时接受 `TEXT_DATA_CHANNEL`；rich 路径的注释与实测都在 CONTROL 上）。
所有帧都是 `GvPb_Message` 信封（protobuf）：`1` seq(varint)、`2` timestamp(varint, 秒)、`which`(bytes, 即字段号=消息类型)、`3` SimpleAction。剪贴板用 which=17/21/22。

## 1. which=17 ClipboardChange —— 客户端 → 被控端，单向
- 子消息：`1` content_id (uint32/varint)、`2` text (string/bytes UTF-8)、`3` more (bool, 0|1)。
- 分片：单帧 text ≤ 8192 字节（按 char 边界切，`chunks()`），同一 `content_id` 连续发送；`more=1` 表示后续还有。
- 重组：按 content_id 拼接，**同 id 且距首片 <5s** 才续接，否则丢弃重开；重复字段号、非 UTF-8、字段类型不符一律丢弃并清空挂起状态。
- 总长上限 `MAX_TEXT = 30*1024`（留出 UTF-16 + 信封余量，压在 webrtc 65535 读缓冲以下）；单帧上限 `MAX_TEXT+256`。
- 被控端**从不主动发 17**（全仓只有一处接收 17）。⇒ 若我们只做被控端，17 只需解析。

## 2. which=21 / 22 Clipboard RPC —— 双向（被控端 ↔ 客户端）
RPC 外壳：请求侧 `1` header{`1` request_id} + `9` clipboard；应答侧 `1` header + `5` clipboard。

### 21（被控端发起：offer / ask / block）
- `9{1: FormatList}`：offer，`FormatList{1..n: Format}`，`Format{1: format_id, 2: name}`。被控端只 offer `id=13 "CF_UNICODETEXT"`（Windows 客户端）；Mac 主机用 `id=0 "public.utf8-plain-text"`，被控端也能接受。
- `9{2: Ask}`：`Ask{1: format_id, 2: key(string ≤256), 3: name}`，key 形如 `%08x-0000-4000-8000-%012x`。
- `9{3: Block}`：`Block{1: key, 2: index(varint), 3: data(bytes)}`，块大小 `0x20000`(128 KiB)，总数据 `MAX_BYTES = 2*MAX_TEXT+2`。
- 方向：被控端本地剪贴板变化时发 21 offer（`webrtc.rs:874-883`），也在拿到 Ask 后发 21 blocks。

### 22（应答，双向）
- `5{1: FormatList}`：对 offer 的格式确认（可为空 body）。
- `5{2: Confirm}`：`Confirm{1: status(1=ok, 2=fail), 2: key, 3: block_count}`。
- `5{3: BlockAck}`：`BlockAck{1: key, 2: index, 3: ok=1}`。
- 块序号两种约定都接受：0..count-1 或 1..count（原生提供方从 1 编号）；缺块则永不完成。

### 21 上的纯文本旁路（legacy text RPC）
- 请求：`1` header + `10` ClipboardChange（`1` format_id=1（官方 ClipboardMobileManager 文本格式）、`2` text、`3` more）。
- 应答：`1` header + `6` TextChangeResponse{`1` err=1 (CB_SUCCEEDED)}，即尾部字节 `50 02 08 01`。
- 被控端收到 21 且含 `10` 时：写入本地剪贴板并回 22 应答；含 `9` 时走上面的格式协商。

## 3. 实现最小集（我们这侧）
1. CONTROL 收帧后按 which 分派：17 → 文本重组落剪贴板；21 → 解析 `9`/`10`，回 22。
2. 本地剪贴板变化 → 发 21 offer（`9{1}`），等客户端 22/21 再来取数据。
3. 上限：单帧 30 KiB+256，文本总长 30 KiB，块 128 KiB，块数 ≤ `MAX_BYTES/0x20000+2`。

## UNCONFIRMED
- which=21/22 的官方消息名（代码只叫 "Clipboard RPC"；17 有注释写明 ClipboardChange）。
- 17 是否也出现在 `BINARY_DATA_CHANNEL`（代码只分派 CONTROL/TEXT 两个通道）。
- 分片是否必须 8192（代码自选值，非协议常量）。
- Windows 客户端是否会同时用 17 与 21 双路推送。
