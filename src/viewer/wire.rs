//! 控制端侧的协议构造：ConnectOptions(protobuf)、control 头、RPC 设置、echo。
//! 全部按官方 Windows/Mac 客户端线上观测过的字段号写，不发明格式。
use serde_json::{json, Value};

use crate::wire::{decode_wire, WireValue};

pub const CHANNELS: [&str; 5] = ["CONTROL_DATA_CHANNEL", "TEXT_DATA_CHANNEL", "STREAMER_DATA_CHANNEL", "FILE_DATA_CHANNEL", "BINARY_DATA_CHANNEL"];

pub fn uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 15) | 64;
    b[8] = (b[8] & 63) | 128;
    let s = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &s[..8], &s[8..12], &s[12..16], &s[16..20], &s[20..])
}
pub fn varint(mut n: u64, out: &mut Vec<u8>) { while n >= 128 { out.push(n as u8 | 128); n >>= 7; } out.push(n as u8); }
pub fn int(field: u32, value: u64, out: &mut Vec<u8>) { varint(u64::from(field) << 3, out); varint(value, out); }
pub fn bytes(field: u32, value: &[u8], out: &mut Vec<u8>) { varint((u64::from(field) << 3) | 2, out); varint(value.len() as u64, out); out.extend(value); }
/// 官方客户端 Message 外壳：字段 1 = seq，字段 <kind> = 负载。
pub fn envelope(seq: u64, kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    int(1, seq, &mut out);
    bytes(kind, payload, &mut out);
    out
}

pub struct DecoderCap { pub fps: u32, pub codec: u32, pub width: u32, pub height: u32, pub chroma: u32 }
pub struct Shape {
    pub platform: u32,
    pub client_version: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub chroma: u32,
    pub decoders: Vec<DecoderCap>,
    pub display_modes: Vec<[u32; 3]>,
    pub displays: Vec<Value>,
    pub codecs: Vec<Value>,
}

impl Shape {
    /// 本机控制端的能力：H264 软解、4:2:0、上限 4K@144。不报 HEVC/真彩——
    /// openh264 只出 4:2:0，报了会被被控端当真去编 4:4:4/HEVC 然后解不动。
    pub fn local(width: u32, height: u32, fps: u32) -> Self {
        Self {
            platform: 4,
            client_version: "4.37.0.1062".into(),
            width,
            height,
            fps,
            chroma: 1,
            decoders: vec![DecoderCap { fps: 144, codec: 1, width: 3840, height: 2160, chroma: 1 }],
            display_modes: vec![[1920, 1080, 60], [2560, 1440, 60], [1280, 720, 60]],
            displays: vec![json!({"id": 0, "fps": 144, "type": 1, "hdr": 0})],
            codecs: vec![json!({"video_codec": 1, "chroma_sampling": 1, "codec_impl": 5, "bit_depth": 8, "width": 3840, "height": 2160})],
        }
    }

    pub fn connect_options(&self, device: &str) -> Vec<u8> {
        let mut out = Vec::new();
        int(1, 1, &mut out); // capture_type = DESKTOP
        int(2, u64::MAX, &mut out);
        let mut params = Vec::new();
        int(1, 2, &mut params); // fps 枚举 2 = 60
        int(2, 4, &mut params); // video_quality 4 = 原画
        int(4, 2, &mut params); // choose_resolution_type 2 = 本地自适应
        let mut size = Vec::new();
        int(1, self.width.into(), &mut size);
        int(2, self.height.into(), &mut size);
        bytes(5, &size, &mut params);
        bytes(6, &[], &mut params);
        int(7, self.chroma.into(), &mut params);
        int(9, 1, &mut params);
        int(11, self.fps.into(), &mut params); // fpsCount 真实帧率
        bytes(3, &params, &mut out);
        for d in &self.decoders {
            let mut cap = Vec::new();
            for (f, v) in [(1, d.fps), (2, d.codec), (3, d.width), (4, d.height)] { int(f, v.into(), &mut cap); }
            int(5, d.chroma.into(), &mut cap);
            bytes(4, &cap, &mut out);
        }
        for mode in &self.display_modes {
            let mut value = Vec::new();
            for (i, v) in mode.iter().enumerate() { int(i as u32 + 1, (*v).into(), &mut value); }
            bytes(6, &value, &mut out);
        }
        bytes(7, &size, &mut out);
        int(8, self.platform.into(), &mut out);
        bytes(9, device.as_bytes(), &mut out);
        int(10, 1, &mut out);
        let mut flags = Vec::new();
        for [f, v] in [[1u32, 6u32], [2, 2], [3, 1], [4, 2], [6, 2], [7, 2], [9, 1]] { int(f, v.into(), &mut flags); }
        bytes(11, &flags, &mut out);
        bytes(12, self.client_version.as_bytes(), &mut out);
        out
    }

