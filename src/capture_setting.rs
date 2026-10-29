//! 采集设置的通告面：`Message` 18 `CodecNegotiation`、可选缩放尺寸、显示器刷新率。
//!
//! RPC 的**解析**在 `control.rs`；这里管的是**通告**——客户端的帧率/画质菜单
//! 按宿主声明来解锁：没发 18（CodecNegotiation）的老式宿主会被锁在默认档
//! （"被控端受限不支持指定帧率"就是这条通告缺失的症状，参照实现每次
//! CONTROL 初始化都发 18，fps 报显示器的真实刷新率）。

/// 参照实现的缩放档（客户端"其他分辨率"菜单照这份列表渲染）：原生尺寸的 5/6..2/6（偶数对齐），太小的一档直接砍掉。
/// 客户端"其他分辨率"菜单照这份列表渲染；原生由 resolution 字段本身给出。
pub fn scaled_sizes(native: (u32, u32)) -> Vec<(u32, u32)> {
    [5u32, 4, 3, 2]
        .iter()
        .map(|sixths| (native.0 * sixths / 6 & !1, native.1 * sixths / 6 & !1))
        .filter(|(w, h)| *w >= 640 && *h >= 360)
        .collect()
}

/// int64 语义是 zigzag。
fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 {
        out.push((v as u8 & 127) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_varint(field: u32, v: u64, out: &mut Vec<u8>) {
    varint(u64::from(field) << 3, out);
    varint(v, out);
}

fn put_bytes(field: u32, v: &[u8], out: &mut Vec<u8>) {
    varint((field << 3 | 2).into(), out);
    varint(v.len() as u64, out);
    out.extend_from_slice(v);
}

/// `Message` 18 `CodecNegotiation` payload：`{1 encoder_cap_list: EncoderCap{1 capture_mode(1=DESKTOP),
/// 2 fps, 3 codec_type(1=H264,2=H265), 4 width, 5 height}}`（官方 Windows 宿主的描述符）。
/// 客户端只从这份列表里挑 codec，并按 fps 上限解锁帧率菜单。
pub fn codec_negotiation(size: (u32, u32), fps: u32, hevc: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for codec in [Some(1u64), hevc.then_some(2)].into_iter().flatten() {
        let mut cap = Vec::new();
        put_varint(1, 1, &mut cap);
        put_varint(2, u64::from(fps), &mut cap);
        put_varint(3, codec, &mut cap);
        put_varint(4, u64::from(size.0), &mut cap);
        put_varint(5, u64::from(size.1), &mut cap);
        put_bytes(1, &cap, &mut out);
    }
    out
}

/// 上报给客户端的刷新率（能力 / 屏幕清单 / codec 协商三处共用），照参照实现报
/// 显示器的真实模式（官方 Mac 报 120）：X11 走 `xrandr` 当前模式，拿不到退 60。
/// `UU_DISPLAY_FPS` 强制覆盖。
pub fn display_fps(display: Option<&str>) -> u32 {
    if let Ok(v) = std::env::var("UU_DISPLAY_FPS") {
        if let Ok(f) = v.trim().parse::<f64>() {
            if f > 0.0 {
                return f.round().clamp(1.0, 1000.0) as u32;
            }
        }
    }
    match read_xrandr_refresh(display) {
        Some(f) => f.round().clamp(1.0, 1000.0) as u32,
        None => 60,
    }
}

/// `xrandr` 主输出当前模式的刷新率；解析失败一律 None，绝不编一个 240 上去。
fn read_xrandr_refresh(display: Option<&str>) -> Option<f64> {
    let mut command = std::process::Command::new("xrandr");
    if let Some(d) = display {
        command.env("DISPLAY", d);
    }
    let out = command.output().ok()?;
    parse_xrandr_primary_refresh(&String::from_utf8_lossy(&out.stdout))
}

fn parse_xrandr_primary_refresh(text: &str) -> Option<f64> {
    // `xrandr` 的刷新率在输出行之后的缩进 mode 行里：`  2560x1440     240.00* ...`。
    // 带星号的是当前模式；输出行本身没有刷新值。
    let lines: Vec<&str> = text.lines().collect();
    let idx = lines.iter().position(|l| l.contains(" connected "))?;
    for line in &lines[idx + 1..] {
        if !line.starts_with("  ") {
            break;
        }
        if let Some(refresh) = starred_refresh(line) {
            return Some(refresh);
        }
    }
    None
}

fn starred_refresh(line: &str) -> Option<f64> {
    // 线上形态是 `240.00*+`（星=当前模式，加号=首选），星可能不在末尾。
    line.split_whitespace().find_map(|tok| {
        let core = tok.split('*').next()?;
        core.contains('.').then(|| core.parse::<f64>().ok()).flatten()
    })
}

/// 官方 `ConnectOptions`（be-controlled 附件 protobuf）里客户端随拉流请求带来的初始设置与解码能力。
/// schema 直接取自 Windows GameViewerServer.exe 内嵌描述符（不是参照实现的推断）：
/// ```text
/// ConnectOptions{1 capture_type, 3 capture_params, 4 decoder_cap_list(repeated), 11 feature_flag, 12 client_version}
/// CaptureParams{1 fps(FPS 枚举), 2 video_quality, 3 cursor_capture, 4 choose_resolution_type,
///                5 local_resolution, 6 choose_resolution, 7 chroma_format, 8 max_custom_bitrate,
///                9 enable_hdr, 10 auto_frame_quality, 11 fpsCount}
/// DecoderCap{1 fps, 2 codec_type, 3 resolution_width, 4 resolution_height, 5 chroma_format}
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct InitialSettings {
    pub fps: Option<u32>,
    pub quality: Option<u64>,
    pub chroma: Option<u64>,
    pub hdr: bool,
    pub custom_bps: Option<u64>,
    /// choose_resolution_type == 3（FOLLOW_REMOTE）：跟随被控端原生分辨率。
    pub follow_remote: bool,
    pub resolution: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DecoderCap {
    pub fps: u32,
    /// 1 = H264、2 = H265。
    pub codec: u64,
    pub width: u32,
    pub height: u32,
    /// 1 = 4:2:0、3 = 4:4:4。
    pub chroma: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectOptions {
    pub capture_type: u64,
    pub initial: InitialSettings,
    pub decoder_caps: Vec<DecoderCap>,
    /// 客户端声明的 ff_capture_setting（FeatureFlag field 1）；0 = 它不走采集设置 RPC。
    pub client_capture_setting_version: u64,
    pub client_version: String,
}

/// FPS 枚举（0 未知 / 1=30 / 2=60 / 3=90 / 4=144）→ 真实帧率。
pub fn fps_from_enum(value: u64) -> Option<u32> {
    match value {
        1 => Some(30),
        2 => Some(60),
        3 => Some(90),
        4 => Some(144),
        _ => None,
    }
}

/// 真实帧率 → FPS 枚举。官方 CaptureSettingSync 的 field 1 是**枚举**不是帧率数字，
/// 报 60 客户端解成未知档，会认为被控端没接受它指定的帧率。
pub fn fps_to_enum(fps: u32) -> u64 {
    match fps {
        0..=30 => 1,
        31..=60 => 2,
        61..=90 => 3,
        _ => 4,
    }
}

/// 解 `ConnectOptions`。解不出结构返回 None（调用方按默认处理，不影响连接）。
pub fn connect_options(data: &[u8]) -> Option<ConnectOptions> {
    use crate::wire::{decode_wire, nested, varint_field, WireValue};
    let fields = decode_wire(data).ok()?;
    let mut out = ConnectOptions {
        capture_type: varint_field(&fields, 1).unwrap_or(0),
        ..Default::default()
    };
    if let Some(params) = nested(&fields, 3) {
        // fpsCount(field 11) 是真实帧率，优先于枚举档。
        out.initial.fps = varint_field(&params, 11).map(|v| v as u32).or_else(|| fps_from_enum(varint_field(&params, 1).unwrap_or(0)));
        out.initial.quality = varint_field(&params, 2);
        out.initial.chroma = varint_field(&params, 7);
        out.initial.custom_bps = varint_field(&params, 8);
        out.initial.hdr = varint_field(&params, 9).unwrap_or(0) != 0;
        out.initial.follow_remote = varint_field(&params, 4) == Some(3);
        out.initial.resolution = nested(&params, 6).and_then(|r| {
            let w = varint_field(&r, 1)?;
            let h = varint_field(&r, 2)?;
            Some((w as u32, h as u32))
        });
    }
    for cap in fields.iter().filter(|f| matches!(f.value, WireValue::Bytes(_)) && f.number == 4) {
        let crate::wire::WireValue::Bytes(raw) = &cap.value else { continue };
        let Ok(list) = decode_wire(raw) else { continue };
        out.decoder_caps.push(DecoderCap {
            fps: varint_field(&list, 1).unwrap_or(0) as u32,
            codec: varint_field(&list, 2).unwrap_or(1),
            width: varint_field(&list, 3).unwrap_or(0) as u32,
            height: varint_field(&list, 4).unwrap_or(0) as u32,
            chroma: varint_field(&list, 5).unwrap_or(1),
        });
    }
    if let Some(flag) = nested(&fields, 11) {
        out.client_capture_setting_version = varint_field(&flag, 1).unwrap_or(0);
    }
    if let Some(crate::wire::WireValue::Bytes(v)) = fields.iter().find(|f| f.number == 12).map(|f| &f.value) {
        out.client_version = String::from_utf8_lossy(v).into_owned();
    }
    Some(out)
}

/// 客户端解码能力与宿主编码能力求交：真彩要两边都支持，帧率与尺寸取较小值。
/// 参照实现不做这一步（它只看宿主能力），官方宿主解析 decoder_cap_list 后求交。
pub fn decoder_allows_full_chroma(caps: &[DecoderCap]) -> bool {
    caps.iter().any(|c| c.chroma == 3)
}

pub fn decoder_max_fps(caps: &[DecoderCap], fallback: u32) -> u32 {
    caps.iter().map(|c| c.fps).filter(|f| *f > 0).min().unwrap_or(fallback)
}

pub fn decoder_supports_codec(caps: &[DecoderCap], codec: u64) -> bool {
    caps.iter().any(|c| c.codec == codec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_sizes_downscales_even_and_drops_tiny() {
        assert_eq!(
            scaled_sizes((2560, 1440)),
            vec![(2132, 1200), (1706, 960), (1280, 720), (852, 480)]
        );
        // 3/6 = 640x360 正好压线保留；2/6 = 426x240 < 下限，被砍。
        assert_eq!(scaled_sizes((1280, 720)), vec![(1066, 600), (852, 480), (640, 360)]);
    }

    #[test]
    fn connect_options_carries_initial_settings_and_decoder_caps() {
        // Windows 官方 ConnectOptions 实测形状（journal 里的 be-controlled 附件）：
        // {1 capture_type=1, 3 CaptureParams{2 quality=3,4 choose=2,7 chroma=1,11 fpsCount=60},
        //  4 DecoderCap{1 fps=144,2 codec=1,3 3840,4 2160,5 chroma=3}, 11 FeatureFlag{1=4}, 12 "4.37.0"}
        let mut data = vec![0x08, 0x01];
        let params = [0x10, 0x03, 0x20, 0x02, 0x38, 0x01, 0x58, 0x3c];
        data.push(0x1a); data.push(params.len() as u8); data.extend(params);
        // varint：3840 = 0x80,0x1e；2160 = 0xf0,0x10；fps 144 = 0x90,0x01。
        let cap = [0x08, 0x90, 0x01, 0x10, 0x01, 0x18, 0x80, 0x1e, 0x20, 0xf0, 0x10, 0x28, 0x03];
        data.push(0x22); data.push(cap.len() as u8); data.extend(cap);
        let flag = [0x08, 0x04];
        data.push(0x5a); data.push(flag.len() as u8); data.extend(flag);
        let ver = b"4.37.0";
        data.push(0x62); data.push(ver.len() as u8); data.extend(ver);
        let opts = connect_options(&data).unwrap();
        assert_eq!(opts.capture_type, 1);
        assert_eq!(opts.initial.quality, Some(3));
        assert_eq!(opts.initial.fps, Some(60));
        assert_eq!(opts.initial.chroma, Some(1));
        assert!(!opts.initial.follow_remote);
        assert_eq!(opts.decoder_caps.len(), 1);
        assert_eq!(opts.decoder_caps[0], DecoderCap { fps: 144, codec: 1, width: 3840, height: 2160, chroma: 3 });
        assert_eq!(opts.client_capture_setting_version, 4);
        assert_eq!(opts.client_version, "4.37.0");
        // 求交：客户端能解 4:4:4 → 真彩可用；帧率取两边较小值。
        assert!(decoder_allows_full_chroma(&opts.decoder_caps));
        assert_eq!(decoder_max_fps(&opts.decoder_caps, 240), 144);
        assert!(!decoder_supports_codec(&opts.decoder_caps, 2));
    }

    #[test]
    fn codec_negotiation_announces_h264_with_fps_and_size() {
        let out = codec_negotiation((2560, 1440), 240, false);
        let fields = crate::wire::decode_wire(&out).unwrap();
        let crate::wire::WireValue::Bytes(cap) = &fields[0].value else { panic!("cap 不是 bytes") };
        let cap = crate::wire::decode_wire(cap).unwrap();
        let nums: Vec<u64> = cap
            .iter()
            .filter_map(|f| match &f.value { crate::wire::WireValue::Varint(v) => Some(*v), _ => None })
            .collect();
        assert_eq!(nums, vec![1, 240, 1, 2560, 1440]);
        // hevc=true 时第二条 cap 的 codec_type 是 2。
        let both = codec_negotiation((2560, 1440), 240, true);
        let fields = crate::wire::decode_wire(&both).unwrap();
        assert_eq!(fields.len(), 2);
        let crate::wire::WireValue::Bytes(cap2) = &fields[1].value else { panic!() };
        let nums2: Vec<u64> = crate::wire::decode_wire(cap2)
            .unwrap()
            .iter()
            .filter_map(|f| match &f.value { crate::wire::WireValue::Varint(v) => Some(*v), _ => None })
            .collect();
        assert_eq!(nums2, vec![1, 240, 2, 2560, 1440]);
    }

    #[test]
    fn xrandr_refresh_reads_starred_mode_of_primary_output() {
        let text = "Screen 0: minimum 320 x 200, current 2560 x 1440, maximum 16384 x 16384\neDP-1 connected primary 2560x1440+0+0 (normal left inverted right x axis y axis) 0mm x 0mm\n   2560x1440     240.00*  165.00  120.00  60.00\n   1920x1080     60.00  50.00\nHDMI-2 disconnected\n";
        assert_eq!(parse_xrandr_primary_refresh(text), Some(240.0));
        // 非主输出也要能读（connected 但无 primary）。
        let laptop = "eDP-1 connected 1920x1080+0+0\n   1920x1080     60.01*  59.97\n";
        assert_eq!(parse_xrandr_primary_refresh(laptop), Some(60.01));
        // 线上真形态：当前模式带 `*+` 后缀。
        let real = "eDP-1 connected primary 2560x1440+0+0 (normal left inverted right x axis y axis) 381mm x 214mm\n   2560x1440    240.00*+  60.00 +  60.00\n   1920x1440     60.00\n";
        assert_eq!(parse_xrandr_primary_refresh(real), Some(240.0));
        assert_eq!(parse_xrandr_primary_refresh("no outputs here"), None);
    }
}
