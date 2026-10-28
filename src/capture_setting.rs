//! 采集设置的通告面：`Message` 18 `CodecNegotiation`、可选缩放尺寸、显示器刷新率。
//!
//! RPC 的**解析**在 `control.rs`；这里管的是**通告**——客户端的帧率/画质菜单
//! 按宿主声明来解锁：没发 18（CodecNegotiation）的老式宿主会被锁在默认档
//! （"被控端受限不支持指定帧率"就是这条通告缺失的症状，参照实现每次
//! CONTROL 初始化都发 18，fps 报显示器的真实刷新率）。

/// 参照实现的缩放档：原生尺寸的 5/6..2/6（偶数对齐），太小的一档直接砍掉。
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