    /// 官方 Control::SerializeSioMsg 只有这三个键；ice_id 走 be-controlled，ack 会另发一个。
    pub fn control_header(&self, app_control_id: &str) -> Value {
        let streamer = json!({"control_id": app_control_id, "device_capability": {"display_info": self.displays, "ice_id": "", "video_codec_capability": self.codecs}});
        json!({"app_control_id": app_control_id, "app_data": {"_placeholder": true, "num": 0}, "streamer_data": streamer.to_string()})
    }
}

fn header(id: u64) -> Vec<u8> { let mut h = Vec::new(); int(1, id, &mut h); h }

/// RpcRequest（Message 21）。被控端按 body 里的 bytes 字段号分派：2=采集设置 3=帧率 4=画质。
pub fn rpc_quality(seq: u64, request_id: u64, quality: u64) -> Vec<u8> {
    let mut q = Vec::new(); int(1, quality, &mut q);
    let mut rpc = Vec::new(); bytes(1, &header(request_id), &mut rpc); bytes(4, &q, &mut rpc);
    envelope(seq, 21, &rpc)
}
/// fps_menu_index：1=30 2=60 3=90 4=144（菜单发序号不是帧率）。
pub fn rpc_fps(seq: u64, request_id: u64, index: u64) -> Vec<u8> {
    let mut f = Vec::new(); int(1, index, &mut f);
    let mut rpc = Vec::new(); bytes(1, &header(request_id), &mut rpc); bytes(3, &f, &mut rpc);
    envelope(seq, 21, &rpc)
}
/// Some((w,h)) + resolution_type 4；None = 跟随被控端（resolution_type 3）。
pub fn rpc_resolution(seq: u64, request_id: u64, size: Option<(u32, u32)>) -> Vec<u8> {
    let mut s = Vec::new();
    match size {
        Some((w, h)) => { int(5, w.into(), &mut s); int(6, h.into(), &mut s); int(10, 4, &mut s); }
        None => int(10, 3, &mut s),
    }
    let mut rpc = Vec::new(); bytes(1, &header(request_id), &mut rpc); bytes(2, &s, &mut rpc);
    envelope(seq, 21, &rpc)
}
/// 真彩开关：CaptureSettingRequest 字段 7 = ChromaFormat（3=4:4:4，1=4:2:0）。
pub fn rpc_chroma(seq: u64, request_id: u64, chroma: u64) -> Vec<u8> {
    let mut s = Vec::new(); int(7, chroma, &mut s);
    let mut rpc = Vec::new(); bytes(1, &header(request_id), &mut rpc); bytes(2, &s, &mut rpc);
    envelope(seq, 21, &rpc)
}

pub fn session_features() -> Vec<u8> {
    let mut flags = Vec::new();
    for [f, v] in [[1u32, 6u32], [2, 2], [3, 1], [4, 2], [6, 2], [7, 2], [9, 1]] { int(f, v.into(), &mut flags); }
    flags
}
pub fn echo_request(seq: u64) -> Vec<u8> {
    let mut action = Vec::new();
    int(1, 0, &mut action);
    bytes(4, &session_features(), &mut action);
    envelope(seq, 3, &action)
}
pub fn echo_response(seq: u64, request_seq: u64) -> Vec<u8> {
    let mut action = Vec::new();
    int(1, 1, &mut action);
    bytes(2, format!("{{\"seq\":{request_seq}}}").as_bytes(), &mut action);
    bytes(4, &session_features(), &mut action);
    envelope(seq, 3, &action)
}

/// 被控端 echo 请求（SimpleAction action 0）：取出它的 seq 以便按 seq 应答。
pub fn echo_request_seq(data: &[u8]) -> Option<u64> {
    let fields = decode_wire(data).ok()?;
    let seq = fields.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Varint(v)) => Some(*v), _ => None })?;
    fields.iter().find_map(|f| match (f.number, &f.value) {
        // proto3 省略默认值 0，所以没有字段 1 的 SimpleAction 也是 echo 请求。
        (3, WireValue::Bytes(action)) => decode_wire(action).ok()?.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Varint(v)) => Some(*v), _ => None }).unwrap_or(0).eq(&0).then_some(seq),
        _ => None,
    })
}
/// 我们自己 ping 的回包（action 1 + args.seq）→ 返回 seq 用来算 RTT。
pub fn echo_response_seq(data: &[u8]) -> Option<u64> {
    let fields = decode_wire(data).ok()?;
    fields.iter().find_map(|f| match (f.number, &f.value) {
        (3, WireValue::Bytes(action)) => {
            let act = decode_wire(action).ok()?;
            if !act.iter().any(|f| f.number == 1 && f.value == WireValue::Varint(1)) { return None; }
            act.iter().find_map(|f| match (f.number, &f.value) {
                (2, WireValue::Bytes(args)) => serde_json::from_slice::<Value>(args).ok().and_then(|v| v.get("seq").and_then(Value::as_u64)),
                _ => None,
            })
        }
        _ => None,
    })
}

