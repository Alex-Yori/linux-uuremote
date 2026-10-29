//! CONTROL 数据通道上的输入与控制消息。
//!
//! 线上事实（从官方客户端抓到的报文反推，见 SPEC §10）：
//! - 控制消息是 **UTF-8 JSON**，可能以 Socket.IO/SCTP 的 **binary** 帧送达，也可能 text，两种都要吃。
//! - 键与按键标识是 **Windows 虚拟键码（VK）**，不是 X11 keycode；坐标归一化到源屏 0..1。
//! - 鼠标按键是 **位掩码**（bit0 左 / bit1 右 / bit2 中），不是 X11 按钮序号。
//! - 另有一条 protobuf `SimpleAction` 控制面（`echo_request` 等），用 `wire` 解码，宿主按需回 `echo_response`。
//!
//! 本模块只做**语义解释**，不碰 X11：坐标/键码的 Linux 翻译在 `input`。

use serde_json::Value;

use crate::initialization::envelope;
use crate::wire::{WireField, WireValue, decode_wire, nested, varint_field};

/// 一条输入动作（已经和平台无关）。
#[derive(Debug, Clone, PartialEq)]
pub enum ControlInput {
    Keyboard { key: i32, pressed: bool },
    KeyboardClick { key: i32 },
    MouseButton { button: i32, pressed: bool },
    MouseClick { button: i32 },
    /// 归一化到源屏的绝对指针位置（0.0..=1.0）。
    MouseAbsolute { x: f64, y: f64 },
    MouseRelative { dx: i32, dy: i32 },
    MouseScroll { dx: i32, dy: i32 },
    /// 移动端自带输入法打出来的整段文字（官方 Windows 宿主 `text_input {content}`）。
    TextInput { text: String },
}

/// 输入事件的一行摘要（诊断日志用，不含内容敏感信息）。
pub fn input_summary(input: &ControlInput) -> &'static str {
    match input {
        ControlInput::Keyboard { pressed, .. } => if *pressed { "kbd_press" } else { "kbd_release" },
        ControlInput::KeyboardClick { .. } => "kbd_click",
        ControlInput::MouseButton { pressed, .. } => if *pressed { "mouse_press" } else { "mouse_release" },
        ControlInput::MouseClick { .. } => "mouse_click",
        ControlInput::MouseAbsolute { .. } => "mouse_move_absolute",
        ControlInput::MouseRelative { .. } => "mouse_move_relative",
        ControlInput::MouseScroll { .. } => "mouse_scroll",
        ControlInput::TextInput { .. } => "text_input",
    }
}

/// 整条消息的输入摘要（不解构消息，避免部分移动）。
pub fn input_summary_of(message: &ControlMessage) -> &'static str {
    match message {
        ControlMessage::Input(input) => input_summary(input),
        ControlMessage::KeyboardHeartbeat => "kbd_heartbeat",
        ControlMessage::Action(_) => "simple_action",
        ControlMessage::Rpc(_) => "rpc_request",
        ControlMessage::Unsupported(_) => "unsupported",
    }
}

/// CONTROL 通道一条消息解出来的东西。
#[derive(Debug, Clone, PartialEq)]
pub enum ControlMessage {
    /// 客户端保活：`{"heartbeat":"kbd_heartbeat"}`。
    KeyboardHeartbeat,
    Input(ControlInput),
    /// 认不出的 JSON：原样留着，日志里报形状，绝不猜语义。
    Unsupported(Value),
    /// protobuf SimpleAction 控制面。
    Action(SimpleAction),
    /// 画质/帧率/分辨率设置 RPC（Message 21）。
    Rpc(RpcRequest),
}

/// `Message.simpleAction`（field 3）解出来的东西。
#[derive(Debug, Clone, PartialEq)]
pub struct SimpleAction {
    /// `Message.seq`（field 1）；回 `echo_response` 时要用同一个号。
    pub seq: u64,
    pub timestamp: u64,
    /// `SimpleAction.actionType`（field 1）。
    pub action: u64,
    /// `SimpleAction.args`（field 2），官方是 JSON 字符串。
    pub args: String,
}

fn int_field(v: &Value, name: &str) -> Result<i32, String> {
    v.get(name)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| format!("CONTROL 字段 {name} 必须是 i32 整数"))
}

fn coord_field(v: &Value, name: &str) -> Result<f64, String> {
    v.get(name)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .ok_or_else(|| format!("CONTROL 字段 {name} 必须是有限数字"))
}

