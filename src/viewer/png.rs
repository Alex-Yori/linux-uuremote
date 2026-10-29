//! 极简 PNG 解码：只认被控端光标精灵产出的 RGBA8/非隔行 PNG（IHDR+IDAT+IEND）。
//! 五种行滤波器全支持；其余情况报错了事，不做容错性猜测。
use flate2::read::ZlibDecoder;
use std::io::Read;

pub struct Image { pub width: u32, pub height: u32, pub rgba: Vec<u8> }

pub fn decode(png: &[u8]) -> Result<Image, String> {
    if png.len() < 8 || &png[..8] != b"\x89PNG\r\n\x1a\n" { return Err("不是 PNG 数据".into()); }
    let mut pos = 8usize;
    let mut width = 0u32; let mut height = 0u32;
    let mut depth = 0u8; let mut color = 0u8; let mut interlace = 0u8;
    let mut idat = Vec::new();
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes([png[pos], png[pos+1], png[pos+2], png[pos+3]]) as usize;
        let kind = &png[pos+4..pos+8];
        let data_start = pos + 8;
        if data_start + len + 4 > png.len() { return Err("PNG 块截断".into()); }
        let data = &png[data_start..data_start+len];
        match kind {
            b"IHDR" => {
                if len != 13 { return Err("IHDR 长度不对".into()); }
                width = u32::from_be_bytes([data[0],data[1],data[2],data[3]]);
                height = u32::from_be_bytes([data[4],data[5],data[6],data[7]]);
                depth = data[8]; color = data[9]; interlace = data[12];
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        pos = data_start + len + 4;
    }
    if width == 0 || height == 0 || width > 4096 || height > 4096 { return Err("光标精灵尺寸超限".into()); }
    if depth != 8 || color != 6 || interlace != 0 { return Err("只支持 RGBA8 非隔行 PNG".into()); }
    let raw_len = height as usize * (1 + width as usize * 4);
    let mut raw = Vec::with_capacity(raw_len);
    ZlibDecoder::new(idat.as_slice()).take(raw_len as u64 + 1).read_to_end(&mut raw).map_err(|e| format!("inflate 失败: {e}"))?;
    if raw.len() != raw_len { return Err("PNG 解压长度不匹配".into()); }
    let stride = width as usize * 4;
    let mut out = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        let row = &raw[y*(stride+1)..y*(stride+1)+stride+1];
        let filter = row[0];
        let (above, cur) = out.split_at_mut(y * stride);
        let prev: &[u8] = if y == 0 { &[] } else { &above[(y - 1) * stride..] };
        for x in 0..stride {
            let raw_v = row[1+x] as i32;
            let a = if x >= 4 { cur[x-4] as i32 } else { 0 };
            let b = prev.get(x).copied().unwrap_or(0) as i32;
            let c = if x >= 4 { prev.get(x-4).copied().unwrap_or(0) as i32 } else { 0 };
            let v = match filter {
                0 => raw_v,
                1 => raw_v + a,
                2 => raw_v + b,
                3 => raw_v + (a + b) / 2,
                4 => { // Paeth
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p-a).abs(), (p-b).abs(), (p-c).abs());
                    let pred = if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c };
                    raw_v + pred
                }
                _ => return Err("未知 PNG 滤波器".into()),
            };
            cur[x] = (v & 0xff) as u8;
        }
    }
    Ok(Image { width, height, rgba: out })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_matches_host_encoder() {
        // 用被控端自己的编码器产 PNG，再解回来，保证两侧字节级一致。
        let png = crate::cursor::encode_png(3, 2, &[10,20,30,40, 50,60,70,80, 90,100,110,120, 130,140,150,160, 170,180,190,200, 210,220,230,240]);
        let img = decode(&png).unwrap();
        assert_eq!(img.width, 3);
        assert_eq!(img.height, 2);
        assert_eq!(img.rgba, vec![10,20,30,40, 50,60,70,80, 90,100,110,120, 130,140,150,160, 170,180,190,200, 210,220,230,240]);
    }
}
