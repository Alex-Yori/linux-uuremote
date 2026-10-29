//! /dev/uinput 内核级虚拟键盘。
//!
//! 桌面是 GNOME/Wayland（Xwayland 只是其中一个客户端）：XTEST 的键盘事件只进 X11 客户端，
//! Wayland 原生应用（终端、设置、GNOME 本体）收不到。uinput 造的虚拟键盘是内核 evdev 设备，
//! 合成器/libinput 和所有客户端一视同仁——和 Sunshine/参照实现同一做法。
//! 鼠标不从这里走：XWarpPointer + XTEST 按钮在本机已实测可用，不动它。
//!
//! 权限：udev 规则 `KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", TAG+="uaccess"`
//! 把 /dev/uinput 授权给登录座位的用户（/etc/udev/rules.d/99-uinput.rules）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0;

const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_5568;
/// EV_REP：声明设备有自动重复能力（内核代劳对 uinput 实测无效——重复由宿主
/// keyrepeat 状态机做——但读取方如 libinput 会因此按系统节奏理解长按）。
const EV_REP: u16 = 0x0004;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

struct Device(File);

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.0.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

impl Device {
    fn ioctl(&self, request: libc::c_ulong, value: libc::c_ulong) -> std::io::Result<()> {
        if unsafe { libc::ioctl(self.0.as_raw_fd(), request, value) } < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn create(name: &str, configure: impl FnOnce(&Device) -> std::io::Result<()>) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")?;
        let device = Device(file);
        configure(&device)?;
        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x06, // BUS_VIRTUAL
                vendor: 0x1d6b,
                product: 1,
                version: 1,
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        let len = name.len().min(79);
        setup.name[..len].copy_from_slice(&name.as_bytes()[..len]);
        if unsafe { libc::ioctl(device.0.as_raw_fd(), UI_DEV_SETUP, &setup) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        device.ioctl(UI_DEV_CREATE, 0)?;
        Ok(device)
    }

    fn emit(&mut self, events: &[(u16, u16, i32)]) -> std::io::Result<()> {
        let zero = libc::timeval { tv_sec: 0, tv_usec: 0 };
        let mut buffer = Vec::with_capacity((events.len() + 1) * std::mem::size_of::<InputEvent>());
        for &(kind, code, value) in events.iter().chain(std::iter::once(&(EV_SYN, SYN_REPORT, 0))) {
            let event = InputEvent { time: zero, kind, code, value };
            let bytes = unsafe { std::slice::from_raw_parts((&event as *const InputEvent).cast::<u8>(), std::mem::size_of::<InputEvent>()) };
            buffer.extend_from_slice(bytes);
        }
        self.0.write_all(&buffer)
    }
}

/// 内核虚拟键盘。键盘码 1..=248 全部声明，覆盖客户端可能发来的所有键。
pub struct Keyboard(Device);

impl Keyboard {
    pub fn open() -> std::io::Result<Self> {
        let device = Device::create("linux-uuremote keyboard", |d| {
            d.ioctl(UI_SET_EVBIT, EV_KEY.into())?;
            let _ = d.ioctl(UI_SET_PROPBIT, EV_REP.into()); // 声明重复能力（宿主自己做重复）
            for code in 1..=248u16 {
                d.ioctl(UI_SET_KEYBIT, code.into())?;
            }
            Ok(())
        })?;
        Ok(Self(device))
    }

    pub fn key(&mut self, code: u16, pressed: bool) -> std::io::Result<()> {
        self.0.emit(&[(EV_KEY, code, i32::from(pressed))])
    }