/// 解一条 CONTROL 报文（JSON 或 protobuf SimpleAction 二选一，看首字节）。
///
/// JSON 里字段类型不对 → `Err`（客户端发了半截包，必须显式报错）；
/// 认不出的 action → `Unsupported`（未知不等于错，别自作主张执行）。
pub fn decode_control(data: &[u8]) -> Result<ControlMessage, String> {
    // protobuf 帧首字节是 field 1 的 tag（8 = varint seq）；JSON 永远以 `{` 开头。
    if data.first() != Some(&b'{') {
        let fields = decode_wire(data).map_err(|e| format!("CONTROL protobuf 解不开: {e:?}"))?;
        if nested(&fields, 3).is_some() {
            return decode_action(data).map(ControlMessage::Action);
        }
        if let Some(request) = decode_rpc_fields(&fields) {
            return Ok(ControlMessage::Rpc(request));
        }
        return Err("CONTROL protobuf 里既没有 simpleAction(3) 也没有 RpcRequest(21)".into());
    }
    let v: Value = serde_json::from_slice(data).map_err(|_| "CONTROL 载荷不是合法 JSON")?;
    if v.get("heartbeat").and_then(Value::as_str) == Some("kbd_heartbeat") {
        return Ok(ControlMessage::KeyboardHeartbeat);
    }
    let Some(action) = v.get("action").and_then(Value::as_str) else {
        return Ok(ControlMessage::Unsupported(v));
    };
    let input = match action {
        "kbd_press" => ControlInput::Keyboard { key: int_field(&v, "key")?, pressed: true },
        "kbd_release" => ControlInput::Keyboard { key: int_field(&v, "key")?, pressed: false },
        "kbd_click" => ControlInput::KeyboardClick { key: int_field(&v, "key")? },
        "mouse_press" => ControlInput::MouseButton { button: int_field(&v, "button")?, pressed: true },
        "mouse_release" => ControlInput::MouseButton { button: int_field(&v, "button")?, pressed: false },
        "mouse_click" => ControlInput::MouseClick { button: int_field(&v, "button")? },
        "mouse_move_absolute" => ControlInput::MouseAbsolute { x: coord_field(&v, "abs_x")?, y: coord_field(&v, "abs_y")? },
        "mouse_move_relative" => ControlInput::MouseRelative { dx: int_field(&v, "delta_x")?, dy: int_field(&v, "delta_y")? },
        "mouse_scroll" => ControlInput::MouseScroll { dx: int_field(&v, "delta_x")?, dy: int_field(&v, "delta_y")? },
        "text_input" => {
            let text = v.get("content").and_then(Value::as_str).ok_or("CONTROL 字段 content 必须是字符串")?;
            if text.len() > 16 * 1024 {
                return Err("text_input content 超过 16 KiB".into());
            }
            ControlInput::TextInput { text: text.to_owned() }
        }
        _ => return Ok(ControlMessage::Unsupported(v)),
    };
    Ok(ControlMessage::Input(input))
}

/// 解 protobuf `Message{seq,timestamp,simpleAction{actionType,args}}`。
pub fn decode_action(data: &[u8]) -> Result<SimpleAction, String> {
    let fields = decode_wire(data).map_err(|e| format!("CONTROL protobuf 解不开: {e:?}"))?;
    let seq = varint(&fields, 1).unwrap_or(0);
    let timestamp = varint(&fields, 2).unwrap_or(0);
    let inner = nested(&fields, 3).ok_or("protobuf 里没有 simpleAction(field 3)")?;
    Ok(SimpleAction {
        seq,
        timestamp,
        // protobuf 不序列化默认值：缺 field 1 就是 actionType=0（echo_request）。
        // Windows 客户端每 ~500ms 发的保活正是这种"空"包，必须当 echo 回，否则客户端判活失败、不发输入。
        action: varint(&inner, 1).unwrap_or(0),
        args: string_field(&inner, 2).unwrap_or_default(),
    })
}

