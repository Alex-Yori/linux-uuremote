//! SDL scancode 就是 USB HID usage；按物理键发送，不猜打字文本（照参照实现的表）。
use serde_json::{json, Value};

pub fn windows_vk(scan: i32) -> Option<i32> {
    Some(match scan {
        4..=29 => 0x41 + scan - 4, 30..=38 => 0x31 + scan - 30, 39 => 0x30,
        40 => 0x0d, 41 => 0x1b, 42 => 8, 43 => 9, 44 => 0x20,
        45 => 0xbd, 46 => 0xbb, 47 => 0xdb, 48 => 0xdd, 49 => 0xdc, 51 => 0xba, 52 => 0xde, 53 => 0xc0, 54 => 0xbc, 55 => 0xbe, 56 => 0xbf,
        57 => 0x14, 58..=69 => 0x70 + scan - 58, 70 => 0x2c, 71 => 0x91, 72 => 0x13,
        73 => 0x2d, 74 => 0x24, 75 => 0x21, 76 => 0x2e, 77 => 0x23, 78 => 0x22,
        79 => 0x27, 80 => 0x25, 81 => 0x28, 82 => 0x26, 83 => 0x90,
        84 => 0x6f, 85 => 0x6a, 86 => 0x6d, 87 => 0x6b, 88 => 0x0d,
        89..=97 => 0x61 + scan - 89, 98 => 0x60, 99 => 0x6e, 101 => 0x5d,
        104..=115 => 0x7c + scan - 104,
        224 => 0xa2, 225 => 0xa0, 226 => 0xa4, 227 => 0x5b, 228 => 0xa3, 229 => 0xa1, 230 => 0xa5, 231 => 0x5c,
        _ => return None,
    })
}

/// 画面在窗口里的等比适配区；坐标换算只认这个矩形。
#[derive(Clone, Copy, Debug, Default)]
pub struct Viewport { pub x: f64, pub y: f64, pub width: f64, pub height: f64 }
impl Viewport {
    pub fn fit(window: (u32, u32), frame: (u32, u32)) -> Self {
        let scale = (window.0 as f64 / frame.0.max(1) as f64).min(window.1 as f64 / frame.1.max(1) as f64);
        let (w, h) = (frame.0 as f64 * scale, frame.1 as f64 * scale);
        Self { x: (window.0 as f64 - w) / 2., y: (window.1 as f64 - h) / 2., width: w, height: h }
    }
    /// 被控端接受的归一化坐标：abs_x/abs_y ∈ [0,1]。
    pub fn motion(&self, x: f64, y: f64) -> Option<Value> {
        if self.width <= 0. || self.height <= 0. || !x.is_finite() || !y.is_finite() { return None }
        Some(json!({"action": "mouse_move_absolute", "abs_x": ((x - self.x) / self.width).clamp(0., 1.), "abs_y": ((y - self.y) / self.height).clamp(0., 1.)}))
    }
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vk_table_spot_checks() {
        assert_eq!(windows_vk(4), Some(0x41)); // A
        assert_eq!(windows_vk(30), Some(0x31)); // 1
        assert_eq!(windows_vk(44), Some(0x20)); // Space
        assert_eq!(windows_vk(224), Some(0xa2)); // Left Ctrl
        assert_eq!(windows_vk(255), None);
    }
    #[test]
    fn viewport_clamps_motion() {
        let v = Viewport::fit((1000, 800), (2560, 1440));
        let m = v.motion(v.x + v.width * 0.5, v.y + v.height * 0.25).unwrap();
        assert!((m["abs_x"].as_f64().unwrap() - 0.5).abs() < 1e-6);
        assert!((m["abs_y"].as_f64().unwrap() - 0.25).abs() < 1e-6);
        assert!(v.motion(-100., 0.).unwrap()["abs_x"].as_f64().unwrap() == 0.);
    }
}
