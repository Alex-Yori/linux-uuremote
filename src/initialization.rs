//! CONTROL 初始化 protobuf：宿主必须在数据通道打开后自报屏幕清单与采集目标，
//! 否则客户端不知道把哪条视频轨绑到哪块屏幕，画面永远不会显示。
//!
//! 形状来自参照实现（Mac 客户端恢复出的 `Message`）：
//! `Message{ 1: seq, 2: timestamp, <which>: payload }`，
//! which=7 ScreenSources、8 CaptureChange、15 SystemStateChange、12 ReportError。
//! 坐标与尺寸必须是**实测**值，这里不编造任何未测到的能力（DPI、分辨率档位都省略）。

/// protobuf varint（LEB128），写入 out。
fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// 带符号整数按 int32/int64 的 protobuf 惯例走 varint（负数变成 10 字节补码）。
/// 值为 0 时整字段省略——这是官方编码器的行为，也是"未声明"的语义。
fn integer(number: u32, value: i32, out: &mut Vec<u8>) {
    if value != 0 {
        varint(u64::from(number) << 3, out);
        varint(value as i64 as u64, out);
    }
}

fn boolean(number: u32, value: bool, out: &mut Vec<u8>) {
    if value {
        integer(number, 1, out);
    }
}

fn bytes(number: u32, value: &[u8], out: &mut Vec<u8>) {
    varint((u64::from(number) << 3) | 2, out);
    varint(value.len() as u64, out);
    out.extend_from_slice(value);
}

fn text(number: u32, value: &str, out: &mut Vec<u8>) {
    if !value.is_empty() {
        bytes(number, value.as_bytes(), out);
    }
}

/// `Message{ seq, timestamp, <which>: payload }` 信封。which 是嵌套消息的字段号。
pub fn envelope(seq: u64, timestamp: u64, which: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if seq != 0 {
        out.push(8);
        varint(seq, &mut out);
    }
    if timestamp != 0 {
        out.push(16);
        varint(timestamp, &mut out);
    }
    bytes(which, payload, &mut out);
    out
}

/// 一块屏幕的几何：逻辑坐标 + 像素尺寸。合成器桌面 left/top 恒为 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenGeometry {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub pixel_width: i32,
    pub pixel_height: i32,
}

impl ScreenGeometry {
    /// 逻辑与像素都必须是正数；不合法就报错，不用假值兜底。
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if self.width <= 0 || self.height <= 0 || self.pixel_width <= 0 || self.pixel_height <= 0 {
            return Err("屏幕尺寸必须有正的逻辑与像素宽高".into());
        }
        let mut out = Vec::new();
        for (field, value) in [
            (1, self.left),
            (2, self.top),
            (3, self.width),
            (4, self.height),
            (5, self.pixel_width),
            (6, self.pixel_height),
        ] {
            integer(field, value, &mut out);
        }
        Ok(out)
    }
}

/// 上报给客户端的一块屏幕。`video_track_index` 绑到 SDP 里的第几条视频轨（单屏恒为 0）。
#[derive(Clone, Debug)]
pub struct Screen<'a> {
    pub id: i32,
    pub fps: i32,
    pub resolution: ScreenGeometry,
    pub primary: bool,
    /// 坐标空间缩放比。X11 根窗口就是像素，实测 1.0。
    pub dpr: f64,
    pub display_name: &'a str,
    pub video_track_index: i32,
    /// 官方枚举：0 物理、1 虚拟、2 超大屏。
    pub screen_type: i32,
    /// 官方枚举：0 未知、1 内建、2 非内建。
    pub builtin_screen_type: i32,
}

impl Screen<'_> {
    fn encode(&self) -> Result<Vec<u8>, String> {
        if self.id < 0 || self.fps <= 0 || self.video_track_index < -1 {
            return Err("屏幕 id 非负、track ≥ -1、fps 为正".into());
        }
        if !self.dpr.is_finite() || self.dpr <= 0.0 {
            return Err("dpr 必须为正有限值".into());
        }
        if !(0..=2).contains(&self.screen_type) || !(0..=2).contains(&self.builtin_screen_type) {
            return Err("未知的 Screen 枚举值".into());
        }
        let rect = self.resolution.encode()?;
        let mut out = Vec::new();
        integer(1, self.id, &mut out);
        integer(2, self.fps, &mut out);
        bytes(3, &rect, &mut out);
        // 没有实测的缩放档位就不声明（客户端「其他分辨率」列表为空是诚实的）。
        bytes(4, &rect, &mut out); // current_resolution = 原生
        integer(5, self.screen_type, &mut out);
        bytes(6, &rect, &mut out); // init_resolution
        boolean(7, self.primary, &mut out);
        out.push(65); // field 8 double dpr：fixed64，不是 varint
        out.extend_from_slice(&self.dpr.to_le_bytes());
        // field 9 dpi_scale 省略：没测过显示器 DPI，不吹。
        text(10, self.display_name, &mut out);
        integer(12, self.video_track_index, &mut out);
        integer(13, self.builtin_screen_type, &mut out);
        Ok(out)
    }
}