fn varint(fields: &[WireField<'_>], number: u32) -> Option<u64> {
    fields.iter().find(|f| f.number == number).and_then(|f| match &f.value {
        WireValue::Varint(v) => Some(*v),
        _ => None,
    })
}

fn string_field(fields: &[WireField<'_>], number: u32) -> Option<String> {
    fields.iter().find(|f| f.number == number).and_then(|f| match &f.value {
        WireValue::Bytes(b) => std::str::from_utf8(b).ok().map(str::to_owned),
        _ => None,
    })
}

fn put_varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// 采集设置 RPC（`Message` 21 `RpcRequest`，case 2/3/4/6）带来的变更意图。
/// None = 这条报文没提该项，保持原值；不要把 0 当"取消"。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SettingPatch {
    pub fps: Option<u32>,
    /// 1 Fast / 2 高清2M / 3 超清8M / 4 原画20M / 5 Auto / 6 Custom。
    pub quality: Option<u64>,
    /// field 8：客户端自定义码率，单位 **bit/s**。
    pub custom_bps: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// field 7：1=4:2:0、3=4:4:4。我们只产 4:2:0，Sync 里报实际值。
    pub chroma: Option<u64>,
    /// field 13：客户端当前解码的 codec（1=H264、2=H265）。是告知，不是切换指令。
    pub codec: Option<u64>,
    /// field 10 == 3：跟随被控端原生分辨率 → 撤销降档。
    pub follow_host: bool,
}

/// Message 21 RpcRequest。request_id 必须原样回进 Message 22，否则客户端配不上对。
#[derive(Debug, Clone, PartialEq)]
pub struct RpcRequest {
    pub request_id: u64,
    /// 2=CaptureSetting 3=FpsSetting 4=VideoQuality 6=Resolution；0=认不出的类型（剪贴板 RPC 等），只 ack 不改状态。
    pub case: u32,
    /// Message 21 body 里出现过的 bytes 字段号（照参照实现区分会话/插件 RPC 用）。
    pub kinds: Vec<u32>,
    pub patch: SettingPatch,
}

/// fps 菜单序号（field 1）→ 真实帧率。菜单发的是**序号不是帧率**；field 18 fpsCount 优先。
pub fn fps_from_index(index: Option<u64>) -> Option<u32> {
    match index? {
        1 => Some(30),
        2 => Some(60),
        3 => Some(90),
        4 => Some(144),
        _ => None,
    }
}

/// 画质档位 → 目标码率 kbps（参照实现的实测值）。0 = 用宿主自动公式。
pub fn preset_bitrate_kbps(quality: u64, custom_bps: u64) -> u32 {
    match quality {
        1 => 1_000,
        2 => 2_000,
        3 => 8_000,
        4 => 20_000,
        6 => (custom_bps / 1_000).clamp(1, 100_000) as u32,
        _ => 0,
    }
}

pub fn decode_rpc(data: &[u8]) -> Result<RpcRequest, String> {
    let fields = decode_wire(data).map_err(|e| format!("CONTROL protobuf 解不开: {e:?}"))?;
    decode_rpc_fields(&fields).ok_or_else(|| "protobuf 不是 Message 21 RpcRequest（缺 header.request_id）".to_owned())
}

fn decode_rpc_fields(fields: &[WireField<'_>]) -> Option<RpcRequest> {
    let body = nested(fields, 21)?;
    // 会话/插件 RPC 可能不带 RequestHeader——参照实现容忍缺 header，我们也容忍（request_id=0）。
    let request_id = nested(&body, 1).and_then(|h| varint_field(&h, 1)).unwrap_or(0);
    let kinds: Vec<u32> = body.iter().filter(|f| matches!(f.value, WireValue::Bytes(_))).map(|f| f.number).collect();
    let mut patch = SettingPatch::default();
    let case;
    if let Some(inner) = nested(&body, 2) {
        case = 2;
        patch.quality = varint_field(&inner, 2);
        // 16/17 优先于 5/6（官方注释）；field 1 是菜单序号，18 才是真实帧率。
        patch.width = varint_field(&inner, 16).or_else(|| varint_field(&inner, 5)).map(|v| v as u32);
        patch.height = varint_field(&inner, 17).or_else(|| varint_field(&inner, 6)).map(|v| v as u32);
        patch.chroma = varint_field(&inner, 7);
        patch.custom_bps = varint_field(&inner, 8);
        patch.follow_host = varint_field(&inner, 10) == Some(3);
        patch.codec = varint_field(&inner, 13);
        patch.fps = varint_field(&inner, 18).map(|v| v as u32).or_else(|| fps_from_index(varint_field(&inner, 1)));
    } else if let Some(inner) = nested(&body, 3) {
        case = 3;
        patch.fps = fps_from_index(varint_field(&inner, 1));
    } else if let Some(inner) = nested(&body, 4) {
        case = 4;
        patch.quality = varint_field(&inner, 1);
    } else if let Some(inner) = nested(&body, 6) {
        case = 6;
        patch.width = varint_field(&inner, 1).map(|v| v as u32);
        patch.height = varint_field(&inner, 2).map(|v| v as u32);
    } else {
        case = 0;
    }
    Some(RpcRequest { request_id, case, kinds, patch })
}

fn put_int(number: u32, value: u64, out: &mut Vec<u8>) {
    if value != 0 {
        // tag 也是 varint：字段号 ≥16 时 (number<<3) 会超 127，push 单字节会解成续接。
        put_varint((u64::from(number) << 3) | 0, out);
        put_varint(value, out);
    }
}

fn put_nested(number: u32, payload: &[u8], out: &mut Vec<u8>) {
    put_varint((u64::from(number) << 3) | 2, out);
    put_varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

/// 回 Message 22 RpcResponse：`1 header{1 request_id 原样}` + `2 CaptureSettingResponse{1 errors[]}`。
/// 空 errors = 已应用；拒绝时 errors[0] = `{1 code=-1, 2 message}`。
pub fn encode_rpc_response(request_id: u64, own_seq: u64, timestamp: u64, error: Option<&str>) -> Vec<u8> {
    let mut header = Vec::new();
    put_int(1, request_id, &mut header);
    let mut response = Vec::new();
    if let Some(message) = error {
        let mut err = Vec::new();
        // int32 -1 上线是符号扩展的 u64::MAX。
        put_int(1, -1i64 as u64, &mut err);
        put_nested(2, message.as_bytes(), &mut err);
        put_nested(1, &err, &mut response);
    }
    let mut payload = Vec::new();
    put_nested(1, &header, &mut payload);
    // 成功也要发空消息：官方是 put_bytes(2, &[])，缺字段客户端可能一直等。
    put_nested(2, &response, &mut payload);
    envelope(own_seq, timestamp, 22, &payload)
}

/// 会话/插件 RPC 应答面，字段号照 Windows GameViewerServer.exe 内嵌 rpc_main.proto：
/// RpcResponse{11 send_video_track_rsp、12 query_plugin、13 update_plugin、14 dl_install_plugin、
/// 15 plugin_enable_notif、16 plugin_notif、17 virtual_audio_policy_rsp}，与请求同位；
/// 插件 16→12（查询：未启用）/ 17→13 / 18→14 / 19→15 / 20→16（error_code=1 不支持）。
/// Linux 没有插件；不答这些 RPC 客户端会停在"正在建立连接"，画质菜单也不开。
/// 认不出任何会话/插件字段时返回 None（调用方回空 CaptureSettingResponse）。
pub fn session_rpc_response(request_id: u64, own_seq: u64, timestamp: u64, kinds: &[u32]) -> Option<Vec<u8>> {
    let kind = *kinds.iter().find(|k| matches!(**k, 15 | 16..=21))?;
    let (field, error_code, id_field): (u32, bool, u32) = match kind {
        15 => (11, false, 0),
        21 => (17, false, 0),
        16 => (12, false, 3),
        17 => (13, true, 3),
        18 => (14, true, 2),
        19 => (15, true, 2),
        _ => (16, true, 2),
    };
    let mut response = Vec::new();
    if error_code {
        put_int(1, 1, &mut response);
    }
    if id_field != 0 {
        put_nested(id_field, &[], &mut response);
    }
    let mut header = Vec::new();
    put_int(1, request_id, &mut header);
    let mut payload = Vec::new();
    put_nested(1, &header, &mut payload);
    put_nested(field, &response, &mut payload);
    Some(envelope(own_seq, timestamp, 22, &payload))
}

/// Message 25 CaptureSettingSync：报**实际生效**值。官方 schema（Windows 内嵌描述符）里
/// field 1 是 **FPS 枚举**（1=30/2=60/3=90/4=144），不是帧率数字——报 60 客户端解成未知档，
/// 会判定"被控端没接受指定帧率"。`chroma` 是管线真实色度（1=4:2:0、3=4:4:4）。
pub fn encode_capture_setting_sync(
    seq: u64,
    timestamp: u64,
    fps: u32,
    quality: u64,
    custom_bps: u64,
    width: u32,
    height: u32,
    chroma: u64,
) -> Vec<u8> {
    let mut payload = Vec::new();
    put_int(1, crate::capture_setting::fps_to_enum(fps), &mut payload);
    put_int(2, quality, &mut payload);
    put_int(3, chroma, &mut payload);
    if quality == 6 && custom_bps > 0 {
        put_int(4, custom_bps, &mut payload);
    }
    put_int(5, u64::from(width), &mut payload);
    put_int(6, u64::from(height), &mut payload);
    put_int(9, u64::from(width), &mut payload);
    put_int(10, u64::from(height), &mut payload);
    envelope(seq, timestamp, 25, &payload)
}

/// Message 14 `ReportQosStats`（官方 Windows 宿主每次会话都报）：
/// `{1 encoder_type, 2 capture_type, 3 probe_bps, 4 video_quality, 5 fast_bitrate,
/// 6 general_bitrate, 7 hd_bitrate, 8 bluray_bitrate}`。控制端的编码器/采集显示与各档
/// 码率上限读这份报文；不报它，客户端只能显示默认值。码率单位 bit/s。
pub fn encode_qos_stats(
    seq: u64,
    timestamp: u64,
    encoder_type: &str,
    capture_type: &str,
    probe_bps: u64,
    quality: u64,
    tier_bitrates_bps: (u64, u64, u64, u64),
) -> Vec<u8> {
    let mut payload = Vec::new();
    put_nested(1, encoder_type.as_bytes(), &mut payload);
    put_nested(2, capture_type.as_bytes(), &mut payload);
    put_int(3, probe_bps, &mut payload);
    put_int(4, quality, &mut payload);
    for (field, bps) in [(5u32, tier_bitrates_bps.0), (6, tier_bitrates_bps.1), (7, tier_bitrates_bps.2), (8, tier_bitrates_bps.3)] {
        put_int(field, bps, &mut payload);
    }
    envelope(seq, timestamp, 14, &payload)
}

/// ff_capture_setting 版本号。4 = 累积含 CUSTOM_BITRATE（fps/预设画质/自定义码率 RPC）。
/// 声明了就必须每条 21 都回 22；不声明客户端一条 RPC 都不发（安全默认）。
pub fn capture_setting_feature_version() -> u8 {
    4
}

/// 回 `echo_response`：官方形状是 `Message{seq, timestamp, simpleAction{actionType=1, args="{\"seq\":N}"}}`。
///
/// args 里的 `seq` 是**请求**的 seq，客户端靠它配对；自己的 seq 与时间戳另算。
pub fn encode_echo_response(request_seq: u64, own_seq: u64, timestamp: u64) -> Vec<u8> {
    let args = format!(r#"{{"seq":{request_seq}}}"#);
    let mut action = vec![8, 1, 18];
    put_varint(args.len() as u64, &mut action);
    action.extend_from_slice(args.as_bytes());
    // FeatureFlag（SimpleAction field 4）：1 ff_capture_setting = 4。声明了就必须每条 21 都回 22，
    // 客户端画质菜单才开得起来；不声明它一条 RPC 都不发。
    // 照参照实现的完整声明：2 ff_simple_action=2、3 ff_system_metrics=2、6 ff_file_transfer_ftp=2。
    let flags = [8u8, capture_setting_feature_version(), 16, 2, 24, 2, 48, 2];
    action.push(34);
    action.push(flags.len() as u8);
    action.extend_from_slice(&flags);
    let mut out = Vec::new();
    if own_seq != 0 {
        out.push(8);
        put_varint(own_seq, &mut out);
    }
    if timestamp != 0 {
        out.push(16);
        put_varint(timestamp, &mut out);
    }
    out.push(26);
    put_varint(action.len() as u64, &mut out);
    out.extend_from_slice(&action);
    out
}

/// 官方 ActionType 表（客户端 `_ProtoNameProviding` 的 28 项名字表）。只用于解释与日志。
pub fn action_name(action: u64) -> Option<&'static str> {
    const NAMES: [&str; 28] = [
        "echo_request",
        "echo_response",
        "show_desktop",
        "show_windows",
        "capture_desktop",
        "capture_window",
        "capture_mumu",
        "stop_capture",
        "restart_capture",
        "reboot_system",
        "lock_system",
        "key_toggle",
        "shutdown_system",
        "show_taskmgr",
        "mute_system",
        "auto_unlock_system",
        "mute_restore",
        "mute_permanent",
        "mute_temporary",
        "set_auto_lock",
        "stop_acquire_update",
        "quit_capture",
        "acquire_system_info",
        "microphone_opened",
        "microphone_closed",
        "speaker_opened",
        "speaker_closed",
        "need_restart_userapp",
    ];
    usize::try_from(action).ok().and_then(|n| NAMES.get(n).copied())
}

/// Windows VK → X11 keysym 基码（不含 shift 变体；修饰键单独成事件）。
///
/// 线上没有扩展键位，所以 `VK_RETURN` 就是 Return。字母用**未加 shift 的小写**符号，
/// 大写靠客户端另外发的 Shift 事件。认不出的 VK 返回 `None`（宁可丢键也不注入裸值）。
pub fn windows_vk_to_keysym(vk: i32) -> Option<u32> {
    let keysym = match vk {
        0x30..=0x39 => vk as u32, // 0..9
        0x41..=0x5a => (vk + 0x20) as u32, // A..Z → a..z
        0x60..=0x69 => 0xffb0 + (vk - 0x60) as u32, // KP_0..KP_9
        0x70..=0x87 => 0xffbe + (vk - 0x70) as u32, // F1..F24
        0x08 => 0xff08, // BackSpace
        0x09 => 0xff09, // Tab
        0x0c => 0xff0b, // Clear
        0x0d => 0xff0d, // Return
        0x10 | 0xa0 => 0xffe1, // Shift / Shift_L
        0xa1 => 0xffe2, // Shift_R
        0x11 | 0xa2 => 0xffe3, // Control / Control_L
        0xa3 => 0xffe4, // Control_R
        0x12 | 0xa4 => 0xffe9, // Alt / Alt_L
        0xa5 => 0xffea, // Alt_R
        0x13 => 0xff13, // Pause
        0x14 => 0xffe5, // Caps_Lock
        0x1b => 0xff1b, // Escape
        0x20 => 0x0020, // space
        0x21 => 0xff55, // Prior
        0x22 => 0xff56, // Next
        0x23 => 0xff57, // End
        0x24 => 0xff50, // Home
        0x25 => 0xff51, // Left
        0x26 => 0xff52, // Up
        0x27 => 0xff53, // Right
        0x28 => 0xff54, // Down
        0x2c => 0xff61, // Print
        0x2d => 0xff63, // Insert
        0x2e => 0xffff, // Delete
        0x5b => 0xffeb, // Super_L
        0x5c => 0xffec, // Super_R
        0x5d => 0xff67, // Menu
        0x6a => 0xffaa, // KP_Multiply
        0x6b => 0xffab, // KP_Add
        0x6c => 0xffac, // KP_Separator
        0x6d => 0xffad, // KP_Subtract
        0x6e => 0xffae, // KP_Decimal
        0x6f => 0xffaf, // KP_Divide
        0x90 => 0xff7f, // Num_Lock
        0x91 => 0xff14, // Scroll_Lock
        0xba => 0x003b, // OEM_1 ;
        0xbb => 0x003d, // OEM_PLUS =
        0xbc => 0x002c, // OEM_COMMA
        0xbd => 0x002d, // OEM_MINUS
        0xbe => 0x002e, // OEM_PERIOD
        0xbf => 0x002f, // OEM_2 /
        0xc0 => 0x0060, // OEM_3 `
        0xdb => 0x005b, // OEM_4 [
        0xdc => 0x005c, // OEM_5 \
        0xdd => 0x005d, // OEM_6 ]
        0xde => 0x0027, // OEM_7 '
        // OEM_102（欧洲 < > 键）依赖物理布局，不猜。
        _ => return None,
    };
    Some(keysym)
}

/// 绝对坐标 → 源屏像素。Mac/移动端把指针归一化到 0..1；Windows 客户端发的可能
/// 是屏幕像素（实测：只认 0..1 会把指针永远钉死不动）。两种都认：
/// 分量都 ≤1 按归一化解释，否则按像素解释并夹到屏内。
pub fn normalized_pointer(x: f64, y: f64, width: u32, height: u32) -> Option<(i32, i32)> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return None;
    }
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return None;
    }
    let mapped = if x <= 1.0 && y <= 1.0 {
        (x * (width - 1) as f64, y * (height - 1) as f64)
    } else {
        (x, y)
    };
    Some((
        mapped.0.clamp(0.0, (width - 1) as f64).round() as i32,
        mapped.1.clamp(0.0, (height - 1) as f64).round() as i32,
    ))
}

