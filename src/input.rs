//! 输入注入：把 `control` 解释出的动作变成 X11 XTest 事件。
//!
//! 拆成两半是有意的：
//! - **事件计划**（纯函数）：动作 → 一串 `(type, detail, x, y)`，可确定性单测。
//! - **X 引擎**（薄）：把计划喂给 `XTest.FakeInput`，外加一次 `QueryPointer` 拿初值。
//!
//! XTest 的坑（写死在这里，别再猜）：
//! - `KeyPress/KeyRelease`：`detail` 是 keysym，但 keysym 超过 255 时 detail 装不下——
//!   协议规定 **detail = 0 时 keycode 从 `root_x` 取**，所以统一走 detail=0 + root_x=keycode。
//! - keysym ≠ keycode：必须先查当前 X 键盘映射（`GetKeyboardMapping`）换出 keycode。
//! - `MotionNotify`：`detail = 1`（XPointerMove）= 绝对根坐标；`detail = 0` = 相对位移。
//! - 滚轮不是轴事件，是按钮 4/5（上下）与 6/7（左右）的按下+松开。

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ConnectionExt as _, Keysym};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::control::{ControlInput, ControlMessage, mouse_button_to_x11, normalized_pointer, windows_vk_to_keysym};

/// XTest 事件类型（X 协议事件码）。
pub const KEYPRESS: u8 = 2;
pub const KEYRELEASE: u8 = 3;
pub const BUTTONPRESS: u8 = 4;
pub const BUTTONRELEASE: u8 = 5;
pub const MOTIONNOTIFY: u8 = 6;
/// MotionNotify 的 detail：1 = 绝对移动到根坐标，0 = 相对位移。
pub const POINTER_MOVE_ABSOLUTE: u8 = 1;
pub const POINTER_MOVE_RELATIVE: u8 = 0;

/// 滚轮方向对应的 X11 按钮序号。
pub const WHEEL_UP: u8 = 4;
pub const WHEEL_DOWN: u8 = 5;
pub const WHEEL_LEFT: u8 = 6;
pub const WHEEL_RIGHT: u8 = 7;

/// 单次滚动最多重复的格数（客户端可以一次报很大的 delta，别把服务器刷爆）。
const SCROLL_REPEAT_CAP: i32 = 20;

/// 一条要发给 XTest 的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XTestEvent {
    pub type_: u8,
    pub detail: u8,
    /// 绝对模式 = 根坐标；相对模式 = 位移；键事件 = keycode。
    pub x: i16,
    pub y: i16,
}

impl XTestEvent {
    fn motion(x: i32, y: i32, absolute: bool) -> Self {
        Self {
            type_: MOTIONNOTIFY,
            detail: if absolute { POINTER_MOVE_ABSOLUTE } else { POINTER_MOVE_RELATIVE },
            x: clamp_i16(x),
            y: clamp_i16(y),
        }
    }
    fn button(type_: u8, button: u8, x: i32, y: i32) -> Self {
        Self { type_, detail: button, x: clamp_i16(x), y: clamp_i16(y) }
    }
    fn key(type_: u8, keycode: u8) -> Self {
        Self { type_, detail: 0, x: keycode as i16, y: 0 }
    }
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// 当前 X 服务器键盘映射：keycode → 一组 keysym。
///
/// 从 `GetKeyboardMapping` 建，可离线构造（单测不需要 X）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    first_keycode: u8,
    keysyms_per_keycode: usize,
    keysyms: Vec<Keysym>,
}

impl Keymap {
    pub fn new(first_keycode: u8, keysyms_per_keycode: u8, keysyms: Vec<Keysym>) -> Self {
        Self { first_keycode, keysyms_per_keycode: keysyms_per_keycode.max(1) as usize, keysyms }
    }

    /// 查当前服务器的映射（min_keycode..=max_keycode 一次问完）。
    pub fn from_connection(conn: &RustConnection) -> Result<Self, String> {
        let setup = conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - first + 1;
        let reply = conn
            .get_keyboard_mapping(first, count)
            .map_err(|e| format!("GetKeyboardMapping 请求失败: {e}"))?
            .reply()
            .map_err(|e| format!("GetKeyboardMapping 回包错误: {e:?}"))?;
        Ok(Self::new(first, reply.keysyms_per_keycode, reply.keysyms))
    }