    /// 会话结束兜底：把声明过的键全部松开，哪怕账本漏了某次按下。
    pub fn release_all(&mut self) -> std::io::Result<()> {
        let keys: Vec<(u16, u16, i32)> = (1..=248u16).map(|code| (EV_KEY, code, 0)).collect();
        self.0.emit(&keys)
    }
}

/// X keysym → 物理 evdev 键码（US 布局位置）。合成器再按用户自己的键盘布局解释，
/// 和真实硬件键盘完全一样——所以不走 X 的 keysym→level 查找，AltGr 之类的坑不存在。
pub fn keysym_to_evdev(keysym: u32) -> Option<u16> {
    const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    const KEYPAD: [u16; 10] = [82, 79, 80, 81, 75, 76, 77, 71, 72, 73];
    let code = match keysym {
        0x61..=0x7a => LETTERS[(keysym - 0x61) as usize],
        0x31..=0x39 => (keysym - 0x31) as u16 + 2, // 1..9
        0x30 => 11,                                // 0
        0xffb0..=0xffb9 => KEYPAD[(keysym - 0xffb0) as usize],
        0xffbe..=0xffc7 => (keysym - 0xffbe) as u16 + 59, // F1..F10
        0xffc8 => 87,
        0xffc9 => 88, // F11、F12
        0xffca..=0xffd5 => (keysym - 0xffca) as u16 + 183, // F13..F24
        0xff0b => 76, // Clear = 小键盘 5
        0xff08 => 14,
        0xff09 => 15,
        0xff0d => 28,
        0xff1b => 1,
        0x20 => 57, // Backspace/Tab/Return/Escape/Space
        0xff13 => 119,
        0xffe5 => 58,
        0xff7f => 69,
        0xff14 => 70,
        0xff61 => 99, // Pause/CapsLock/NumLock/ScrollLock/SysRq
        0xffe1 => 42,
        0xffe2 => 54,
        0xffe3 => 29,
        0xffe4 => 97,
        0xffe9 => 56,
        0xffea => 100, // Shift/Ctrl/Alt 左右
        0xffeb => 125,
        0xffec => 126,
        0xff67 => 127, // Super 左右 + Menu
        0xff55 => 104,
        0xff56 => 109,
        0xff57 => 107,
        0xff50 => 102, // PageUp/PageDown/End/Home
        0xff51 => 105,
        0xff52 => 103,
        0xff53 => 106,
        0xff54 => 108, // 方向键
        0xff63 => 110,
        0xffff => 111, // Insert/Delete
        0xffaa => 55,
        0xffab => 78,
        0xffac => 121,
        0xffad => 74,
        0xffae => 83,
        0xffaf => 98,
        0xff8d => 96, // 小键盘运算符
        0x3b => 39,
        0x3d => 13,
        0x2c => 51,
        0x2d => 12,
        0x2e => 52,
        0x2f => 53,
        0x60 => 41, // ;=,. / `
        0x5b => 26,
        0x5c => 43,
        0x5d => 27,
        0x27 => 40, // []\ '
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::windows_vk_to_keysym;

    #[test]
    fn maps_keysyms_to_physical_us_positions() {
        assert_eq!(keysym_to_evdev(0x61), Some(30)); // a
        assert_eq!(keysym_to_evdev(0x7a), Some(44)); // z
        assert_eq!(keysym_to_evdev(0x30), Some(11)); // 0
        assert_eq!(keysym_to_evdev(0x31), Some(2)); // 1
        assert_eq!(keysym_to_evdev(0xffe1), Some(42)); // Shift_L
        assert_eq!(keysym_to_evdev(0xffc9), Some(88)); // F12
        assert_eq!(keysym_to_evdev(0xffb5), Some(76)); // KP_5
        assert_eq!(keysym_to_evdev(0x12345), None);
    }

    #[test]
    fn every_mapped_windows_vk_has_a_physical_key() {
        let missing: Vec<i32> = (0..=0xff)
            .filter(|vk| windows_vk_to_keysym(*vk).is_some_and(|ks| keysym_to_evdev(ks).is_none()))
            .collect();
        assert!(missing.is_empty(), "VK 有 keysym 但没有 evdev 映射：{missing:x?}");
    }
}