/// 鼠标按键位掩码 → X11 按钮序号。bit0 左=1，bit1 右=3，bit2 中=2；组合值不猜。
pub fn mouse_button_to_x11(button: i32) -> Option<u8> {
    match button {
        1 => Some(1),
        2 => Some(3),
        4 => Some(2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::decode_wire;

    fn response_fields(payload: &[u8]) -> Vec<(u32, Vec<u32>)> {
        let outer = decode_wire(payload).unwrap();
        let message = nested(&outer, 22).unwrap();
        let mut out = Vec::new();
        for f in message {
            if let WireValue::Bytes(inner) = &f.value {
                let numbers = decode_wire(inner).unwrap().iter().map(|x| x.number).collect();
                out.push((f.number, numbers));
            }
        }
        out
    }

    #[test]
    fn qos_stats_reports_encoder_names_and_tier_bitrates() {
        let stats = encode_qos_stats(1, 2, "NVIDIA NVENC", "X11", 6_000_000, 5, (1_000_000, 2_000_000, 8_000_000, 20_000_000));
        let fields = decode_wire(&stats).unwrap();
        let body = nested(&fields, 14).unwrap();
        let numbers: Vec<u32> = body.iter().map(|f| f.number).collect();
        assert_eq!(numbers, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let strings: Vec<&str> = body
            .iter()
            .filter_map(|f| match &f.value { WireValue::Bytes(b) => std::str::from_utf8(b).ok(), _ => None })
            .collect();
        assert_eq!(strings, vec!["NVIDIA NVENC", "X11"]);
    }

    #[test]
    fn sync_reports_fps_as_enum_not_as_a_number() {
        // 官方 CaptureSettingSync{1 fps(FPS 枚举), 2 frame_quality, 3 chroma_format, 5/6 逻辑分辨率,
        // 9/10 像素分辨率}。field 1 报 60（数字）客户端解成未知档 → "被控端不支持指定帧率"。
        let sync = encode_capture_setting_sync(1, 2, 60, 4, 0, 2560, 1440, 3);
        let fields = decode_wire(&sync).unwrap();
        let body = nested(&fields, 25).unwrap();
        let got: Vec<(u32, u64)> = body
            .iter()
            .filter_map(|f| match &f.value { WireValue::Varint(v) => Some((f.number, *v)), _ => None })
            .collect();
        assert_eq!(got, vec![(1, 2), (2, 4), (3, 3), (5, 2560), (6, 1440), (9, 2560), (10, 1440)]);
        // 帧率档边界：30→1、90→3、144/更高→4（>144 官方走 super screen，普通档封顶 144）。
        assert_eq!(crate::capture_setting::fps_to_enum(12), 1);
        assert_eq!(crate::capture_setting::fps_to_enum(90), 3);
        assert_eq!(crate::capture_setting::fps_to_enum(240), 4);
    }

    #[test]
    fn session_rpc_responses_use_the_matching_response_fields() {
        let reply = session_rpc_response(7, 1, 2, &[15]).unwrap();
        // 官方 rpc_main.proto：RpcResponse 11 = send_video_track_rsp（空 = 已接受）。
        assert_eq!(response_fields(&reply), vec![(1, vec![1]), (11, vec![])]);
        let reply = session_rpc_response(8, 1, 2, &[21]).unwrap();
        assert_eq!(response_fields(&reply), vec![(1, vec![1]), (17, vec![])]);
        let reply = session_rpc_response(9, 1, 2, &[16]).unwrap();
        assert_eq!(response_fields(&reply), vec![(1, vec![1]), (12, vec![3])]);
        let reply = session_rpc_response(10, 1, 2, &[18]).unwrap();
        assert_eq!(response_fields(&reply), vec![(1, vec![1]), (14, vec![1, 2])]);
        assert!(session_rpc_response(11, 1, 2, &[2, 3]).is_none());
    }

    #[test]
    fn rpc_request_without_header_still_decodes_with_kinds() {
        // Message 21 body 只有 field 15（send_video_track_req），没有 RequestHeader。
        let message = envelope(1, 2, 21, &[122u8, 0]);
        let request = decode_rpc(&message).unwrap();
        assert_eq!(request.request_id, 0);
        assert_eq!(request.case, 0);
        assert_eq!(request.kinds, vec![15]);
    }

    #[test]
    fn protobuf_echo_request_omits_default_action_type() {
        // protobuf never serializes actionType=0; Windows keepalive looks like
        // Message{ simpleAction{ FeatureFlag } } with no field 1 at all.
        // Message{3: SimpleAction{4: FeatureFlag{1:6}}}，手拼字节，无 field 1。
        let msg = [0x1Au8, 4, 0x22, 2, 0x08, 6];
        let action = decode_action(&msg).expect("缺 actionType 要当 0 解");
        assert_eq!(action.action, 0);
    }

    #[test]
    fn json_and_binary_carried_json_both_decode() {
        let text = br#"{"heartbeat":"kbd_heartbeat"}"#;
        assert_eq!(decode_control(text).unwrap(), ControlMessage::KeyboardHeartbeat);
        // 同一段 JSON 用 binary 帧送来也要认（首字节仍是 `{`）。
        let mut binary = Vec::new();
        binary.extend_from_slice(text);
        assert_eq!(decode_control(&binary).unwrap(), ControlMessage::KeyboardHeartbeat);
    }

    #[test]
    fn absolute_and_relative_pointer_keys() {
        assert_eq!(
            decode_control(br#"{"action":"mouse_move_absolute","abs_x":0.5,"abs_y":0.25}"#).unwrap(),
            ControlMessage::Input(ControlInput::MouseAbsolute { x: 0.5, y: 0.25 })
        );
        assert_eq!(
            decode_control(br#"{"action":"mouse_move_relative","delta_x":-4,"delta_y":5}"#).unwrap(),
            ControlMessage::Input(ControlInput::MouseRelative { dx: -4, dy: 5 })
        );
    }

    #[test]
    fn button_and_key_identifiers_stay_wire_values() {
        assert_eq!(
            decode_control(br#"{"action":"mouse_press","button":2}"#).unwrap(),
            ControlMessage::Input(ControlInput::MouseButton { button: 2, pressed: true })
        );
        assert_eq!(
            decode_control(br#"{"action":"mouse_release","button":2}"#).unwrap(),
            ControlMessage::Input(ControlInput::MouseButton { button: 2, pressed: false })
        );
        assert_eq!(
            decode_control(br#"{"action":"kbd_release","key":65}"#).unwrap(),
            ControlMessage::Input(ControlInput::Keyboard { key: 65, pressed: false })
        );
        assert_eq!(
            decode_control(br#"{"action":"mouse_scroll","delta_x":0,"delta_y":-3}"#).unwrap(),
            ControlMessage::Input(ControlInput::MouseScroll { dx: 0, dy: -3 })
        );
    }

    #[test]
    fn text_input_keeps_unicode_and_caps_size() {
        assert_eq!(
            decode_control("{\"action\":\"text_input\",\"content\":\"你好 a\"}".as_bytes()).unwrap(),
            ControlMessage::Input(ControlInput::TextInput { text: "你好 a".into() })
        );
        assert!(decode_control(br#"{"action":"text_input"}"#).is_err());
        let big = format!(r#"{{"action":"text_input","content":"{}"}}"#, "a".repeat(20 * 1024));
        assert!(decode_control(big.as_bytes()).is_err());
    }

    #[test]
    fn bad_field_types_are_errors_not_guesses() {
        assert!(decode_control(br#"{"action":"kbd_press"}"#).is_err());
        assert!(decode_control(br#"{"action":"kbd_press","key":2147483648}"#).is_err());
        assert!(decode_control(br#"{"action":"kbd_press","key":1.5}"#).is_err());
        assert!(decode_control(br#"{"action":"mouse_move_absolute","abs_x":"x","abs_y":1}"#).is_err());
        assert!(decode_control(&[0xff, 0x08]).is_err());
    }

    #[test]
    fn unknown_action_is_unsupported() {
        assert!(matches!(decode_control(br#"{"action":"unverified"}"#).unwrap(), ControlMessage::Unsupported(_)));
        assert!(matches!(decode_control(br#"{"nope":1}"#).unwrap(), ControlMessage::Unsupported(_)));
    }

    #[test]
    fn simple_action_envelope_roundtrips() {
        // Message{seq=9, ts=11, simpleAction{actionType=4, args="{\"seq\":9}"}}
        let wire = {
            let mut v = vec![8, 9, 16, 11, 26];
            let inner = {
                let args = br#"{"seq":9}"#;
                let mut a = vec![8, 4, 18, args.len() as u8];
                a.extend_from_slice(args);
                a
            };
            v.push(inner.len() as u8);
            v.extend_from_slice(&inner);
            v
        };
        let action = decode_action(&wire).unwrap();
        assert_eq!(action.seq, 9);
        assert_eq!(action.timestamp, 11);
        assert_eq!(action.action, 4);
        assert_eq!(action.args, r#"{"seq":9}"#);
        assert_eq!(action_name(4), Some("capture_desktop"));
        assert_eq!(action_name(22), Some("acquire_system_info"));
        assert_eq!(action_name(28), None);
    }

    #[test]
    fn echo_response_shape_matches_official() {
        let reply = encode_echo_response(200, 1, 2);
        let outer = decode_wire(&reply).unwrap();
        let inner = nested(&outer, 3).expect("simpleAction");
        assert_eq!(varint(&outer, 1), Some(1));
        assert_eq!(varint(&outer, 2), Some(2));
        assert_eq!(varint(&inner, 1), Some(1), "actionType = echo_response");
        assert_eq!(string_field(&inner, 2).unwrap(), r#"{"seq":200}"#, "args 带请求 seq");
    }

    #[test]
    fn vk_map_uses_unshifted_symbols_and_keeps_modifiers_apart() {
        assert_eq!(windows_vk_to_keysym(65), Some(u32::from(b'a')));
        assert_eq!(windows_vk_to_keysym(90), Some(u32::from(b'z')));
        for d in b'0'..=b'9' {
            assert_eq!(windows_vk_to_keysym(i32::from(d)), Some(u32::from(d)));
        }
        assert_eq!(windows_vk_to_keysym(16), Some(0xffe1));
        assert_eq!(windows_vk_to_keysym(160), Some(0xffe1));
        assert_eq!(windows_vk_to_keysym(161), Some(0xffe2));
        assert_eq!(windows_vk_to_keysym(13), Some(0xff0d));
        assert_eq!(windows_vk_to_keysym(46), Some(0xffff));
        assert_eq!(windows_vk_to_keysym(112), Some(0xffbe));
        assert_eq!(windows_vk_to_keysym(135), Some(0xffd5));
        assert_eq!(windows_vk_to_keysym(96), Some(0xffb0));
        assert_eq!(windows_vk_to_keysym(220), Some(u32::from(b'\\')));
        for vk in [-1, 0, 7, 21, 136, 226, 255, 256, i32::MAX] {
            assert_eq!(windows_vk_to_keysym(vk), None);
        }
    }

    #[test]
    fn normalized_pointer_maps_edges_and_rejects_junk() {
        assert_eq!(normalized_pointer(0.0, 0.0, 1920, 1080), Some((0, 0)));
        assert_eq!(normalized_pointer(1.0, 1.0, 1920, 1080), Some((1919, 1079)));
        assert_eq!(normalized_pointer(0.5, 0.5, 1920, 1080), Some((960, 540)));
        assert_eq!(normalized_pointer(0.6805736636245111, 0.15990730011587487, 1920, 1080), Some((1306, 173)));
        assert_eq!(normalized_pointer(f64::NAN, 0.5, 1920, 1080), None);
        assert_eq!(normalized_pointer(-0.1, 0.5, 1920, 1080), None);
        // 1.1 现在按像素解释（Windows 客户端发像素），夹到屏内而不是丢弃。
        assert_eq!(normalized_pointer(0.5, 1.1, 1920, 1080), Some((1, 1)));
        assert_eq!(normalized_pointer(0.5, 0.5, 0, 1080), None);
    }

    #[test]
    fn absolute_coordinates_accept_windows_pixel_space() {
        // >1 的分量按像素解释（夹到屏内），≤1 仍是归一化。
        assert_eq!(normalized_pointer(1200.0, 900.0, 2560, 1440), Some((1200, 900)));
        assert_eq!(normalized_pointer(9999.0, 9999.0, 2560, 1440), Some((2559, 1439)));
        assert_eq!(normalized_pointer(0.5, 0.5, 1920, 1080), Some((960, 540)));
        assert_eq!(normalized_pointer(-1.0, 0.5, 1920, 1080), None);
    }

    #[test]
    fn button_bitmasks_map_to_x11_ordinals() {
        assert_eq!(mouse_button_to_x11(1), Some(1));
        assert_eq!(mouse_button_to_x11(2), Some(3));
        assert_eq!(mouse_button_to_x11(4), Some(2));
        assert_eq!(mouse_button_to_x11(3), None);
        assert_eq!(mouse_button_to_x11(0), None);
    }
}