    /// keysym → keycode。
    ///
    /// 查找顺序 **0, 2, 1, 3**：偶数下标是未加 shift 的基本符号（第一/第二组），优先；
    /// 只在奇数下标（shift 变体）里出现的 keysym 会被当作兜底，注入时要靠客户端另发 Shift。
    /// keysym 0 是「没有键」，必须跳过。
    pub fn keycode_for(&self, keysym: Keysym) -> Option<u8> {
        if keysym == 0 {
            return None;
        }
        for order in [0usize, 2, 1, 3] {
            if order >= self.keysyms_per_keycode {
                continue;
            }
            for (i, &code) in self.keycode_iter().iter().enumerate() {
                let Some(slot) = self.keysyms.get(i * self.keysyms_per_keycode + order) else { continue };
                if *slot == keysym {
                    return Some(code);
                }
            }
        }
        None
    }

    fn keycode_iter(&self) -> Vec<u8> {
        let count = self.keysyms.len() / self.keysyms_per_keycode;
        (0..count).map(|i| self.first_keycode + i as u8).collect()
    }
}

/// Unicode 字符 → X11 keysym（Latin-1 直接用码位，其余走 Unicode keysym 域 0x01000000+cp）。
pub fn keysym_for_char(ch: char) -> Keysym {
    let cp = u32::from(ch);
    if cp <= 0xff {
        cp
    } else {
        0x0100_0000 + cp
    }
}

/// 把一条 CONTROL 消息里的**指针**动作翻成 XTest 事件序列（`None`/空 = 无需注入）。
///
/// 键盘与 `text_input` 返回空序列：它们要先过当前服务器的键盘映射，
/// 只有 `X11Input` 拿得到，别在这里用「ASCII 直映」糊弄过去。
///
/// `pointer` 是当前指针位置（绝对事件会更新它）。认不出的按钮返回空序列——
/// 丢事件比注入错键好。
pub fn plan_pointer_events(
    message: &ControlMessage,
    width: u32,
    height: u32,
    pointer: &mut (i32, i32),
    scroll_remainder: &mut [i32; 2],
) -> Vec<XTestEvent> {
    let (px, py) = *pointer;
    let ControlMessage::Input(input) = message else { return Vec::new() };
    match input {
        crate::control::ControlInput::MouseAbsolute { x, y } => match normalized_pointer(*x, *y, width, height) {
            Some((nx, ny)) => {
                *pointer = (nx, ny);
                vec![XTestEvent::motion(nx, ny, true)]
            }
            None => Vec::new(),
        },
        crate::control::ControlInput::MouseRelative { dx, dy } => {
            *pointer = (px + *dx, py + *dy);
            vec![XTestEvent::motion(*dx, *dy, false)]
        }
        crate::control::ControlInput::MouseButton { button, pressed } => match mouse_button_to_x11(*button) {
            Some(b) => vec![XTestEvent::button(if *pressed { BUTTONPRESS } else { BUTTONRELEASE }, b, px, py)],
            None => Vec::new(),
        },
        crate::control::ControlInput::MouseClick { button } => match mouse_button_to_x11(*button) {
            Some(b) => vec![
                XTestEvent::button(BUTTONPRESS, b, px, py),
                XTestEvent::button(BUTTONRELEASE, b, px, py),
            ],
            None => Vec::new(),
        },
        crate::control::ControlInput::MouseScroll { dx, dy } => {
            // delta 单位是 Windows wheel notch = 120（官方 Mac 客户端同一单位；
            // 触控板的小 delta 靠累加攒满一格才发）。曾经把 delta 当"点击格数"直接
            // 循环，一格 120 被放大成 20 次按钮事件 → 用户「滚一格滑一大屏」。
            let mut out = Vec::new();
            for (axis, delta, positive, negative) in [(0, *dy, WHEEL_UP, WHEEL_DOWN), (1, *dx, WHEEL_RIGHT, WHEEL_LEFT)] {
                let rem = scroll_remainder[axis];
                let total = rem + delta;
                let mut notches = (total / 120).clamp(-SCROLL_REPEAT_CAP, SCROLL_REPEAT_CAP);
                if notches == 0 && delta != 0 && total * rem < 0 && delta.abs() >= rem.abs() {
                    // 反向 delta 攒过零点：立刻发一格反向事件（触控板来回滚不憋方向）。
                    notches = delta.signum();
                }
                // 余量只保留不满一格的部分：限流砍掉的量直接丢弃，否则一次快速滚动
                // 会把几千点欠账攒着，之后每帧都补发一格，一直滑个不停。
                scroll_remainder[axis] = (total % 120).clamp(-119, 119);
                let button = if notches > 0 { positive } else { negative };
                for _ in 0..notches.unsigned_abs() {
                    out.push(XTestEvent::button(BUTTONPRESS, button, px, py));
                    out.push(XTestEvent::button(BUTTONRELEASE, button, px, py));
                }
            }
            out
        }
        crate::control::ControlInput::Keyboard { .. }
        | crate::control::ControlInput::KeyboardClick { .. }
        | crate::control::ControlInput::TextInput { .. } => Vec::new(),
    }
}