pub struct CursorShape { pub hotspot_x: i32, pub hotspot_y: i32, pub width: i32, pub height: i32, pub png: Vec<u8> }

/// Message 15 光标形状：change{2: shape{1,2 hotspot; 3,4 size; 5 png}}。
pub fn parse_cursor(data: &[u8]) -> Option<CursorShape> {
    let fields = decode_wire(data).ok()?;
    let change = decode_wire(nested(&fields, 15)?).ok()?;
    let shape = decode_wire(nested(&change, 2)?).ok()?;
    let png = match shape.iter().find(|f| f.number == 5).map(|f| &f.value) { Some(WireValue::Bytes(b)) => b.to_vec(), _ => return None };
    let number = |n: u32| shape.iter().find(|f| f.number == n).and_then(|f| match &f.value { WireValue::Varint(v) => Some(*v as i32), _ => None }).unwrap_or(0);
    Some(CursorShape { hotspot_x: number(1), hotspot_y: number(2), width: number(3), height: number(4), png })
}

fn nested<'a>(fields: &[crate::wire::WireField<'a>], number: u32) -> Option<&'a [u8]> {
    fields.iter().find(|f| f.number == number).and_then(|f| match &f.value { WireValue::Bytes(b) => Some(&b[..]), _ => None })
}

/// Message 18（编码能力清单）/ 25（实际采集同步）：只取来记日志，RTP 协商才是准。
pub fn media_control_note(data: &[u8]) -> Option<(Option<u32>, Option<u32>)> {
    let fields = decode_wire(data).ok()?;
    let mut codec_list = None;
    let mut chroma = None;
    for field in &fields {
        if let WireValue::Bytes(body) = &field.value {
            match field.number {
                18 => {
                    if let Ok(caps) = decode_wire(body) {
                        let mut mask = 0;
                        for cap in caps {
                            if cap.number == 1 {
                                if let WireValue::Bytes(c) = &cap.value {
                                if let Ok(inner) = decode_wire(c) {
                                    for f in inner {
                                        if f.number == 3 {
                                            if let WireValue::Varint(v) = f.value {
                                                if v == 1 { mask |= 1 } else if v == 2 { mask |= 2 }
                                            }
                                        }
                                    }
                                }
                                }
                            }
                        }
                        codec_list = Some(mask);
                    }
                }
                25 => {
                    if let Ok(inner) = decode_wire(body) {
                        for f in inner {
                            if f.number == 3 {
                                if let WireValue::Varint(v) = f.value {
                                    if [1, 3].contains(&v) { chroma = Some(v as u32); }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Some((codec_list, chroma))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn envelope_round_trips_through_host_parser() {
        // 被控端 decode_rpc 认 Message 21 的字段号；这里验证控制端产出的包能被它解开。
        let msg = rpc_fps(5, 42, 3);
        let parsed = crate::control::decode_rpc(&msg).unwrap();
        assert_eq!(parsed.request_id, 42);
        assert_eq!(parsed.case, 3);
        assert_eq!(parsed.patch.fps, Some(90));
        let msg = rpc_quality(6, 43, 4);
        let parsed = crate::control::decode_rpc(&msg).unwrap();
        assert_eq!(parsed.case, 4);
        assert_eq!(parsed.patch.quality, Some(4));
        let msg = rpc_resolution(7, 44, Some((1920, 1080)));
        let parsed = crate::control::decode_rpc(&msg).unwrap();
        assert_eq!(parsed.case, 2);
        assert_eq!(parsed.patch.width, Some(1920));
        assert_eq!(parsed.patch.height, Some(1080));
        assert!(!parsed.patch.follow_host);
        let msg = rpc_resolution(8, 45, None);
        let parsed = crate::control::decode_rpc(&msg).unwrap();
        assert!(parsed.patch.follow_host);
        let msg = rpc_chroma(9, 46, 3);
        let parsed = crate::control::decode_rpc(&msg).unwrap();
        assert_eq!(parsed.patch.chroma, Some(3));
    }
    #[test]
    fn echo_round_trip() {
        let req = echo_request(123);
        assert!(matches!(crate::control::decode_control(&req), Ok(crate::control::ControlMessage::Action(_))));
        assert_eq!(echo_request_seq(&req), Some(123));
        let resp = echo_response(456, 123);
        assert_eq!(echo_response_seq(&resp), Some(123));
        assert_eq!(echo_request_seq(&resp), None);
    }
    #[test]
    fn cursor_parses_host_encoder_output() {
        let sprite = crate::cursor::Sprite { x: 0, y: 0, width: 4, height: 2, hotspot_x: 1, hotspot_y: 1, serial: 0, argb: vec![0u32; 8] };
        let msg = crate::cursor::encode_cursor_shape(9, 10, &sprite);
        let cursor = parse_cursor(&msg).unwrap();
        assert_eq!(cursor.width, 4);
        assert_eq!(cursor.height, 2);
        assert_eq!(cursor.hotspot_x, 1);
        assert!(!cursor.png.is_empty());
    }
}