/// Message.which=7 ScreenSources：屏幕清单 + 当前选中的屏幕 id。
pub fn encode_screen_sources(seq: u64, timestamp: u64, screens: &[Screen<'_>], current_id: i32) -> Result<Vec<u8>, String> {
    if screens.is_empty() || !screens.iter().any(|s| s.id == current_id) {
        return Err("选中的屏幕必须存在于清单里".into());
    }
    let mut payload = Vec::new();
    for screen in screens {
        bytes(1, &screen.encode()?, &mut payload);
    }
    integer(2, current_id, &mut payload);
    Ok(envelope(seq, timestamp, 7, &payload))
}

/// Message.which=8 CaptureChange：声明采集目标。枚举 desktop=0、window=1、none=99。
pub fn encode_capture_change(seq: u64, timestamp: u64, capture_type: i32, type_value: i32, description: &str) -> Result<Vec<u8>, String> {
    if !matches!(capture_type, 0 | 1 | 2 | 3 | 99) {
        return Err("未知的 CaptureChange 类型".into());
    }
    let mut payload = Vec::new();
    integer(1, capture_type, &mut payload);
    integer(2, type_value, &mut payload);
    text(3, description, &mut payload);
    Ok(envelope(seq, timestamp, 8, &payload))
}

/// Message.which=15 SystemStateChange.field3 PermissionState：能力位。
/// 只声明真正做了的事——剪贴板/音频没实现就报 false。
pub struct Permissions {
    pub assistant: bool,
    pub video_capture_allowed: bool,
    pub capture_content_visible: bool,
    pub audio_capture_allowed: bool,
    pub floating_ball: bool,
}

pub fn encode_permissions(seq: u64, timestamp: u64, permissions: Permissions) -> Vec<u8> {
    let mut state = Vec::new();
    for (number, value) in [
        (1, permissions.assistant),
        (2, permissions.video_capture_allowed),
        (3, permissions.capture_content_visible),
        (4, permissions.audio_capture_allowed),
        (5, permissions.floating_ball),
    ] {
        boolean(number, value, &mut state);
    }
    let mut change = Vec::new();
    bytes(3, &state, &mut change);
    envelope(seq, timestamp, 15, &change)
}

/// Message.which=15 SystemStateChange.field1 SecureDesktopState：锁屏状态。
/// Linux 这边没有验过自动解锁，supports_auto_logon 报 false。
pub fn encode_secure_desktop(seq: u64, timestamp: u64, locked: bool, sas_shown: bool, supports_auto_logon: bool) -> Vec<u8> {
    let mut state = Vec::new();
    boolean(1, locked, &mut state);
    boolean(2, sas_shown, &mut state);
    boolean(5, supports_auto_logon, &mut state);
    let mut change = Vec::new();
    bytes(1, &state, &mut change);
    envelope(seq, timestamp, 15, &change)
}

/// Message.which=12 ReportError：把失败原因原样吐给客户端，别静默。
pub fn encode_report_error(seq: u64, timestamp: u64, error_code: i32, error_msg: &str) -> Vec<u8> {
    let mut report = Vec::new();
    integer(2, error_code, &mut report);
    text(3, error_msg, &mut report);
    envelope(seq, timestamp, 12, &report)
}

/// 单屏整桌面流的最小自报清单：id=0、track=0、原生分辨率、dpr=1。
/// 尺寸/fps 不合法时返回空，绝不编一个 1920x1080 上去。
pub fn initial_messages(width: u32, height: u32, fps: u32) -> Vec<(u32, Vec<u8>)> {
    let (Ok(width), Ok(height), Ok(fps)) = (i32::try_from(width), i32::try_from(height), i32::try_from(fps)) else {
        return Vec::new();
    };
    let screen = Screen {
        id: 0,
        fps,
        resolution: ScreenGeometry { left: 0, top: 0, width, height, pixel_width: width, pixel_height: height },
        primary: true,
        dpr: 1.0,
        display_name: "",
        video_track_index: 0,
        screen_type: 0,
        builtin_screen_type: 0,
    };
    let Ok(encoded) = screen.encode() else { return Vec::new() };
    let mut inventory = Vec::new();
    bytes(1, &encoded, &mut inventory);
    vec![(7, inventory), (8, Vec::new())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::decode_wire;

    fn payload_of(message: &[u8], which: u32) -> Vec<(u32, u64)> {
        let fields = decode_wire(message).unwrap();
        let nested = fields
            .iter()
            .find(|f| f.number == which)
            .and_then(|f| match &f.value { crate::wire::WireValue::Bytes(b) => Some(*b), _ => None })
            .unwrap();
        decode_wire(nested)
            .unwrap()
            .iter()
            .filter_map(|f| match &f.value { crate::wire::WireValue::Varint(v) => Some((f.number, *v)), _ => None })
            .collect()
    }

    #[test]
    fn envelope_carries_seq_timestamp_and_nested_payload() {
        let message = envelope(7, 1234, 8, &[8, 1]);
        let fields = decode_wire(&message).unwrap();
        assert_eq!(fields[0].number, 1);
        assert_eq!(fields[1].number, 2);
        assert_eq!(fields[2].number, 8);
        assert_eq!(payload_of(&message, 8), vec![(1, 1)]);
    }

    #[test]
    fn geometry_rejects_non_positive_dimensions() {
        let bad = ScreenGeometry { left: 0, top: 0, width: 0, height: 900, pixel_width: 1600, pixel_height: 900 };
        assert!(bad.encode().is_err());
        let ok = ScreenGeometry { left: 0, top: 0, width: 1600, height: 900, pixel_width: 1600, pixel_height: 900 };
        // left/top 都是 0 → 整字段省略；只剩宽/高/像素宽/像素高四个 varint 字段。
        assert_eq!(ok.encode().unwrap().len(), 12);
    }

    #[test]
    fn screen_advertises_measured_geometry_and_dpr_fixed64() {
        let screen = Screen {
            id: 0,
            fps: 30,
            resolution: ScreenGeometry { left: 0, top: 0, width: 3200, height: 1800, pixel_width: 3200, pixel_height: 1800 },
            primary: true,
            dpr: 1.0,
            display_name: "",
            video_track_index: 0,
            screen_type: 0,
            builtin_screen_type: 0,
        };
        let message = encode_screen_sources(1, 2, &[screen], 0).unwrap();
        let sources = decode_wire(&message).unwrap();
        let payload = sources.iter().find(|f| f.number == 7).unwrap();
        let crate::wire::WireValue::Bytes(inner) = &payload.value else { panic!("which 7 必须是嵌套消息") };
        let fields = decode_wire(inner).unwrap();
        // 选中的屏幕 id 是 0：protobuf 的标量默认值不上线，所以只发清单本身。
        assert_eq!(fields.iter().filter(|f| f.number == 1).count(), 1);
        assert!(!fields.iter().any(|f| f.number == 2));
        let screen_bytes = fields.iter().find(|f| f.number == 1).unwrap();
        let crate::wire::WireValue::Bytes(body) = &screen_bytes.value else { panic!() };
        let encoded = decode_wire(body).unwrap();
        assert!(encoded.iter().any(|f| f.number == 2 && matches!(f.value, crate::wire::WireValue::Varint(30))));
        // field 8 dpr 是 fixed64
        assert!(encoded.iter().any(|f| f.number == 8 && matches!(f.value, crate::wire::WireValue::Fixed64(_))));
        assert!(encoded.iter().any(|f| f.number == 7)); // primary
        // video_track_index=0 是标量默认值：和 current_screen_id 一样不上线。
        assert!(!encoded.iter().any(|f| f.number == 12));
        assert!(!encoded.iter().any(|f| f.number == 9)); // 不吹 dpi_scale
    }

    #[test]
    fn selected_screen_id_is_carried_when_not_zero() {
        let screen = Screen {
            id: 2,
            fps: 15,
            resolution: ScreenGeometry { left: 0, top: 0, width: 1920, height: 1080, pixel_width: 1920, pixel_height: 1080 },
            primary: false,
            dpr: 1.0,
            display_name: "DP-4",
            video_track_index: 1,
            screen_type: 0,
            builtin_screen_type: 2,
        };
        let message = encode_screen_sources(1, 2, &[screen.clone()], 2).unwrap();
        let which7 = decode_wire(&message).unwrap().into_iter().find(|f| f.number == 7).unwrap();
        let crate::wire::WireValue::Bytes(inner) = &which7.value else { panic!() };
        let fields = decode_wire(inner).unwrap();
        assert!(fields.iter().any(|f| f.number == 2 && matches!(f.value, crate::wire::WireValue::Varint(2))));
        // track 1 不是默认值，必须上线。
        let crate::wire::WireValue::Bytes(body) = &fields.iter().find(|f| f.number == 1).unwrap().value else { panic!() };
        assert!(decode_wire(body).unwrap().iter().any(|f| f.number == 12 && matches!(f.value, crate::wire::WireValue::Varint(1))));
        // 清单之外还有第二块屏幕时也必须各占一个 field 1。
        let two = encode_screen_sources(1, 2, &[screen.clone(), screen], 2).unwrap();
        let which7 = decode_wire(&two).unwrap().into_iter().find(|f| f.number == 7).unwrap();
        let crate::wire::WireValue::Bytes(inner) = &which7.value else { panic!() };
        assert_eq!(decode_wire(inner).unwrap().iter().filter(|f| f.number == 1).count(), 2);
    }

    #[test]
    fn capture_change_declares_only_known_types() {
        let message = encode_capture_change(3, 4, 0, 0, "Linux desktop").unwrap();
        assert_eq!(payload_of(&message, 8), vec![]); // type=0 全默认，只留描述文本
        assert!(encode_capture_change(3, 4, 9, 0, "").is_err());
    }

    #[test]
    fn permissions_and_secure_desktop_state_fit_their_envelopes() {
        let message = encode_permissions(9, 10, Permissions { assistant: false, video_capture_allowed: true, capture_content_visible: true, audio_capture_allowed: false, floating_ball: false });
        let which15 = decode_wire(&message).unwrap();
        let crate::wire::WireValue::Bytes(change) = &which15.iter().find(|f| f.number == 15).unwrap().value else { panic!() };
        let change_fields = decode_wire(change).unwrap();
        assert_eq!(change_fields[0].number, 3);
        let state = match &change_fields[0].value { crate::wire::WireValue::Bytes(b) => *b, _ => panic!() };
        assert_eq!(decode_wire(state).unwrap().iter().filter_map(|f| match &f.value { crate::wire::WireValue::Varint(v) => Some(*v), _ => None }).collect::<Vec<_>>(), vec![1, 1]);

        let locked = encode_secure_desktop(1, 2, true, false, false);
        let which15 = decode_wire(&locked).unwrap();
        let crate::wire::WireValue::Bytes(change) = &which15.iter().find(|f| f.number == 15).unwrap().value else { panic!() };
        let change_fields = decode_wire(change).unwrap();
        assert_eq!(change_fields[0].number, 1);
        let state = match &change_fields[0].value { crate::wire::WireValue::Bytes(b) => *b, _ => panic!() };
        assert_eq!(decode_wire(state).unwrap().iter().filter_map(|f| match &f.value { crate::wire::WireValue::Varint(v) => Some((f.number, *v)), _ => None }).collect::<Vec<_>>(), vec![(1, 1)]);
    }

    #[test]
    fn report_error_carries_code_and_message() {
        let message = encode_report_error(5, 6, 42, "capture unavailable");
        let which12 = decode_wire(&message).unwrap();
        let crate::wire::WireValue::Bytes(body) = &which12.iter().find(|f| f.number == 12).unwrap().value else { panic!() };
        let fields = decode_wire(body).unwrap();
        assert!(fields.iter().any(|f| f.number == 2 && matches!(f.value, crate::wire::WireValue::Varint(42))));
        assert!(fields.iter().any(|f| f.number == 3 && matches!(&f.value, crate::wire::WireValue::Bytes(b) if *b == b"capture unavailable")));
    }

    #[test]
    fn initial_messages_refuse_bad_geometry_without_inventing_values() {
        let messages = initial_messages(1920, 1080, 30);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].0, 7);
        assert_eq!(messages[1].0, 8);
        assert!(initial_messages(0, 1080, 30).is_empty());
        assert!(initial_messages(1920, 1080, 0).is_empty());
    }
}