/// X11 XTest 注入引擎。
pub struct X11Input {
    conn: RustConnection,
    root: u32,
    width: u32,
    height: u32,
    keymap: Keymap,
    pointer: (i32, i32),
    /// 不满一格的滚动余量（单位 120/notch）。触控板小 delta 攒满 120 才发一次按钮，
    /// 大 delta 按整格换算——方向翻转时余量自然抵消。
    /// 两轴各自的滚动余量 [竖直, 水平]（单位 120/notch）。
    scroll_remainder: [i32; 2],
    /// 已注入但客户端还没松开的鼠标按键（X11 按钮序号），会话结束时统一松开。
    held_buttons: Vec<u8>,
    held_keys: Vec<u8>,
    /// 内核虚拟键盘（/dev/uinput）。GNOME/Wayland 下 XTEST 键盘只进 X 客户端，
    /// 原生 Wayland 应用收不到——键盘优先走 uinput，打不开才退回 XTEST。
    keyboard: Option<crate::uinput::Keyboard>,
}

impl X11Input {
    pub fn connect(conn: RustConnection, screen_index: usize) -> Result<Self, String> {
        let screen = conn.setup().roots.get(screen_index).ok_or(format!("没有屏幕 #{screen_index}"))?.clone();
        let keymap = Keymap::from_connection(&conn)?;
        let pointer = conn
            .query_pointer(screen.root)
            .map_err(|e| format!("QueryPointer 请求失败: {e}"))?
            .reply()
            .map(|r| (i32::from(r.root_x), i32::from(r.root_y)))
            .unwrap_or((0, 0));
        Ok(Self {
            conn,
            root: screen.root,
            width: u32::from(screen.width_in_pixels),
            height: u32::from(screen.height_in_pixels),
            keymap,
            pointer,
            scroll_remainder: [0, 0],
            held_buttons: Vec::new(),
            held_keys: Vec::new(),
            keyboard: match crate::uinput::Keyboard::open() {
                Ok(k) => {
                    println!("   键盘后端：uinput 虚拟键盘（Wayland 原生应用可达）");
                    Some(k)
                }
                Err(e) => {
                    println!("   键盘后端：XTEST（uinput 打不开：{e}——Wayland 原生应用收不到键盘）");
                    None
                }
            },
        })
    }

    pub fn screen(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 处理一条 CONTROL 消息；返回是否注入了东西。
    pub fn apply(&mut self, message: &ControlMessage) -> Result<bool, String> {
        let events = match message {
            ControlMessage::Input(ControlInput::Keyboard { key, pressed }) => {
                if self.inject_uinput_key(*key, *pressed)? {
                    return Ok(true);
                }
                self.key_events(*key, *pressed)
            }
            ControlMessage::Input(ControlInput::KeyboardClick { key }) => {
                if self.inject_uinput_key(*key, true)? {
                    self.inject_uinput_key(*key, false)?;
                    return Ok(true);
                }
                self.click_events(*key)
            }
            ControlMessage::Input(ControlInput::TextInput { text }) => self.text_events(text),
            ControlMessage::Input(ControlInput::MouseButton { button, pressed }) => {
                if let Some(b) = mouse_button_to_x11(*button) {
                    if *pressed {
                        if !self.held_buttons.contains(&b) {
                            self.held_buttons.push(b);
                        }
                    } else {
                        self.held_buttons.retain(|&c| c != b);
                    }
                }
                plan_pointer_events(message, self.width, self.height, &mut self.pointer, &mut self.scroll_remainder)
            }
            other => plan_pointer_events(other, self.width, self.height, &mut self.pointer, &mut self.scroll_remainder),
        };
        if events.is_empty() {
            return Ok(false);
        }
        for e in &events {
            if e.type_ == MOTIONNOTIFY {
                // Xwayland 把 XTEST 的 MotionNotify 连 detail=1 当**相对位移**处理
                // （实测：abs(1000,300) 从 (200,900) 落点 (1200,1200)，再加码到
                // 屏角就永远钉死右下角）。WarpPointer 才精确，且会给窗口发 MotionNotify。
                let (px, py) = self.pointer;
                warp_pointer(&self.conn, self.root, px, py)?;
            } else {
                self.conn
                    .xtest_fake_input(e.type_, e.detail, 0, self.root, e.x, e.y, 0)
                    .map_err(|err| format!("XTest FakeInput 失败: {err}"))?;
            }
        }
        self.conn.flush().map_err(|e| format!("X flush 失败: {e}"))?;
        Ok(true)
    }

    /// 会话结束：把还按着的键和按钮松开，别让本机卡在按住状态。
    pub fn release_all(&mut self) {
        if let Some(keyboard) = self.keyboard.as_mut() {
            let _ = keyboard.release_all();
        }
        for button in std::mem::take(&mut self.held_buttons) {
            let (px, py) = self.pointer;
            let _ = self.conn.xtest_fake_input(BUTTONRELEASE, button, 0, self.root, clamp_i16(px), clamp_i16(py), 0);
        }
        for keycode in std::mem::take(&mut self.held_keys) {
            let _ = self.conn.xtest_fake_input(KEYRELEASE, 0, 0, self.root, keycode as i16, 0, 0);
        }
        let _ = self.conn.flush();
    }

    /// uinput 键盘注入；返回 Ok(true)=已注入，Ok(false)=没有虚拟键盘（走 XTEST 老路）。
    fn inject_uinput_key(&mut self, vk: i32, pressed: bool) -> Result<bool, String> {
        let Some(keyboard) = self.keyboard.as_mut() else { return Ok(false) };
        let Some(keysym) = windows_vk_to_keysym(vk) else { return Ok(false) };
        let Some(code) = crate::uinput::keysym_to_evdev(keysym) else { return Ok(false) };
        keyboard.key(code, pressed).map_err(|e| format!("uinput 键盘注入失败：{e}"))?;
        Ok(true)
    }

    fn key_events(&mut self, vk: i32, pressed: bool) -> Vec<XTestEvent> {
        let Some(code) = self.keycode_for_vk(vk) else { return Vec::new() };
        if pressed {
            if !self.held_keys.contains(&code) {
                self.held_keys.push(code);
            }
        } else {
            self.held_keys.retain(|&c| c != code);
        }
        vec![XTestEvent::key(if pressed { KEYPRESS } else { KEYRELEASE }, code)]
    }

    fn click_events(&mut self, vk: i32) -> Vec<XTestEvent> {
        let Some(code) = self.keycode_for_vk(vk) else { return Vec::new() };
        vec![XTestEvent::key(KEYPRESS, code), XTestEvent::key(KEYRELEASE, code)]
    }

    fn text_events(&mut self, text: &str) -> Vec<XTestEvent> {
        let mut out = Vec::new();
        for ch in text.chars() {
            if let Some(code) = self.keymap.keycode_for(keysym_for_char(ch)) {
                out.push(XTestEvent::key(KEYPRESS, code));
                out.push(XTestEvent::key(KEYRELEASE, code));
            }
        }
        out
    }

    fn keycode_for_vk(&self, vk: i32) -> Option<u8> {
        let keysym = windows_vk_to_keysym(vk)?;
        self.keymap.keycode_for(keysym)
    }
}

/// 用 X 协议 warp 指针到根坐标（XTest 之外的兜底：有些合成器对 FakeInput 的
/// MotionNotify 反应不对，WarpPointer 一定移动指针）。
pub fn warp_pointer(conn: &RustConnection, root: u32, x: i32, y: i32) -> Result<(), String> {
    conn.warp_pointer(0u32, root, 0, 0, 0, 0, clamp_i16(x), clamp_i16(y))
        .map_err(|e| format!("WarpPointer 请求失败: {e}"))?
        .ignore_error();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::decode_control;

    fn msg(json: &str) -> ControlMessage {
        decode_control(json.as_bytes()).unwrap()
    }

    #[test]
    fn absolute_move_updates_pointer_and_is_absolute() {
        let mut p = (10, 10);
        let mut rem = [0i32; 2];
        let ev = plan_pointer_events(&msg(r#"{"action":"mouse_move_absolute","abs_x":1.0,"abs_y":1.0}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(ev, vec![XTestEvent { type_: MOTIONNOTIFY, detail: POINTER_MOVE_ABSOLUTE, x: 1919, y: 1079 }]);
        assert_eq!(p, (1919, 1079));
    }

    #[test]
    fn relative_move_is_relative_delta() {
        let mut p = (100, 200);
        let mut rem = [0i32; 2];
        let ev = plan_pointer_events(&msg(r#"{"action":"mouse_move_relative","delta_x":-30,"delta_y":7}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(ev, vec![XTestEvent { type_: MOTIONNOTIFY, detail: POINTER_MOVE_RELATIVE, x: -30, y: 7 }]);
        assert_eq!(p, (70, 207));
    }

    #[test]
    fn pixel_space_absolute_maps_to_screen() {
        // Windows 客户端发像素：1.4/0.5 混合空间按像素解释并夹到屏内。
        let mut p = (42, 42);
        let mut rem = [0i32; 2];
        let events = plan_pointer_events(&msg(r#"{"action":"mouse_move_absolute","abs_x":600.0,"abs_y":900.0}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(events.len(), 1);
        assert_eq!(p, (600, 900));
        assert!(plan_pointer_events(&msg(r#"{"action":"mouse_move_absolute","abs_x":-1.0,"abs_y":0.5}"#), 1920, 1080, &mut p, &mut rem).is_empty());
    }

    #[test]
    fn button_bitmask_press_release_use_current_position() {
        let mut p = (300, 400);
        let mut rem = [0i32; 2];
        let press = plan_pointer_events(&msg(r#"{"action":"mouse_press","button":2}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(press, vec![XTestEvent { type_: BUTTONPRESS, detail: 3, x: 300, y: 400 }]);
        let release = plan_pointer_events(&msg(r#"{"action":"mouse_release","button":2}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(release, vec![XTestEvent { type_: BUTTONRELEASE, detail: 3, x: 300, y: 400 }]);
        assert!(plan_pointer_events(&msg(r#"{"action":"mouse_press","button":9}"#), 1920, 1080, &mut p, &mut rem).is_empty());
    }

    #[test]
    fn click_is_press_then_release() {
        let mut p = (0, 0);
        let mut rem = [0i32; 2];
        let ev = plan_pointer_events(&msg(r#"{"action":"mouse_click","button":1}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(ev, vec![
            XTestEvent { type_: BUTTONPRESS, detail: 1, x: 0, y: 0 },
            XTestEvent { type_: BUTTONRELEASE, detail: 1, x: 0, y: 0 },
        ]);
    }

    #[test]
    fn wheel_delta_is_windows_notches_of_120_with_accumulating_remainder() {
        // 官方客户端一格里 = 120；直接当"格数"会把一格放大成几十次按钮（滚一格滑一大屏）。
        let mut p = (5, 5);
        let mut rem = [0i32; 2];
        let one = plan_pointer_events(&msg(r#"{"action":"mouse_scroll","delta_x":0,"delta_y":120}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(one.len(), 2, "一格 = 一次 press+release");
        assert_eq!(one[0], XTestEvent { type_: BUTTONPRESS, detail: WHEEL_UP, x: 5, y: 5 });
        assert_eq!(rem, [0, 0]);
        // 触控板小 delta：攒不满一格不发，攒满即发。
        assert!(plan_pointer_events(&msg(r#"{"action":"mouse_scroll","delta_x":0,"delta_y":2}"#), 1920, 1080, &mut p, &mut rem).is_empty());
        assert_eq!(rem, [2, 0]);
        let filled = plan_pointer_events(&msg(r#"{"action":"mouse_scroll","delta_x":0,"delta_y":118}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(filled.len(), 2, "2 + 118 = 120 → 恰好一格");
        assert_eq!(rem, [0, 0]);
        // 方向翻转：余量抵消，不会两个方向都抖。
        assert!(plan_pointer_events(&msg(r#"{"action":"mouse_scroll","delta_x":0,"delta_y":40}"#), 1920, 1080, &mut p, &mut rem).is_empty());
        let back = plan_pointer_events(&msg(r#"{"action":"mouse_scroll","delta_x":0,"delta_y":-70}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(back[0].detail, WHEEL_DOWN);
        assert_eq!(rem, [-30, 0]);
        let capped = plan_pointer_events(&msg(r#"{"action":"mouse_scroll","delta_x":0,"delta_y":8400}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(capped.len(), 2 * SCROLL_REPEAT_CAP as usize, "快速滚动有上限");
    }

    #[test]
    fn heartbeat_and_unknown_inject_nothing() {
        let mut p = (1, 1);
        let mut rem = [0i32; 2];
        assert!(plan_pointer_events(&msg(r#"{"heartbeat":"kbd_heartbeat"}"#), 1920, 1080, &mut p, &mut rem).is_empty());
        assert!(plan_pointer_events(&msg(r#"{"action":"whatever"}"#), 1920, 1080, &mut p, &mut rem).is_empty());
    }

    #[test]
    fn coordinates_are_clamped_into_i16_not_wrapped() {
        let mut p = (0, 0);
        let mut rem = [0i32; 2];
        let ev = plan_pointer_events(&msg(r#"{"action":"mouse_move_relative","delta_x":100000,"delta_y":-70000}"#), 1920, 1080, &mut p, &mut rem);
        assert_eq!(ev[0].x, i16::MAX);
        assert_eq!(ev[0].y, i16::MIN);
    }

    #[test]
    fn keymap_prefers_unshifted_slot_and_skips_holes() {
        // keycode 10: [a, A, 0, 0]；keycode 11: [0, 0, b, B]；keycode 12 全空。
        let km = Keymap::new(10, 4, vec![0x61, 0x41, 0, 0, 0, 0, 0x62, 0x42, 0, 0, 0, 0]);
        assert_eq!(km.keycode_for(0x61), Some(10));
        assert_eq!(km.keycode_for(0x62), Some(11), "第二组也要能找到");
        assert_eq!(km.keycode_for(0x41), Some(10), "shift 变体作兜底");
        assert_eq!(km.keycode_for(0), None, "keysym 0 是空洞");
        assert_eq!(km.keycode_for(0xffbe), None, "没映射的键不编造 keycode");
    }

    #[test]
    fn keyboard_actions_delegate_to_the_keymap_path() {
        let mut p = (1, 1);
        let mut rem = [0i32; 2];
        assert!(plan_pointer_events(&msg(r#"{"action":"kbd_press","key":65}"#), 1920, 1080, &mut p, &mut rem).is_empty());
        assert!(plan_pointer_events(&msg(r#"{"action":"text_input","content":"ab"}"#), 1920, 1080, &mut p, &mut rem).is_empty());
    }

    #[test]
    fn unicode_keysym_domain_for_non_latin() {
        assert_eq!(keysym_for_char('a'), 0x61);
        assert_eq!(keysym_for_char('\u{a0}'), 0xa0);
        assert_eq!(keysym_for_char('é'), 0xe9);
        assert_eq!(keysym_for_char('你'), 0x0100_4f60);
        let km = Keymap::new(60, 2, vec![0x0100_4f60, 0, 0, 0]);
        assert_eq!(km.keycode_for(keysym_for_char('你')), Some(60));
    }
}
