//! uu-viewer —— UU 远程 Linux 控制端。
//!
//! 界面照原版 Windows 客户端做两层：
//! 1. **主页窗口**（仿"全部设备/我的设备"页）：左侧栏 网易UU远程 logo + 我的设备
//!    （在线绿点/离线灰点）+ 全部设备 + 远程协助 + 设置；主区 在线状态标题、蓝色大卡
//!    "进入桌面 →"、功能按钮行（文件传输/观看模式/终端/端口映射）、快速启动框。
//!    未登录时主区显示"立即登录"，点开是扫码登录面板（二维码 + 状态轮询）。
//! 2. **远程画面窗口**：顶部设置条（画质/帧率/分辨率/真彩/声音/全屏/断开，F1 呼出）+
//!    右下角实时状态（fps/码率/frm/ping）；键鼠透传、被控端光标精灵叠加。
//!
//! 用法：
//!   uu-viewer                      打开主页，选中设备点"进入桌面"
//!   uu-viewer --device <id|别名>   跳过主页直接连
//!   uu-viewer --list               终端列设备
//!   --quality 1..4|auto  --fps 30|60|90|144  --window WxH  --no-audio  --view-only

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linux_uuremote::api::{self, api_data, ApiClient, Credentials, QrPollAction};
use linux_uuremote::config;
use linux_uuremote::viewer::rtc::{CursorImage, Frame, Host, Stats};
use linux_uuremote::viewer::wire;
use linux_uuremote::viewer::icons;
use linux_uuremote::viewer::{join, Cmd, Session};

use sdl2::event::{Event, WindowEvent};
use sdl2::mouse::MouseButton;
use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::{Point, Rect};
use sdl2::render::{Texture, TextureCreator, TextureAccess};
use sdl2::video::{FullscreenType, Window, WindowContext, WindowPos};
use sdl2::Sdl;
use serde_json::{json, Value};

// ---- 配色：桌面 ~/桌面/uu.html「UI 还原」稿的 CSS 变量照搬（图标资产与显示尺寸不动）----
const BRAND_BLUE: Color = Color::RGB(22, 93, 255);   // --primary-blue #165DFF
const UI_BLUE: Color = BRAND_BLUE;
const GREEN: Color = Color::RGB(0, 180, 42);         // --green-online #00B42A
const SIDE_BG: Color = Color::RGB(245, 246, 248);    // --bg-sidebar #F5F6F8
const PANEL_BG: Color = SIDE_BG;
const MAIN_BG: Color = Color::RGB(255, 255, 255);    // --bg-main #FFFFFF
const CARD_BG: Color = MAIN_BG;
const CARD_BORDER: Color = Color::RGB(229, 230, 235); // --border-color #E5E6EB
const SEP: Color = CARD_BORDER;
const SELECTED: Color = Color::RGB(232, 235, 240);   // --hover-bg #E8EBF0（选中行底）
const ROW_HOVER: Color = SELECTED;
const HOVER_ROW: Color = Color::RGB(242, 243, 245);  // .menu-item:hover rgba(0,0,0,.03)
const INK: Color = Color::RGB(96, 101, 104);         // 线条图标笔画色（图标不动）
const TEXT_MAIN: Color = Color::RGB(29, 33, 41);     // --text-main #1D2129
const TEXT_DARK: Color = Color::RGB(78, 89, 105);    // --text-secondary #4E5969
const TEXT_MUT: Color = Color::RGB(134, 144, 156);   // --text-tertiary #86909C
const PILL_DARK: Color = Color::RGB(29, 33, 41);     // .status-tag 深底白字
const PREVIEW_BG: Color = Color::RGB(229, 230, 235); // .preview-container 底
const DASH: Color = Color::RGB(201, 205, 212);       // .dashed-box 虚线
const ADD_BG: Color = Color::RGB(242, 243, 245);     // .add-btn 圆底
const WHITE: Color = Color::RGB(255, 255, 255);
const TOPBAR_H: i32 = 40;
const SIDE_W: i32 = 232;                             // .sidebar 宽（HTML）
const PAD: i32 = 32;                                 // .content-body 内边距（HTML）


fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let flag = |name: &str| args.iter().any(|a| a == name);

    // UI 预览：不连云端，用合成帧驱动远程窗口界面，验证工具栏/图标/HUD/Toast 渲染。
    if flag("--ui-preview") {
        run_ui_preview();
        return;
    }

    // 主页预览：合成设备列表渲染主页一帧，把 RGBA 落到文件（--home-snap 给路径），供外部转 PNG 检查。
    if flag("--home-preview") {
        run_home_preview(opt("--home-snap").as_deref());
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio");

    if flag("--list") {
        let file = config::read_with_fallback().ok().and_then(|(f, _)| f);
        let creds = match Credentials::load(file.as_ref()) {
            Ok(c) => c,
            Err(e) => { eprintln!("{e}"); std::process::exit(1); }
        };
        let api = ApiClient::new(creds);
        match rt.block_on(api.device_list()) {
            Ok(devs) => {
                for d in &devs {
                    let id = d.get("device_id").and_then(Value::as_str).unwrap_or("?");
                    let alias = d.get("alias").and_then(Value::as_str).unwrap_or("(无名)");
                    let status = d.get("status").and_then(Value::as_str).unwrap_or("?");
                    println!("{alias}  id={id}  状态={status}");
                }
            }
            Err(e) => { eprintln!("{e}"); std::process::exit(1); }
        }
        return;
    }

    let quality: u64 = match opt("--quality").as_deref() {
        Some("auto") => 5, Some("1") => 1, Some("2") => 2, Some("3") => 3, Some("4") => 4, _ => 4,
    };
    let fps: u32 = match opt("--fps").as_deref() {
        Some("30") => 30, Some("90") => 90, Some("144") => 144, _ => 60,
    };
    let (win_w, win_h) = match opt("--window").as_deref().and_then(parse_size) { Some(v) => v, None => (1440, 900) };
    let view_only = flag("--view-only") || std::env::var("UU_VIEWER_VIEW_ONLY").is_ok_and(|v| v == "1");
    let no_audio = flag("--no-audio");

    force_argb();
    let sdl = sdl2::init().expect("SDL 初始化失败");
    let video = sdl.video().expect("视频子系统失败");
    let fonts = load_fonts();
    let font = fonts.r16.as_ref();
    if font.is_none() {
        println!("警告：没找到可用字体，界面文字将不可见（装 fonts-noto-cjk 试试）");
    }

    // CLI 直连：保持旧行为，不开主页。
    if let Some(w) = opt("--device") {
        let creds = match rt.block_on(load_base_creds()) {
            Ok(c) => c,
            Err(e) => { eprintln!("{e}"); std::process::exit(1); }
        };
        let devs = rt.block_on(ApiClient::new(clone_creds(&creds)).device_list()).unwrap_or_default();
        let target = resolve_device(&devs, &creds, &w).unwrap_or(w);
        println!("连接目标：{target}");
        let mut pump = sdl.event_pump().expect("事件泵");
        match run_session(&sdl, &video, font, &creds, &target, "", quality, fps, win_w, win_h, view_only, no_audio, &mut pump) {
            Ok(()) => println!("已断开。"),
            Err(e) => { eprintln!("{e}"); std::process::exit(1); }
        }
        return;
    }

    // ---- 主页 ----
    let mut canvas = video.window("网易UU远程", WIN_W, WIN_H).position_centered().borderless().build().expect("窗口创建失败").into_canvas().build().expect("渲染器创建失败");
    canvas.window_mut().set_bordered(false);
    set_app_icon(canvas.window_mut());
    let creator = canvas.texture_creator();

    let logged = rt.block_on(load_base_creds()).is_ok_and(|c| !c.token.is_empty());

    // 设备列表后台轮询（登录成功后主页自动出设备）。
    let (dev_tx, dev_rx) = std::sync::mpsc::channel::<Vec<Value>>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            loop {
                let creds = config::read().ok().flatten().and_then(|f| Credentials::load(Some(&f)).ok());
                if let Some(c) = creds {
                    if !c.token.is_empty() {
                        let api = ApiClient::new(c);
                        if let Ok(devs) = api.device_list().await { let _ = dev_tx.send(devs); }
                    }
                }
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        });
    });

    let (login_tx, login_rx) = std::sync::mpsc::channel::<LoginUpdate>();
    let mut login_thread: Option<std::thread::JoinHandle<()>> = None;

    let mut home = Home::new(logged);
    let mut text_cache: HashMap<String, Texture> = HashMap::new();
    let mut qr_tex: Option<Texture> = None;

    let shadow_tex = shadow_texture(&creator);
    let corner_tl = corner_texture(&creator, SIDE_BG, true, true);
    let corner_tr = corner_texture(&creator, MAIN_BG, false, true);
    let corner_bl = corner_texture(&creator, SIDE_BG, true, false);
    let corner_br = corner_texture(&creator, MAIN_BG, false, false);
    let corners = [&corner_tl, &corner_tr, &corner_bl, &corner_br];

    let mut event_pump = sdl.event_pump().expect("事件泵");
    let xconn = x_connect();
    let mut last_move: Option<(i32, i32)> = None;
    let mut drag_cookie: Option<x11rb::cookie::Cookie<'_, x11rb::rust_connection::RustConnection, x11rb::protocol::xproto::QueryPointerReply>> = None;
    'home: loop {
        // 拖动帧：每轮用 X 服务器指针真值驱动搬窗（SDL 事件出界即断，X 查询不受限），松键自动收尾。
        // 拖动期间不重绘（内容不变，mutter 直接搬已渲染缓冲），循环全速跑 → 跟手不掉帧。
        if home.drag.is_some() {
            // 流水线：收上一帧预发的指针回复 → 搬窗 → 立刻预发下一帧。X 往返不阻塞循环，拖动期间不重绘。
            if let Some(r) = drag_cookie.take().and_then(|c| c.reply().ok()) {
                use x11rb::protocol::xproto::KeyButMask;
                let down = u16::from(KeyButMask::BUTTON1) & u16::from(r.mask) != 0;
                if !down { home.drag = None; last_move = None; }
                else {
                    let (gx0, gy0, px0, py0) = home.drag.unwrap();
                    let (nx, ny) = (px0 + r.root_x as i32 - gx0, py0 + r.root_y as i32 - gy0);
                    if last_move != Some((nx, ny)) {
                        last_move = Some((nx, ny));
                        if std::env::var_os("UU_DRAG_DEBUG").is_some() { eprintln!("drag: root=({},{}) -> win=({nx},{ny})", r.root_x, r.root_y); }
                        canvas.window_mut().set_position(WindowPos::Positioned(nx), WindowPos::Positioned(ny));
                    }
                }
            }
            if home.drag.is_some() { drag_cookie = x_query_send(&xconn); }
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        last_move = None;
        while let Some(event) = event_pump.poll_event() {
            match event {
                Event::Quit { .. } => break 'home,
                Event::Window { win_event: WindowEvent::Close, .. } => break 'home,
                Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, .. } => {
                    match home.hit(x - MARGIN, y - MARGIN) {
                        HomeAction::None => {
                            // 顶栏空白区按下 = 拖动整窗（borderless 后没有系统标题栏，这里就是标题栏）。
                            // 抓鼠 + 全局指针真值：拖动量 = 服务器指针当前位置 - 按下时的指针位置，
                            // 与窗口相对坐标/搬窗延迟完全解耦，出界、积压、mutter 排队都不会跑偏。
                            if y - MARGIN < TOPBAR_H && y >= 0 && x >= MARGIN {
                                // 按下瞬间的指针真值取 SDL 事件自带的相对坐标 + 当时窗口位置（零竞态）。
                                let (px, py) = canvas.window().position();
                                if std::env::var_os("UU_DRAG_DEBUG").is_some() { eprintln!("press: root=({},{}) win=({px},{py})", px + x, py + y); }
                                home.drag = Some((px + x, py + y, px, py));
                            }
                        }
                        HomeAction::Select(i) => home.selected = Some(i),
                        HomeAction::Login => {
                            home.login_open = true;
                            home.login_status = "正在生成二维码……".into();
                            qr_tex = None;
                            if login_thread.as_ref().map_or(true, |h| h.is_finished()) {
                                let tx = login_tx.clone();
                                login_thread = Some(std::thread::spawn(move || {
                                    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
                                    rt.block_on(async move { qr_login_flow(tx).await });
                                }));
                            }
                        }
                        HomeAction::Enter(view_only_click) => {
                            let sel = home.selected_device().map(|d| (d.id.clone(), d.alias.clone()));
                            if let Some((id, alias)) = sel {
                                let _ = &mut event_pump;
                                match config::read().ok().flatten().and_then(|f| Credentials::load(Some(&f)).ok()) {
                                    Some(creds) => {
                                        home.session_busy = true;
                                        canvas.window_mut().hide();
                                        let res = run_session(&sdl, &video, font, &creds, &id, &alias, quality, fps, win_w, win_h, view_only_click || view_only, no_audio, &mut event_pump);
                                        canvas.window_mut().show();
                                        home.session_busy = false;
                                        home.status_msg = match res { Ok(()) => "已断开。".to_owned(), Err(e) => format!("会话结束：{e}") };
                                    }
                                    None => home.status_msg = "还没有登录态，先点「立即登录」。".into(),
                                }
                            }
                        }
                        HomeAction::ToggleGroup(g) => home.groups_open[g as usize] ^= true,
                        HomeAction::Minimize => canvas.window_mut().minimize(),
                        HomeAction::Close => break 'home,
                        HomeAction::Stub(label) => home.status_msg = format!("{label}（暂未实现）"),
                    }
                }
                Event::MouseMotion { x, y, .. } => {
                    home.mouse = (x - MARGIN, y - MARGIN);
                }
                Event::MouseButtonUp { .. } => home.drag = None,
                _ => {}
            }
        }

        while let Ok(devs) = dev_rx.try_recv() {
            home.devices = devs.iter().filter_map(DeviceRow::from_json).collect();
            if home.selected.is_none() && !home.devices.is_empty() { home.selected = Some(0); }
            home.logged_in = true;
        }
        while let Ok(u) = login_rx.try_recv() {
            match u {
                LoginUpdate::Qr(url) => {
                    match qrcode::QrCode::new(url.as_bytes()) {
                        Ok(code) => {
                            let w = code.width();
                            let colors = code.to_colors();
                            let scale = 4usize;
                            let px = (w + 8) * scale;
                            if let Ok(mut tex) = creator.create_texture(PixelFormatEnum::RGBA32, sdl2::render::TextureAccess::Static, px as u32, px as u32) {
                                let ok = tex.with_lock(None, |buf, pitch| {
                                    for row in 0..px {
                                        let line = &mut buf[row * pitch..row * pitch + pitch];
                                        for col in 0..px {
                                            let q = row as i64 - 4 * scale as i64;
                                            let c = col as i64 - 4 * scale as i64;
                                            let dark = q >= 0 && c >= 0 && (q as usize) < w && (c as usize) < w && colors[q as usize * w + c as usize] == qrcode::types::Color::Dark;
                                            let v: [u8; 4] = if dark { [20, 20, 20, 255] } else { [255, 255, 255, 255] };
                                            line[col * 4..col * 4 + 4].copy_from_slice(&v);
                                        }
                                    }
                                    Ok::<(), String>(())
                                });
                                if ok.is_ok() { qr_tex = Some(tex); }
                            }
                            home.login_status = "请使用网易UU远程或微信 扫码登录".into();
                        }
                        Err(e) => home.login_status = format!("二维码生成失败：{e}"),
                    }
                }
                LoginUpdate::Status(s) => home.login_status = s.into(),
                LoginUpdate::LoggedIn => { home.login_open = false; home.logged_in = true; home.status_msg = "登录成功。".into(); qr_tex = None; }
                LoginUpdate::Failed(e) => { home.login_status = e; qr_tex = None; }
            }
        }
        if login_thread.as_ref().is_some_and(|h| h.is_finished()) { login_thread = None; }

        let (hm_x, hm_y) = home.mouse;
        draw_home(&mut canvas, &creator, &fonts, &mut home, &mut text_cache, qr_tex.as_ref(), &shadow_tex, corners, hm_x, hm_y);
        canvas.present();
        std::thread::sleep(Duration::from_millis(30));
    }
}

// ---------------- 主页 ----------------

struct DeviceRow { id: String, alias: String, online: bool, controllable: bool }
impl DeviceRow {
    fn from_json(d: &Value) -> Option<Self> {
        Some(Self {
            id: d.get("device_id").and_then(Value::as_str)?.to_owned(),
            alias: d.get("alias").and_then(Value::as_str).unwrap_or("(无名)").to_owned(),
            online: d.get("status").and_then(Value::as_str) == Some("CONNECTED"),
            controllable: d.get("controllable").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

#[derive(Clone, PartialEq)]
enum HomeAction { None, Select(usize), Login, Enter(bool), Minimize, Close, Stub(String), ToggleGroup(u8) }

struct Home {
    devices: Vec<DeviceRow>,
    selected: Option<usize>,
    logged_in: bool,
    login_open: bool,
    login_status: String,
    status_msg: String,
    session_busy: bool,
    mouse: (i32, i32),
    // 拖动锚点：按下瞬间的全局鼠标位置 + 窗口位置（增量直接由服务器指针真值驱动）。
    drag: Option<(i32, i32, i32, i32)>,
    groups_open: [bool; 2],
    hit_map: Vec<(Rect, HomeAction)>,
}
impl Home {
    fn new(logged_in: bool) -> Self {
        Self { devices: Vec::new(), selected: None, logged_in, login_open: false, login_status: String::new(), status_msg: String::new(), session_busy: false, mouse: (-1, -1), drag: None, groups_open: [true, true], hit_map: Vec::new() }
    }
    fn selected_device(&self) -> Option<&DeviceRow> { self.selected.and_then(|i| self.devices.get(i)) }
    fn hit(&mut self, x: i32, y: i32) -> HomeAction {
        if self.login_open {
            // 面板 × 关闭。
            if self.hit_map.iter().any(|(r, a)| matches!(a, HomeAction::Stub(_)) && r.contains_point(Point::new(x, y))) {
                self.login_open = false;
            }
            return HomeAction::None;
        }
        for (r, a) in self.hit_map.iter() {
            if r.contains_point(Point::new(x, y)) { return a.clone(); }
        }
        HomeAction::None
    }
}

/// 填充圆（扫描行近似）。
fn circle(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, cx: i32, cy: i32, r: i32, c: Color) {
    canvas.set_draw_color(c);
    for dy in -r..=r {
        let w = ((r * r - dy * dy) as f64).sqrt() as i32;
        canvas.fill_rect(Rect::new(cx - w, cy + dy, (2 * w + 1) as u32, 1)).ok();
    }
}

/// 在线圆点（保留旧名，等价小圆）。
fn dot(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, x: i32, y: i32, c: Color) {
    circle(canvas, x, y, 4, c);
}

/// 真胶囊（两端半圆）：中间填矩形 + 左右两个半圆，替代 card 的方角擦除近似。
fn capsule(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, r: Rect, fill: Color) {
    canvas.set_draw_color(fill);
    let rad = (r.height() as i32 / 2).max(1);
    canvas.fill_rect(Rect::new(r.x() + rad, r.y(), (r.width() as i32 - 2 * rad).max(1) as u32, r.height())).ok();
    circle(canvas, r.x() + rad, r.y() + rad, rad, fill);
    circle(canvas, r.x() + r.width() as i32 - rad - 1, r.y() + rad, rad, fill);
}

/// 圆角卡：填充 + 用背景色擦角近似圆角，再画直边描线。
fn card(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, r: Rect, fill: Color, bg: Color, rad: i32, border: Option<Color>) {
    canvas.set_draw_color(fill);
    canvas.fill_rect(r).ok();
    canvas.set_draw_color(bg);
    for k in 0..rad {
        let s = rad - k;
        canvas.fill_rect(Rect::new(r.x(), r.y() + k, s as u32, 1)).ok();
        canvas.fill_rect(Rect::new(r.x() + r.width() as i32 - s, r.y() + k, s as u32, 1)).ok();
        canvas.fill_rect(Rect::new(r.x(), r.y() + r.height() as i32 - 1 - k, s as u32, 1)).ok();
        canvas.fill_rect(Rect::new(r.x() + r.width() as i32 - s, r.y() + r.height() as i32 - 1 - k, s as u32, 1)).ok();
    }
    if let Some(b) = border {
        canvas.set_draw_color(b);
        canvas.draw_line(Point::new(r.x() + rad, r.y()), Point::new(r.x() + r.width() as i32 - rad - 1, r.y())).ok();
        canvas.draw_line(Point::new(r.x() + rad, r.y() + r.height() as i32 - 1), Point::new(r.x() + r.width() as i32 - rad - 1, r.y() + r.height() as i32 - 1)).ok();
        canvas.draw_line(Point::new(r.x(), r.y() + rad), Point::new(r.x(), r.y() + r.height() as i32 - rad - 1)).ok();
        canvas.draw_line(Point::new(r.x() + r.width() as i32 - 1, r.y() + rad), Point::new(r.x() + r.width() as i32 - 1, r.y() + r.height() as i32 - rad - 1)).ok();
    }
}

/// 虚线圆角框（官方快速启动区：dash 6px gap 4px）。
fn dash_card(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, r: Rect, fill: Color, bg: Color, dash: Color, rad: i32) {
    card(canvas, r, fill, bg, rad, None);
    canvas.set_draw_color(dash);
    let (x, y, w, h) = (r.x(), r.y(), r.width() as i32, r.height() as i32);
    let mut seg = |a: i32, b: i32, c: i32, d: i32| canvas.draw_line(Point::new(a, b), Point::new(c, d)).ok();
    let mut p = x + rad + 2;
    while p < x + w - rad - 2 { let q = (p + 6).min(x + w - rad - 2); seg(p, y, q, y); seg(p, y + h - 1, q, y + h - 1); p += 10; }
    let mut p = y + rad + 2;
    while p < y + h - rad - 2 { let q = (p + 6).min(y + h - rad - 2); seg(x, p, x, q); seg(x + w - 1, p, x + w - 1, q); p += 10; }
}

/// 折叠箭头（官方圆形箭头按钮近似：两条线拼的 V / ^）。
fn chevron(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, cx: i32, cy: i32, down: bool, c: Color) {
    canvas.set_draw_color(c);
    if down {
        canvas.draw_line(Point::new(cx - 5, cy - 2), Point::new(cx, cy + 3)).ok();
        canvas.draw_line(Point::new(cx, cy + 3), Point::new(cx + 5, cy - 2)).ok();
    } else {
        canvas.draw_line(Point::new(cx - 5, cy + 2), Point::new(cx, cy - 3)).ok();
        canvas.draw_line(Point::new(cx, cy - 3), Point::new(cx + 5, cy + 2)).ok();
    }
}

/// 竖三点（官方右上角"更多"图标）。
fn vdots(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, cx: i32, cy: i32, c: Color) {
    circle(canvas, cx, cy - 6, 2, c);
    circle(canvas, cx, cy, 2, c);
    circle(canvas, cx, cy + 6, 2, c);
}


/// 找带 alpha 通道的 32-bit ARGB visual；SDL 用该 hint 建窗后 mutter 按 alpha 合成（圆角/阴影必需）。
fn argb_visual_id() -> Option<String> {
    use x11rb::connection::Connection;
    let (conn, scr_idx) = x11rb::connect(None).ok()?;
    for depth in conn.setup().roots[scr_idx].allowed_depths.iter() {
        if depth.depth != 32 { continue; }
        for v in depth.visuals.iter() {
            if v.class == x11rb::protocol::xproto::VisualClass::TRUE_COLOR
                && v.red_mask == 0x00ff_0000
                && v.green_mask == 0x0000_ff00
                && v.blue_mask == 0x0000_00ff
            {
                return Some(v.visual_id.to_string());
            }
        }
    }
    None
}

fn force_argb() {
    // alphatest 实测：hint::set（SDL hint 名不带 SDL_HINT_ 前缀）+ GL 渲染器 → 窗口 depth 32，
    // 透明像素被 mutter 按 alpha 合成。
    // SDL 建窗时读这两个环境变量（实测 export 生效、hint::set 不生效）：
    // 32-bit ARGB visual + 禁 bypass-compositor → mutter 按窗口 alpha 合成，圆角外透出桌面。
    // alphatest 实测：hint::set 用 SDL_HINT_ 全名 + GL 默认渲染器 → 窗口 depth 32、alpha 被 mutter 合成。
    let vid = std::env::var("UU_VISUALID").ok().or_else(argb_visual_id).unwrap_or_default();
    if !vid.is_empty() { sdl2::hint::set("SDL_VIDEO_X11_WINDOW_VISUALID", &vid); eprintln!("ARGB visual {vid} 已启用"); } else { eprintln!("没找到 ARGB visual，圆角外会显黑"); }
    sdl2::hint::set("SDL_VIDEO_X11_NET_WM_BYPASS_COMPOSITOR", "0");
}

// 主页卡片几何：官方 920x700，四周 30px 阴影带（0 8px 24px rgba(0,0,0,0.08)），圆角 8。
const CARD_W: i32 = 920;
const CARD_H: i32 = 700;
const MARGIN: i32 = 0;                     // 官方阴影在 X11/mutter 下显成一圈黑，按用户要求去掉边距
const WIN_W: u32 = (CARD_W + 2 * MARGIN) as u32;
const WIN_H: u32 = (CARD_H + 2 * MARGIN) as u32;
const CORNER_R: f32 = 8.0;

/// 圆角卡片到 (px,py) 的带符号距离（负=内部）。
fn rounded_sdf(px: f32, py: f32, ox: f32, oy: f32, w: f32, h: f32, r: f32) -> f32 {
    let qx = (px - (ox + w / 2.0)).abs() - (w / 2.0 - r);
    let qy = (py - (oy + h / 2.0)).abs() - (h / 2.0 - r);
    let oxv = qx.max(0.0);
    let oyv = qy.max(0.0);
    (oxv * oxv + oyv * oyv).sqrt() + qx.max(qy).min(0.0) - r
}

/// 预生成阴影纹理（整窗 RGBA）：卡片外像素 = 黑 alpha 高斯衰减（σ=12≈blur 24，偏移 y+8）。
fn shadow_texture(creator: &TextureCreator<WindowContext>) -> Texture {
    let (w, h) = (WIN_W as usize, WIN_H as usize);
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let d = rounded_sdf(x as f32 + 0.5, y as f32 + 0.5, MARGIN as f32, (MARGIN + 8) as f32, CARD_W as f32, CARD_H as f32, CORNER_R);
            if d > 0.0 {
                let a = 255.0 * 0.08 * (-d * d / (2.0 * 12.0 * 12.0)).exp();
                if a > 0.5 {
                    let i = (y * w + x) * 4;
                    px[i] = 0; px[i + 1] = 0; px[i + 2] = 0; px[i + 3] = a as u8;
                }
            }
        }
    }
    let mut tex = creator.create_texture_streaming(PixelFormatEnum::RGBA32, w as u32, h as u32).expect("阴影纹理失败");
    tex.set_blend_mode(sdl2::render::BlendMode::Blend);
    tex.with_lock(None, |buf, pitch| {
        for row in 0..h {
            buf[row * pitch..row * pitch + w * 4].copy_from_slice(&px[row * w * 4..(row + 1) * w * 4]);
        }
    }).expect("阴影写入失败");
    tex
}

/// 圆角四角蒙版（8x8）：弧内=背景色不透明，弧外 alpha 0；配合擦除+阴影重放裁圆角。
fn corner_texture(creator: &TextureCreator<WindowContext>, bg: Color, tl: bool, tr: bool) -> Texture {
    // (tl,tr) = (1,0)左上 (0,1)右上 (1,1)左下 (0,0)右下；弧内背景色，弧外 alpha 0。
    let n = (CORNER_R.ceil() as usize) + 1;
    let mut px = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let cx = if tl { CORNER_R } else { n as f32 - CORNER_R };
            let cy = if tr { CORNER_R } else { n as f32 - CORNER_R };
            if (fx - cx).powi(2) + (fy - cy).powi(2) <= CORNER_R * CORNER_R {
                let i = (y * n + x) * 4;
                px[i] = bg.r; px[i + 1] = bg.g; px[i + 2] = bg.b; px[i + 3] = 255;
            }
        }
    }
    let mut tex = creator.create_texture_streaming(PixelFormatEnum::RGBA32, n as u32, n as u32).expect("角蒙版失败");
    tex.with_lock(None, |buf, pitch| {
        for row in 0..n {
            buf[row * pitch..row * pitch + n * 4].copy_from_slice(&px[row * n * 4..(row + 1) * n * 4]);
        }
    }).ok();
    tex
}

fn draw_home<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, fonts: &Fonts, home: &mut Home, cache: &mut HashMap<String, Texture<'a>>, qr: Option<&Texture<'a>>, shadow: &Texture<'a>, corners: [&Texture<'a>; 4], mx: i32, my: i32) {
    let (ww, wh) = (CARD_W as u32, CARD_H as u32);
    // SDL 的绘制命令按视口变换，但 clear() 无视视口 → 全程用 fill_rect。
    // 整窗先刷全透明（BlendMode::None 直写 alpha），再铺官方阴影（0 8px 24px rgba(0,0,0,0.08)，偏移 y+8）。
    canvas.set_viewport(None);
    canvas.set_blend_mode(sdl2::render::BlendMode::None);
    canvas.set_draw_color(Color::RGBA(0, 0, 0, 0));
    canvas.fill_rect(Rect::new(0, 0, WIN_W, WIN_H)).ok();
    canvas.set_blend_mode(sdl2::render::BlendMode::Blend);
    canvas.copy(shadow, None, Rect::new(0, 0, WIN_W, WIN_H)).ok();
    // 卡片视口：以下所有绘制坐标 = 920x700 卡片坐标
    canvas.set_viewport(Rect::new(MARGIN, MARGIN, CARD_W as u32, CARD_H as u32));
    canvas.set_draw_color(MAIN_BG);
    canvas.fill_rect(Rect::new(0, 0, ww, wh)).ok();
    home.hit_map.clear();

    // ---- 左侧栏 232px 全高（HTML .sidebar：#F5F6F8 底，logo 在侧栏顶部，与主区无边框线）----
    canvas.set_draw_color(SIDE_BG);
    canvas.fill_rect(Rect::new(0, 0, SIDE_W as u32, wh)).ok();
    // 官方品牌图 1a7ddd73c74d.svg（logo+「网易UU远程」字标合体）＝HTML .logo-area（padding 0 8 16 8）
    if let Some(tex) = icon_texture(creator, icons::BRAND) {
        canvas.copy(&tex, None, Rect::new(8, 14, 94, 32)).ok();
    }

    // ---- 主区顶栏（HTML .top-bar：40px、右对齐、gap 16、padding 0 16；侧栏不加顶条）----
    let topbar: [(&[u8], HomeAction); 6] = [
        (icons::TOP_SUBSCREEN, HomeAction::Stub("无线副屏".into())),
        (icons::TOP_BELL, HomeAction::Stub("消息中心".into())),
        (icons::TOP_USER, HomeAction::Stub("账户".into())),
        (icons::TOP_MORE, HomeAction::Stub("更多".into())),
        (icons::TOP_MINIMIZE, HomeAction::Minimize),
        (icons::TOP_CLOSE, HomeAction::Close),
    ];
    for (i, (png, action)) in topbar.iter().enumerate() {
        // 顶栏细线图标资源是 96px 细线 PNG，缩太小笔画就糊；按用户要求逐步放大到 40px（资源不动）；步进 56。
        let ix = ww as i32 - PAD / 2 - 40 - (5 - i as i32) * 56;
        let r = Rect::new(ix - 8, TOPBAR_H / 2 - 24, 56, 48);
        if r.contains_point(Point::new(mx, my)) {
            card(canvas, r, SELECTED, MAIN_BG, 4, None);
        }
        if let Some(tex) = icon_texture(creator, png) {
            canvas.copy(&tex, None, Rect::new(ix, TOPBAR_H / 2 - 20, 40, 40)).ok();
        }
        home.hit_map.push((r, action.clone()));
    }

    // ---- 侧栏菜单（HTML：.menu-group margin-top 12；.menu-title 12px 三级灰 + 折叠箭头；.menu-item inset 8、高 32、圆角 6、行距 2）----
    let mut y = 14 + 32 + 12;
    let hdr = Rect::new(0, y, SIDE_W as u32, 28);
    htext(canvas, creator, fonts.r12.as_ref(), cache, "我的设备", 12, 20, hdr.y() + 14, TEXT_MUT);
    chevron(canvas, SIDE_W - 22, hdr.y() + 14, !home.groups_open[0], TEXT_MUT);
    home.hit_map.push((hdr, HomeAction::ToggleGroup(0)));
    y += 32;
    if home.groups_open[0] {
        for (i, d) in home.devices.iter().enumerate() {
            let r = Rect::new(8, y, (SIDE_W - 16) as u32, 32);
            side_row(canvas, creator, fonts.r13.as_ref(), cache, Some(if d.online { icons::HOME_MONITOR_ON } else { icons::HOME_MONITOR_OFF }), &d.alias, r, home.selected == Some(i), r.contains_point(Point::new(mx, my)));
            home.hit_map.push((r, HomeAction::Select(i)));
            y += 34;
        }
        if home.devices.is_empty() {
            htext(canvas, creator, fonts.r13.as_ref(), cache, if home.logged_in { "（无设备）" } else { "（未登录）" }, 13, 20, y + 16, TEXT_MUT);
            y += 34;
        }
        let r_all = Rect::new(8, y, (SIDE_W - 16) as u32, 32);
        side_row(canvas, creator, fonts.r13.as_ref(), cache, Some(icons::HOME_GRID_BLUE), "全部设备", r_all, false, r_all.contains_point(Point::new(mx, my)));
        home.hit_map.push((r_all, HomeAction::Stub("全部设备".into())));
        y += 34;
    }
    let hdr2 = Rect::new(0, y + 12, SIDE_W as u32, 28);
    htext(canvas, creator, fonts.r12.as_ref(), cache, "远程协助", 12, 20, hdr2.y() + 14, TEXT_MUT);
    chevron(canvas, SIDE_W - 22, hdr2.y() + 14, !home.groups_open[1], TEXT_MUT);
    home.hit_map.push((hdr2, HomeAction::ToggleGroup(1)));
    y = hdr2.y() + 32;
    if home.groups_open[1] {
        for (label, icon, act) in [("开始协助", icons::HOME_USER_BLUE, HomeAction::Stub("远程协助".into())), ("收藏设备", icons::HOME_STAR_BLUE, HomeAction::Stub("收藏设备".into()))] {
            let r = Rect::new(8, y, (SIDE_W - 16) as u32, 32);
            side_row(canvas, creator, fonts.r13.as_ref(), cache, Some(icon), label, r, false, r.contains_point(Point::new(mx, my)));
            home.hit_map.push((r, act));
            y += 34;
        }
    }
    // .sidebar-bottom：margin-top auto → 钉在侧栏底部（padding 16）。
    let r_set = Rect::new(8, wh as i32 - 16 - 32, (SIDE_W - 16) as u32, 32);
    side_row(canvas, creator, fonts.r13.as_ref(), cache, Some(icons::HOME_GEAR), "设置", r_set, false, r_set.contains_point(Point::new(mx, my)));
    home.hit_map.push((r_set, HomeAction::Stub("设置".into())));

    // ---- 主区（HTML .content-body：padding 32）----
    let cx = SIDE_W + PAD;
    let cw = (ww as i32 - cx - PAD).max(320) as u32;
    let cy = TOPBAR_H + PAD;
    if !home.logged_in {
        htext(canvas, creator, fonts.s20.as_ref(), cache, "全部设备", 20, cx, cy + 10, TEXT_MAIN);
        htext(canvas, creator, fonts.r13.as_ref(), cache, "随时访问个人设备，可用于办公、游戏、文件传输。", 13, cx, cy + 46, TEXT_MUT);
        if !home.login_open {
            let r = pill(canvas, creator, fonts.r13.as_ref(), cache, "立即登录", 13, cx + 90, cy + 150, BRAND_BLUE, WHITE, 28, 10, MAIN_BG);
            home.hit_map.push((r, HomeAction::Login));
        } else {
            draw_login_panel(canvas, creator, fonts.r16.as_ref(), cache, home, qr, cx, cy + 40);
        }
    } else if home.session_busy {
        htext(canvas, creator, fonts.r13.as_ref(), cache, "远控会话进行中……", 13, cx, cy + 12, TEXT_MUT);
        htext(canvas, creator, fonts.r13.as_ref(), cache, "请在远程窗口断开后再连接其他设备。", 13, cx, cy + 40, TEXT_MUT);
    } else if let Some((alias, online, controllable)) = home.selected_device().map(|d| (d.alias.clone(), d.online, d.controllable)) {
        // ---- .device-header：深色胶囊（绿点 + 白字 12px、padding 4 10）+ 设备名 20px/600 + 竖三点，gap 12 ----
        let hc = cy + 11;
        let tag_label = if online { "在线" } else { "离线" };
        let (tw, th) = measure(creator, fonts.r12.as_ref(), cache, tag_label, 12, WHITE);
        let tag = Rect::new(cx, hc - (th + 8) / 2, (tw + 32) as u32, (th + 8) as u32);
        capsule(canvas, tag, PILL_DARK);
        circle(canvas, tag.x() + 13, hc, 3, GREEN);
        if let Some(tex) = cache.get(&format!("{tag_label}|12|{}|{}|{}", WHITE.r, WHITE.g, WHITE.b)) {
            canvas.copy(tex, None, Rect::new(tag.x() + 22, hc - th / 2, tw as u32, th as u32)).ok();
        }
        let name_x = tag.x() + tag.width() as i32 + 12;
        let nw = htext(canvas, creator, fonts.s20.as_ref(), cache, &alias, 20, name_x, hc, TEXT_MAIN);
        let dots_x = name_x + nw + 16;
        vdots(canvas, dots_x, hc, TEXT_MUT);
        home.hit_map.push((Rect::new(dots_x - 10, hc - 12, 20, 24), HomeAction::Stub("设备菜单".into())));

        // ---- .preview-container：高 248、圆角 8、#E5E6EB 底（壁纸位：在线蓝色 hero / 离线灰渐变）----
        let pv = Rect::new(cx, tag.y() + tag.height() as i32 + 24, cw, 248);
        card(canvas, pv, PREVIEW_BG, MAIN_BG, 8, None);
        let inner = Rect::new(pv.x() + 1, pv.y() + 1, pv.width() - 2, pv.height() - 2);
        let mid = if online { Color::RGB(108, 160, 233) } else { Color::RGB(215, 223, 231) };
        for k in 0..40u32 {
            let t = k as f64 / 39.0;
            let c = if online {
                Color::RGB(lerp(150, 66, t) as u8, lerp(210, 110, t) as u8, lerp(250, 216, t) as u8)
            } else {
                Color::RGB(lerp(232, 198, t) as u8, lerp(238, 208, t) as u8, lerp(244, 218, t) as u8)
            };
            canvas.set_draw_color(c);
            let yy = inner.y() + (k as i32 * inner.height() as i32 / 40);
            canvas.fill_rect(Rect::new(inner.x(), yy, inner.width(), inner.height() / 40 + 1)).ok();
        }
        // ---- .enter-desktop：居中半透明黑胶囊（rgba(0,0,0,.5)、radius 16、padding 6 16、白字 13）----
        if online && controllable {
            let r = pill(canvas, creator, fonts.r13.as_ref(), cache, "进入桌面 →", 13, pv.x() + pv.width() as i32 / 2, pv.y() + pv.height() as i32 / 2, Color::RGBA(0, 0, 0, 127), WHITE, 16, 6, mid);
            home.hit_map.push((r, HomeAction::Enter(false)));
        } else {
            let (s, c) = if online { ("该设备不可远控", WHITE) } else { ("设备离线", TEXT_MUT) };
            let w = measure(creator, fonts.r13.as_ref(), cache, s, 13, c).0;
            htext(canvas, creator, fonts.r13.as_ref(), cache, s, 13, pv.x() + pv.width() as i32 / 2 - w / 2, pv.y() + pv.height() as i32 / 2, c);
        }

        // ---- .action-bar：紧贴预览下方、48px、border 1px 但 border-top: none、底部圆角 8 ----
        let band = Rect::new(cx, pv.y() + pv.height() as i32, cw, 48);
        canvas.set_draw_color(MAIN_BG);
        canvas.fill_rect(band).ok();
        canvas.set_draw_color(CARD_BORDER);
        canvas.draw_line(Point::new(band.x(), band.y()), Point::new(band.x(), band.y() + band.height() as i32 - 1)).ok();
        canvas.draw_line(Point::new(band.x() + band.width() as i32 - 1, band.y()), Point::new(band.x() + band.width() as i32 - 1, band.y() + band.height() as i32 - 1)).ok();
        canvas.draw_line(Point::new(band.x() + 8, band.y() + band.height() as i32 - 1), Point::new(band.x() + band.width() as i32 - 9, band.y() + band.height() as i32 - 1)).ok();
        for k in 0..8 {
            let s = 8 - k;
            canvas.set_draw_color(MAIN_BG);
            canvas.fill_rect(Rect::new(band.x(), band.y() + band.height() as i32 - 1 - k, s as u32, 1)).ok();
            canvas.fill_rect(Rect::new(band.x() + band.width() as i32 - s, band.y() + band.height() as i32 - 1 - k, s as u32, 1)).ok();
        }
        // 右侧九宫格单元（HTML：padding-left 16 + border-left 1px）；图标与尺寸不动。
        let grid_x = band.x() + band.width() as i32 - 16 - 22;
        canvas.set_draw_color(CARD_BORDER);
        canvas.fill_rect(Rect::new(grid_x - 16, band.y() + 16, 1, 16)).ok();
        if let Some(tex) = icon_texture(creator, icons::HOME_GRID_BLUE) {
            canvas.copy(&tex, None, Rect::new(grid_x, band.y() + 24 - 11, 22, 22)).ok();
        }
        home.hit_map.push((Rect::new(grid_x - 16, band.y(), (band.x() + band.width() as i32 - grid_x + 16) as u32, band.height()), HomeAction::Stub("快捷工具".into())));
        // 四个功能项 .action-items（space-around）：图标 + gap 6 + 13px 文字；项间 .action-divider 1x16；hover 文字变主色。
        let feats: Vec<(&[u8], String, HomeAction, bool)> = vec![
            (icons::HOME_FOLDER, "文件传输".into(), HomeAction::Stub("文件传输".into()), true),
            (icons::HOME_EYE, "观看模式".into(), HomeAction::Enter(true), online && controllable),
            (icons::HOME_POWER, "关机".into(), HomeAction::Stub("关机".into()), online),
            (icons::HOME_TERMINAL, "终端".into(), HomeAction::Stub("终端".into()), online),
        ];
        let widths: Vec<i32> = feats.iter().map(|(_, label, _, _)| 22 + 6 + measure(creator, fonts.r13.as_ref(), cache, label, 13, TEXT_DARK).0).collect();
        let (avail_x, avail_right) = (band.x() + 16, grid_x - 32);
        let sum: i32 = widths.iter().sum();
        let gap = ((avail_right - avail_x - sum) / (feats.len() as i32 + 1)).max(6);
        let mut px = avail_x + gap;
        let mut prev_right = avail_x;
        for (i, (icon, label, action, enabled)) in feats.into_iter().enumerate() {
            let w = widths[i];
            let ir = Rect::new(px, band.y(), w as u32, band.height());
            if i > 0 {
                canvas.set_draw_color(CARD_BORDER);
                canvas.fill_rect(Rect::new((prev_right + px) / 2, band.y() + 16, 1, 16)).ok();
            }
            if let Some(mut tex) = icon_texture(creator, icon) {
                if !enabled { tex.set_color_mod(200, 204, 209); }
                canvas.copy(&tex, None, Rect::new(px, band.y() + 24 - 11, 22, 22)).ok();
            }
            let color = if !enabled { Color::RGB(190, 194, 199) } else if ir.contains_point(Point::new(mx, my)) { BRAND_BLUE } else { TEXT_DARK };
            htext(canvas, creator, fonts.r13.as_ref(), cache, &label, 13, px + 22 + 6, band.y() + 24, color);
            if enabled { home.hit_map.push((ir, action)); }
            prev_right = px + w;
            px += w + gap;
        }

        // ---- .quick-start：margin-top 24；.section-title 13px/600；.dashed-box 高 184、虚线 #C9CDD4、圆角 8 ----
        let qy = band.y() + band.height() as i32 + 24;
        htext(canvas, creator, fonts.s13.as_ref(), cache, "快速启动", 13, cx, qy + 10, TEXT_MAIN);
        let qb = Rect::new(cx, qy + 20 + 12, cw, 184);
        dash_card(canvas, qb, MAIN_BG, MAIN_BG, DASH, 8);
        let ccx = qb.x() + qb.width() as i32 / 2;
        let box_cy = qb.y() + qb.height() as i32 / 2;
        // .add-btn：32px 圆 #F2F3F5 + 16px 加号；margin-bottom 8 → .add-text 12px 三级灰
        let ccy = box_cy - 11;
        circle(canvas, ccx, ccy, 16, ADD_BG);
        canvas.set_draw_color(TEXT_DARK);
        canvas.fill_rect(Rect::new(ccx - 8, ccy - 1, 16, 2)).ok();
        canvas.fill_rect(Rect::new(ccx - 1, ccy - 8, 2, 16)).ok();
        let aw = measure(creator, fonts.r12.as_ref(), cache, "添加", 12, TEXT_MUT).0;
        htext(canvas, creator, fonts.r12.as_ref(), cache, "添加", 12, ccx - aw / 2, box_cy + 20, TEXT_MUT);
        home.hit_map.push((qb, HomeAction::Stub("快速启动".into())));
    } else {
        htext(canvas, creator, fonts.r13.as_ref(), cache, "选择左侧的设备后点「进入桌面」。", 13, cx, cy + 20, TEXT_MUT);
    }

    if !home.status_msg.is_empty() {
        htext(canvas, creator, fonts.r12.as_ref(), cache, &home.status_msg, 12, cx, wh as i32 - 20, TEXT_MUT);
    }


    // ---- 圆角 8px：擦掉四角像素 → 重放该角阴影 → 铺背景弧内像素（fill_rect，不用 clear）----
    canvas.set_viewport(None);
    let n = CORNER_R.ceil() as u32 + 1;
    let rects = [
        Rect::new(MARGIN, MARGIN, n, n),
        Rect::new(WIN_W as i32 - MARGIN - n as i32, MARGIN, n, n),
        Rect::new(MARGIN, WIN_H as i32 - MARGIN - n as i32, n, n),
        Rect::new(WIN_W as i32 - MARGIN - n as i32, WIN_H as i32 - MARGIN - n as i32, n, n),
    ];
    for (i, r) in rects.iter().enumerate() {
        canvas.set_blend_mode(sdl2::render::BlendMode::None);
        canvas.set_draw_color(Color::RGBA(0, 0, 0, 0));
        canvas.fill_rect(*r).ok();
        canvas.set_blend_mode(sdl2::render::BlendMode::Blend);
        canvas.copy(shadow, *r, *r).ok();
        canvas.copy(corners[i], None, *r).ok();
    }
    canvas.set_viewport(Rect::new(MARGIN, MARGIN, CARD_W as u32, CARD_H as u32));
}


fn draw_login_panel<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, cache: &mut HashMap<String, Texture<'a>>, home: &mut Home, qr: Option<&Texture<'a>>, mx: i32, my: i32) {
    let panel = Rect::new(mx, my, 520, 340);
    card(canvas, panel, Color::RGB(252, 253, 255), MAIN_BG, 10, Some(CARD_BORDER));
    text(canvas, creator, font, cache, "网易UU远程", panel.x() + 16, panel.y() + 12, TEXT_DARK);
    let close = Rect::new(panel.x() + panel.width() as i32 - 34, panel.y() + 8, 26, 26);
    text(canvas, creator, font, cache, "×", close.x() + 7, close.y() + 3, TEXT_MUT);
    home.hit_map.push((close, HomeAction::Stub("close".into())));
    text(canvas, creator, font, cache, "扫码登录", panel.x() + 40, panel.y() + 56, TEXT_DARK);
    if let Some(tex) = qr {
        let q = tex.query();
        canvas.copy(tex, None, Rect::new(panel.x() + 40, panel.y() + 84, q.width, q.height)).ok();
    } else {
        canvas.set_draw_color(Color::RGB(240, 242, 245));
        canvas.fill_rect(Rect::new(panel.x() + 40, panel.y() + 84, 200, 200)).ok();
    }
    text(canvas, creator, font, cache, "请使用网易UU远程或微信 扫码登录", panel.x() + 40, panel.y() + 296, TEXT_MUT);
    let rx = panel.x() + 280;
    text(canvas, creator, font, cache, &home.login_status, rx, panel.y() + 90, TEXT_DARK);
    text(canvas, creator, font, cache, "短信登录暂未实现", rx, panel.y() + 130, TEXT_MUT);
}

/// X 服务器连接（同项目输入注入用的 x11rb）；连不上返回 None（无 X 环境）。
type XConn = Option<(x11rb::rust_connection::RustConnection, usize)>;

fn x_connect() -> XConn {
    let (conn, screen) = x11rb::connect(None).ok()?;
    Some((conn, screen))
}

/// 服务器指针真值 + 左键状态（XQueryPointer：不受 SDL 抓鼠钳制、事件积压、搬窗延迟影响）。
fn x_query(c: &XConn) -> Option<(i32, i32, bool)> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, KeyButMask};
    let (conn, screen) = c.as_ref()?;
    let root = conn.setup().roots[*screen].root;
    let r = conn.query_pointer(root).ok()?.reply().ok()?;
    let down = u16::from(KeyButMask::BUTTON1) & u16::from(r.mask) != 0;
    Some((r.root_x as i32, r.root_y as i32, down))
}


/// 异步预发 XQueryPointer（不阻塞），下一帧收回复——拖动循环把它当采样器用。
fn x_query_send(c: &XConn) -> Option<x11rb::cookie::Cookie<'_, x11rb::rust_connection::RustConnection, x11rb::protocol::xproto::QueryPointerReply>> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::ConnectionExt as _;
    let (conn, screen) = c.as_ref()?;
    let root = conn.setup().roots[*screen].root;
    conn.query_pointer(root).ok()
}
fn lerp(a: i32, b: i32, t: f64) -> i32 { (a as f64 + (b - a) as f64 * t) as i32 }

/// 渲染文本缓存；缺字体时静默（形状仍然可见）。
fn text<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, cache: &mut HashMap<String, Texture<'a>>, s: &str, x: i32, y: i32, color: Color) {
    let Some(font) = font else { return };
    let key = format!("{s}|{}|{}|{}", color.r, color.g, color.b);
    if !cache.contains_key(&key) {
        if let Some(tex) = font.render(s).blended(color).ok().and_then(|surf| creator.create_texture_from_surface(&surf).ok()) { cache.insert(key.clone(), tex); }
    }
    if let Some(tex) = cache.get(&key) {
        let q = tex.query();
        canvas.copy(tex, None, Rect::new(x, y, q.width, q.height)).ok();
    }
}
fn text_center<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, cache: &mut HashMap<String, Texture<'a>>, s: &str, r: Rect, color: Color) {
    let Some(font) = font else { return };
    let key = format!("{s}|{}|{}|{}", color.r, color.g, color.b);
    if !cache.contains_key(&key) {
        if let Some(tex) = font.render(s).blended(color).ok().and_then(|surf| creator.create_texture_from_surface(&surf).ok()) { cache.insert(key.clone(), tex); }
    }
    if let Some(tex) = cache.get(&key) {
        let q = tex.query();
        canvas.copy(tex, None, Rect::new(r.x() + (r.width() - q.width) as i32 / 2, r.y() + (r.height() - q.height) as i32 / 2, q.width, q.height)).ok();
    }
}

/// 主页字号集（HTML 排版规格：12/13 正文 + 13/20 的 600 字重标题；16 常规给远程窗口与登录面板）。
/// SDL2_ttf 的 init() 是全局单例，一个进程只能初始化一次，所以主页要的字号全在这一处加载。
struct Fonts {
    r12: Option<sdl2::ttf::Font<'static, 'static>>,
    r13: Option<sdl2::ttf::Font<'static, 'static>>,
    r16: Option<sdl2::ttf::Font<'static, 'static>>,
    s13: Option<sdl2::ttf::Font<'static, 'static>>,
    s20: Option<sdl2::ttf::Font<'static, 'static>>,
}

fn load_font_at(ctx: &'static sdl2::ttf::Sdl2TtfContext, paths: &[(&str, Option<u32>)], size: u16) -> Option<sdl2::ttf::Font<'static, 'static>> {
    for (path, index) in paths {
        let font = match index {
            Some(i) => ctx.load_font_at_index(path, *i, size),
            None => ctx.load_font(path, size),
        };
        if let Ok(font) = font { return Some(font); }
    }
    None
}

fn load_fonts() -> Fonts {
    let empty = || Fonts { r12: None, r13: None, r16: None, s13: None, s20: None };
    let Ok(ctx) = sdl2::ttf::init() else { return empty() };
    let ctx: &'static sdl2::ttf::Sdl2TtfContext = Box::leak(Box::new(ctx));
    const REG: [(&str, Option<u32>); 3] = [
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", Some(0)),
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-DemiLight.ttc", Some(0)),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", None),
    ];
    // HTML 的 600 字重 → Noto Sans CJK Medium（没有 SemiBold，Medium 最接近；再退 Bold）。
    const SEMI: [(&str, Option<u32>); 3] = [
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Medium.ttc", Some(0)),
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc", Some(0)),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", None),
    ];
    Fonts {
        r12: load_font_at(ctx, &REG, 12),
        r13: load_font_at(ctx, &REG, 13),
        r16: load_font_at(ctx, &REG, 16),
        s13: load_font_at(ctx, &SEMI, 13),
        s20: load_font_at(ctx, &SEMI, 20),
    }
}

/// 取（并缓存）指定字号/颜色的文字纹理，返回占宽占高——HTML 的 flex/gap 排布要先量再画。
fn measure<'a>(creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, cache: &mut HashMap<String, Texture<'a>>, s: &str, size: u16, color: Color) -> (i32, i32) {
    let key = format!("{s}|{size}|{}|{}|{}", color.r, color.g, color.b);
    if !cache.contains_key(&key) {
        if let Some(tex) = font.and_then(|f| f.render(s).blended(color).ok()).and_then(|surf| creator.create_texture_from_surface(&surf).ok()) {
            cache.insert(key.clone(), tex);
        }
    }
    cache.get(&key).map(|tex| { let q = tex.query(); (q.width as i32, q.height as i32) }).unwrap_or((s.len() as i32 * size as i32 / 2 + 4, size as i32 + 2))
}

/// 主页文本：显式字号，垂直居中于 `y_center`，返回占宽（便于接着排下一个元素）。
fn htext<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, cache: &mut HashMap<String, Texture<'a>>, s: &str, size: u16, x: i32, y_center: i32, color: Color) -> i32 {
    let (w, h) = measure(creator, font, cache, s, size, color);
    let key = format!("{s}|{size}|{}|{}|{}", color.r, color.g, color.b);
    if let Some(tex) = cache.get(&key) {
        canvas.copy(tex, None, Rect::new(x, y_center - h / 2, w as u32, h as u32)).ok();
    }
    w
}

/// 胶囊按钮（HTML padding/radius 规格）：胶囊宽按文字实际宽度算，返回命中矩形。
fn pill<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, cache: &mut HashMap<String, Texture<'a>>, label: &str, size: u16, cx: i32, cy: i32, bg: Color, fg: Color, padx: i32, pady: i32, corner_bg: Color) -> Rect {
    let (w, h) = measure(creator, font, cache, label, size, fg);
    let r = Rect::new(cx - (w + 2 * padx) / 2, cy - (h + 2 * pady) / 2, (w + 2 * padx) as u32, (h + 2 * pady) as u32);
    capsule(canvas, r, bg);
    let key = format!("{label}|{size}|{}|{}|{}", fg.r, fg.g, fg.b);
    if let Some(tex) = cache.get(&key) {
        canvas.copy(tex, None, Rect::new(r.x() + padx, r.y() + pady, w as u32, h as u32)).ok();
    }
    r
}

/// 侧栏一行（HTML .menu-item：inset 8、高 32、圆角 6、hover 浅底；选中＝浅底＋左 3px 蓝条）。
/// 图标保持原线条图与 22px 显示尺寸不动，文字用 13px 二级色。
fn side_row<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, cache: &mut HashMap<String, Texture<'a>>, png: Option<&[u8]>, label: &str, r: Rect, selected: bool, hovered: bool) {
    if selected {
        card(canvas, r, SELECTED, SIDE_BG, 6, None);
        canvas.set_draw_color(BRAND_BLUE);
        canvas.fill_rect(Rect::new(r.x(), r.y() + 8, 3, (r.height() as i32 - 16) as u32)).ok();
    } else if hovered {
        card(canvas, r, HOVER_ROW, SIDE_BG, 6, None);
    }
    let cy = r.y() + r.height() as i32 / 2;
    let mut text_x = r.x() + 12;
    if let Some(png) = png {
        if let Some(tex) = icon_texture(creator, png) {
            canvas.copy(&tex, None, Rect::new(r.x() + 12, cy - 11, 22, 22)).ok();
        }
        text_x = r.x() + 12 + 22 + 8;
    }
    htext(canvas, creator, font, cache, label, 13, text_x, cy, TEXT_DARK);
}

enum LoginUpdate { Qr(String), Status(String), LoggedIn, Failed(String) }

/// 扫码登录（与 uu-login 同链路），把状态推给主页 UI。
async fn qr_login_flow(tx: std::sync::mpsc::Sender<LoginUpdate>) {
    let creds = Credentials::default();
    let client = ApiClient::new(creds.clone());
    let (status, body) = match client.post("/api/v1/qrcode/gen/login", None, Duration::from_secs(12)).await { Ok(v) => v, Err(e) => { let _ = tx.send(LoginUpdate::Failed(e)); return; } };
    let gen = match api_data("gen/login", status, &body) { Ok(v) => v, Err(e) => { let _ = tx.send(LoginUpdate::Failed(e)); return; } };
    let url = match gen.get("qrcode_jump_url").and_then(Value::as_str) { Some(u) => u.to_owned(), None => { let _ = tx.send(LoginUpdate::Failed("gen/login 没有跳转链接".into())); return; } };
    let ticket = match api::qr_ticket(&gen) { Some(t) => t.to_owned(), None => { let _ = tx.send(LoginUpdate::Failed("gen/login 没有状态票".into())); return; } };
    let _ = tx.send(LoginUpdate::Qr(url.clone()));
    let mut last = 1i64;
    loop {
        let (status, body) = client
            .post("/api/v1/qrcode/login/status", Some(&json!({ "token": ticket, "qrcode_jump_url": url, "login_status": last })), Duration::from_secs(60))
            .await
            .unwrap_or((0, Value::Null));
        match api::qr_poll_action(last, status, &body) {
            QrPollAction::Wait(s) => last = s,
            QrPollAction::Scanned => { let _ = tx.send(LoginUpdate::Status("已扫码，请在手机上确认……".into())); last = 2; }
            QrPollAction::Refresh(_) => { let _ = tx.send(LoginUpdate::Status("二维码已过期，关掉面板重新点「立即登录」刷新".into())); return; }
            QrPollAction::Failed(msg) => { let _ = tx.send(LoginUpdate::Failed(msg.to_owned())); return; }
            QrPollAction::Confirmed => break,
        }
    }
    let (status, body) = match client.post_json("/api/v1/login/by_qrcode", &json!({ "token": ticket, "qrcode_jump_url": url })).await { Ok(v) => v, Err(e) => { let _ = tx.send(LoginUpdate::Failed(e)); return; } };
    let data = match api_data("login/by_qrcode", status, &body) { Ok(v) => v, Err(e) => { let _ = tx.send(LoginUpdate::Failed(e)); return; } };
    let token = match data.get("token").and_then(Value::as_str) { Some(t) => t.to_owned(), None => { let _ = tx.send(LoginUpdate::Failed("by_qrcode 没有 token".into())); return; } };
    let csrf = api::csrf_from_token(&token).unwrap_or_default();
    let mut file = config::read().ok().flatten().unwrap_or_else(|| json!({}));
    if let Some(map) = file.as_object_mut() {
        map.insert("token".into(), json!(token));
        map.insert("csrf".into(), json!(csrf));
        if let Some(u) = data.get("user_id").and_then(Value::as_str) { map.insert("user_id".into(), json!(u)); }
        if let Some(n) = data.get("nickname").and_then(Value::as_str) { map.insert("nickname".into(), json!(n)); }
    }
    // 全新登录但没有设备身份：补注册（token 已写进 file，用合并后的凭证注册）。
    let mut c = Credentials::load(Some(&file)).unwrap_or_default();
    if c.client_id.is_empty() { c.client_id = api::random_uuid(); }
    if c.system_id.is_empty() { c.system_id = api::random_uuid(); }
    if c.device_id.is_empty() {
        let api = ApiClient::new(c.clone());
        match api::register_device(&api, "1920x1080").await {
            Ok(id) => c.device_id = id,
            Err(e) => { let _ = tx.send(LoginUpdate::Failed(format!("登录成功但设备注册失败：{e}"))); return; }
        }
        if let Some(map) = file.as_object_mut() {
            map.insert("client_id".into(), json!(c.client_id));
            map.insert("system_id".into(), json!(c.system_id));
            map.insert("device_id".into(), json!(c.device_id));
        }
    }
    match config::write(&file) { Ok(_) => { let _ = tx.send(LoginUpdate::LoggedIn); } Err(e) => { let _ = tx.send(LoginUpdate::Failed(e)); } }
}

async fn load_base_creds() -> Result<Credentials, String> {
    let (file, _borrowed) = config::read_with_fallback()?;
    Credentials::load(file.as_ref())
}

fn resolve_device(devs: &[Value], creds: &Credentials, want: &str) -> Option<String> {
    devs.iter()
        .find(|d| d.get("device_id").and_then(Value::as_str) == Some(want))
        .or_else(|| devs.iter().find(|d| d.get("alias").and_then(Value::as_str).is_some_and(|a| a.contains(want))))
        .and_then(|d| d.get("device_id").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| if want == creds.device_id { Some(want.to_owned()) } else { None })
}

// ---------------- 远程画面窗口 ----------------

/// 官方 UU 远程应用图标（从 GameViewer.exe RT_ICON 抽取的 256px RGBA PNG）。
/// SDL2 X11 后端把 SDL_SetWindowIcon 映射为 _NET_WM_ICON（任务栏/切换器用）；
/// GNOME 标题栏本身不绘制图标，属正常。
fn set_app_icon(window: &mut sdl2::video::Window) {
    if let Ok(mut img) = linux_uuremote::viewer::png::decode(icons::APP) {
        if let Ok(surf) = sdl2::surface::Surface::from_data(&mut img.rgba, img.width, img.height, img.width * 4, PixelFormatEnum::RGBA8888) {
            window.set_icon(surf);
        }
    }
}

fn run_session(sdl: &Sdl, video: &sdl2::VideoSubsystem, font: Option<&sdl2::ttf::Font<'static, 'static>>, creds: &Credentials, target: &str, alias: &str, quality: u64, fps: u32, win_w: u32, win_h: u32, view_only: bool, no_audio: bool, event_pump: &mut sdl2::EventPump) -> Result<(), String> {
    let title = if alias.is_empty() { "UU 远程".to_owned() } else { format!("UU 远程 · {alias}") };
    let mut canvas = video.window(&title, win_w, win_h).position_centered().resizable().borderless().build().map_err(|e| e.to_string())?.into_canvas().build().map_err(|e| e.to_string())?;
    canvas.window_mut().set_bordered(false);
    set_app_icon(canvas.window_mut());
    canvas.window_mut().set_always_on_top(true);
    let texture_creator = canvas.texture_creator();
    let mouse_sys = sdl.mouse();

    let audio_ring: Arc<Mutex<std::collections::VecDeque<f32>>> = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let audio_queue = sdl
        .audio()
        .ok()
        .and_then(|audio| audio.open_queue::<f32, Option<&str>>(None, &sdl2::audio::AudioSpecDesired { freq: Some(48000), channels: Some(2), samples: Some(4096) }).ok());
    if let Some(q) = &audio_queue { q.resume(); }

    let stats = Arc::new(Stats::default());
    stats.audio_enabled.store(!no_audio && audio_queue.is_some(), Ordering::Relaxed);
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(2);
    let (cursor_tx, cursor_rx) = std::sync::mpsc::sync_channel::<CursorImage>(2);
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    // 剪贴板桥（控制端本机 X11）：远端复制→写本机 CLIPBOARD，本机复制→推给被控端。
    let disp = std::env::var("DISPLAY").ok().filter(|d| !d.is_empty());
    let clip = linux_uuremote::clipboard::start_x11(disp.as_deref()).map(Arc::new);
    let host = Arc::new(Host { stats: stats.clone(), frames: frame_tx, cursor: cursor_tx, audio: audio_ring.clone(), pings: Arc::new(Mutex::new(HashMap::new())), clipboard: clip, clip_asm: Default::default() });

    // 自连：控制端换用注册的虚拟第二机身份（云端不许同设备控制自己）。
    let mut creds = clone_creds(creds);
    let session_target = target.to_owned();
    if session_target == creds.device_id {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        match rt.block_on(viewer_identity(&ApiClient::new(clone_creds(&creds)))) {
            Ok((did, cid, sid)) => { println!("提示：目标就是本机，控制端以虚拟第二机身份接入（device_id={did}）。"); creds.device_id = did; creds.client_id = cid; creds.system_id = sid; }
            Err(e) => println!("提示：虚拟身份注册失败（{e}），仍用本机身份试连。"),
        }
    }
    let shape = wire::Shape::local(win_w, win_h, fps);

    let (conn_tx, conn_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let session_notify = conn_tx.clone();
    let worker = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            match Session::open(&room_or_die(creds, &session_target).await, &shape, &session_target, host.clone(), Duration::from_secs(20)).await {
                Ok(mut session) => {
                    let _ = conn_tx.send(Ok(()));
                    let result = session.run(cmd_rx, host.clone()).await;
                    if let Err(e) = &result { println!("   会话结束：{e}"); }
                    session.close().await;
                    let _ = session_notify.send(result);
                }
                Err(e) => { println!("   连接失败：{e}"); let _ = conn_tx.send(Err(e)); }
            }
        });
    });

    let mut ui = Ui::new(quality, fps, view_only);
    let mut video_tex: Option<Texture> = None;
    let mut frame_size = (0usize, 0usize);
    let mut cursor_tex: Option<(Texture, i32, i32)> = None;
    let mut mouse_at = Point::new(0, 0);
    let mut resized = false;
    let mut seq: u64 = 5_000_000;
    let mut rpc_id: u64 = 1;
    let mut last_frames = 0u64;
    let mut last_bytes = 0u64;
    let mut stat_since = Instant::now();
    let mut audio_next = Instant::now();
    let mut err: Option<String> = None;
    let mut last_lag_toast = Instant::now() - Duration::from_secs(60);

    match conn_rx.recv_timeout(Duration::from_secs(40)) {
        Ok(Ok(())) => println!("已连接：键鼠已透传，F1 菜单，F11 全屏，关窗即断开。"),
        Ok(Err(e)) => { drop(cmd_tx); let _ = worker.join(); return Err(e); }
        Err(_) => { drop(cmd_tx); let _ = worker.join(); return Err("连接超时（40 秒）".into()); }
    }

    'main: loop {
        for event in event_pump.poll_iter() {
            let (_, wh0) = canvas.window().size();
            let wh0 = wh0 as i32;
            match event {
                Event::Quit { .. } => break 'main,
                Event::Window { win_event: WindowEvent::Close, .. } => break 'main,
                Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::F1), repeat: false, .. } => { ui.toolbar_open = !ui.toolbar_open; ui.panel = None; }
                Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::F11), repeat: false, .. } => { ui.fullscreen = !ui.fullscreen; canvas.window_mut().set_fullscreen(if ui.fullscreen { FullscreenType::Desktop } else { FullscreenType::Off }).ok(); }
                Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::Escape), repeat: false, .. } if ui.panel.is_some() => { ui.panel = None; }
                Event::KeyDown { scancode: Some(sc), repeat: false, .. } if ui.panel.is_none() && !ui.view_only && ui.mouse_mode != 2 => {
                    if let Some(vk) = linux_uuremote::viewer::input::windows_vk(sc as i32) { let _ = cmd_tx.send(Cmd::Key { vk, pressed: true }); }
                }
                Event::KeyUp { scancode: Some(sc), .. } if ui.panel.is_none() && !ui.view_only && ui.mouse_mode != 2 => {
                    if let Some(vk) = linux_uuremote::viewer::input::windows_vk(sc as i32) { let _ = cmd_tx.send(Cmd::Key { vk, pressed: false }); }
                }
                Event::MouseMotion { x, y, xrel, yrel, .. } => {
                    mouse_at = Point::new(x, y);
                    if y >= wh0 - 70 { ui.last_near_bottom = Instant::now(); }
                    if ui.mouse_mode == 3 {
                        // 游戏鼠标：relative 模式采到的增量按 Windows notch 语义原样投出。
                        if !ui.view_only && (xrel != 0 || yrel != 0) { let _ = cmd_tx.send(Cmd::MouseRelative { dx: xrel, dy: yrel }); }
                    } else if ui.panel.is_none() && !ui.view_only && ui.mouse_mode != 2 && frame_size.0 > 0 && !(ui.toolbar_open && y >= wh0 - 70) {
                        let vp = linux_uuremote::viewer::input::Viewport::fit(canvas.window().size(), (frame_size.0.max(1) as u32, frame_size.1.max(1) as u32));
                        if vp.contains(x as f64, y as f64) {
                            if let Some(m) = vp.motion(x as f64, y as f64) {
                                let _ = cmd_tx.send(Cmd::MouseMove { fx: m["abs_x"].as_f64().unwrap_or(0.), fy: m["abs_y"].as_f64().unwrap_or(0.) });
                            }
                        }
                    }
                }
                Event::MouseButtonDown { mouse_btn, x, y, .. } => {
                    mouse_at = Point::new(x, y);
                    if ui.hit_menu(x, y, wh0) {
                        if let Some(action) = ui.pick(x, y, wh0) {
                            match action {
                                Action::Quality(q) => {
                                    ui.quality_label = quality_name(q).into();
                                    if ui.chroma_label == "真彩" && q == 4 { ui.chroma_label = "常规色彩".into(); let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_chroma(seq, rpc_id, 1))); seq += 1; rpc_id += 1; ui.notify("真彩模式下不支持原画画质，已切回常规色彩"); }
                                    let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_quality(seq, rpc_id, q))); seq += 1; rpc_id += 1;
                                }
                                Action::Fps(i) => { ui.fps_label = format!("{} 帧", [30u32, 60, 90, 144][(i as usize - 1).min(3)]); let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_fps(seq, rpc_id, i))); seq += 1; rpc_id += 1; }
                                Action::Resolution(s) => { ui.res_label = match s { None => "跟随被控端".into(), Some((w, h)) => format!("{w}x{h}") }; let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_resolution(seq, rpc_id, s))); seq += 1; rpc_id += 1; }
                                Action::Chroma(c) => {
                                    if c == 3 && ui.quality_label == "原画" { ui.quality_label = "超清".into(); let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_quality(seq, rpc_id, 3))); seq += 1; rpc_id += 1; ui.notify("真彩模式下不支持当前画质，已降到超清"); }
                                    ui.chroma_label = if c == 3 { "真彩".into() } else { "常规色彩".into() };
                                    let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_chroma(seq, rpc_id, c))); seq += 1; rpc_id += 1;
                                }
                                Action::MouseMode(m) => {
                                    ui.mouse_mode = m;
                                    mouse_sys.set_relative_mouse_mode(m == 3);
                                    ui.notify(match m { 1 => "已开启仿真鼠标", 2 => "正在使用被控端鼠标，F1 可解锁", 3 => "已开启游戏鼠标模式", _ => "已切回智能鼠标" });
                                }
                                Action::Sound => { let on = !stats.audio_enabled.load(Ordering::Relaxed); stats.audio_enabled.store(on, Ordering::Relaxed); ui.sound_on = on; }
                                Action::ViewOnly => { ui.view_only = !ui.view_only; ui.notify(if ui.view_only { "当前为观看模式" } else { "已切回操控模式" }); }
                                Action::Fullscreen => { ui.fullscreen = !ui.fullscreen; canvas.window_mut().set_fullscreen(if ui.fullscreen { FullscreenType::Desktop } else { FullscreenType::Off }).ok(); }
                                Action::ToggleToolbar => { ui.toolbar_open = false; }
                                Action::Quit => break 'main,
                            }
                        }
                    } else if !ui.view_only && frame_size.0 > 0 {
                        if let Some(button) = map_button(mouse_btn) { let _ = cmd_tx.send(Cmd::MouseButton { button, pressed: true }); }
                    }
                }
                Event::MouseButtonUp { mouse_btn, .. } => {
                    if !ui.view_only && frame_size.0 > 0 {
                        if let Some(button) = map_button(mouse_btn) { let _ = cmd_tx.send(Cmd::MouseButton { button, pressed: false }); }
                    }
                }
                Event::MouseWheel { x, y, .. } if !ui.view_only && ui.mouse_mode != 2 => {
                    let _ = cmd_tx.send(Cmd::Scroll { dx: x * 120, dy: y * 120 });
                }
                _ => {}
            }
        }

        if let Ok(result) = conn_rx.try_recv() {
            if let Err(e) = result { err = Some(e); }
            break 'main;
        }

        // 官方悬浮球行为：鼠标离开底部 3 秒后自动收起工具栏（下拉开着不收）。
        if ui.toolbar_open && ui.panel.is_none() && ui.last_near_bottom.elapsed() > Duration::from_secs(3) {
            ui.toolbar_open = false;
        }

        let mut latest: Option<Frame> = None;
        while let Ok(frame) = frame_rx.try_recv() { latest = Some(frame); }
        if let Some(frame) = latest {
            if frame_size != (frame.width, frame.height) {
                frame_size = (frame.width, frame.height);
                video_tex = Some(texture_creator.create_texture(PixelFormatEnum::YV12, sdl2::render::TextureAccess::Streaming, frame.width as u32, frame.height as u32).expect("纹理创建失败"));
                if !resized {
                    resized = true;
                    if let Ok(bounds) = video.display_bounds(0) {
                        let w = frame.width.min(bounds.width() as usize);
                        let h = frame.height.min(bounds.height() as usize);
                        canvas.window_mut().set_size(w as u32, h as u32).ok();
                    }
                }
            }
            if let Some(tex) = &mut video_tex {
                tex.update_yuv(None, &frame.y, frame.ystride.max(0) as usize, &frame.u, frame.ustride.max(0) as usize, &frame.v, frame.vstride.max(0) as usize).ok();
            }
        }
        while let Ok(cursor) = cursor_rx.try_recv() {
            match texture_creator.create_texture(PixelFormatEnum::RGBA32, sdl2::render::TextureAccess::Streaming, cursor.width, cursor.height) {
                Ok(mut tex) => {
                    tex.set_blend_mode(sdl2::render::BlendMode::Blend);
                    let pitch = cursor.width as usize * 4;
                    let rgba = cursor.rgba.clone();
                    tex.with_lock(None, |buffer, bpitch| {
                        for row in 0..cursor.height as usize {
                            buffer[row * bpitch..row * bpitch + pitch].copy_from_slice(&rgba[row * pitch..row * pitch + pitch]);
                        }
                        Ok::<(), String>(())
                    })
                    .ok();
                    cursor_tex = Some((tex, cursor.hotspot_x, cursor.hotspot_y));
                }
                Err(_) => {}
            }
        }

        if let Some(q) = &audio_queue {
            while Instant::now() >= audio_next {
                audio_next += Duration::from_millis(10);
                let mut chunk = vec![0.0f32; 960];
                {
                    let mut ring = audio_ring.lock().unwrap_or_else(|e| e.into_inner());
                    let mut i = 0usize;
                    while i < chunk.len() {
                        match ring.pop_front() { Some(v) => { chunk[i] = v; i += 1; } None => break }
                    }
                }
                if stats.audio_enabled.load(Ordering::Relaxed) { let _ = q.queue_audio(&chunk); }
            }
            if audio_next < Instant::now() - Duration::from_millis(100) { audio_next = Instant::now(); }
        }

        let (ww, wh) = canvas.window().size();
        let vp = linux_uuremote::viewer::input::Viewport::fit((ww, wh), (frame_size.0.max(1) as u32, frame_size.1.max(1) as u32));
        canvas.set_draw_color(Color::RGB(10, 10, 12));
        canvas.clear();
        if let Some(tex) = &video_tex {
            canvas.copy(tex, None, Rect::new(vp.x as i32, vp.y as i32, vp.width as u32, vp.height as u32)).ok();
        }
        ui.draw(&mut canvas, &texture_creator, font, ww, wh);
        let near_bottom = (mouse_at.y() as i64) >= wh as i64 - 70;
        mouse_sys.show_cursor(ui.toolbar_open || ui.panel.is_some() || ui.view_only || ui.mouse_mode == 3 || near_bottom);
        if !ui.toolbar_open && ui.panel.is_none() && !ui.view_only && ui.mouse_mode != 3 && !near_bottom {
            if let Some((tex, hx, hy)) = &cursor_tex {
                let scale = vp.width / frame_size.0.max(1) as f64;
                let x = mouse_at.x as f64 - *hx as f64 * scale;
                let y = mouse_at.y as f64 - *hy as f64 * scale;
                let q = tex.query();
                canvas.copy(tex, None, Rect::new(x as i32, y as i32, (q.width as f64 * scale) as u32, (q.height as f64 * scale) as u32)).ok();
            }
        }
        if stat_since.elapsed() >= Duration::from_millis(500) {
            let frames = stats.frames.load(Ordering::Relaxed);
            let bytes = stats.video_bytes.load(Ordering::Relaxed);
            let secs = stat_since.elapsed().as_secs_f64().max(0.2);
            let fps_now = (frames - last_frames) as f64 / secs;
            let kbps = (bytes - last_bytes) as f64 * 8.0 / secs / 1_000.0;
            last_frames = frames;
            last_bytes = bytes;
            stat_since = Instant::now();
            let frm = stats.last_gap_ms.load(Ordering::Relaxed);
            let ping = stats.rtt_ms.load(Ordering::Relaxed);
            let rw = stats.remote_w.load(Ordering::Relaxed);
            let rh = stats.remote_h.load(Ordering::Relaxed);
            let chroma = stats.chroma.load(Ordering::Relaxed);
            ui.hud = vec![
                format!("帧率：{fps_now:.0} fps    带宽占用：{kbps:.0} Kbps"),
                format!("frm：{frm} ms    延迟：{ping} ms    {rw}x{rh}{}", if chroma == 3 { "  真彩" } else { "" }),
            ];
            // 官方「检测到网络延迟高」提示（30s 内只提一次，不打断操作）。
            if ping > 120 && last_lag_toast.elapsed() > Duration::from_secs(30) {
                last_lag_toast = Instant::now();
                ui.notify("检测到网络延迟高，请尝试降低画质或帧率，以改善远程控制体验");
            }
        }
        canvas.present();
        std::thread::sleep(Duration::from_millis(8));
    }

    drop(cmd_tx);
    let _ = worker.join();
    match err { Some(e) => Err(e), None => Ok(()) }
}

// ---------------- 共用小件 ----------------

/// --ui-preview：合成帧 + 新 UI，12 秒后自动退出（自测渲染，不需要云端/被控端）。
fn run_ui_preview() {
    let sdl = sdl2::init().expect("SDL 初始化失败");
    let video = sdl.video().expect("视频子系统失败");
    let font = load_font();
    let mut canvas = video.window("UU 远程 · UI 预览", 1280, 720).position_centered().resizable().build().expect("窗口创建失败").into_canvas().build().expect("渲染器创建失败");
    set_app_icon(canvas.window_mut());
    let creator = canvas.texture_creator();
    let mut ui = Ui::new(4, 90, false);
    ui.notify("真彩模式下不支持当前画质，已降到超清");
    ui.hud = vec!["帧率：88 fps    带宽占用：2010 Kbps".into(), "frm：6 ms    延迟：12 ms    2560x1440".into()];
    let mut tex = creator.create_texture(PixelFormatEnum::YV12, sdl2::render::TextureAccess::Streaming, 1280, 720).expect("纹理失败");
    let w = 1280usize;
    let h = 720usize;
    let mut y = vec![16u8; w * h];
    let u = vec![128u8; w * h / 4];
    let v = vec![128u8; w * h / 4];
    let start = Instant::now();
    let mut tick = 0usize;
    let mut pump = sdl.event_pump().expect("事件泵");
    loop {
        for event in pump.poll_iter() {
            if matches!(event, Event::Quit { .. } | Event::Window { win_event: WindowEvent::Close, .. }) { return; }
        }
        if start.elapsed() > Duration::from_secs(12) { return; }
        ui.last_near_bottom = Instant::now();
        if start.elapsed() > Duration::from_secs(7) { ui.toolbar_open = false; } // 验证收起态悬浮球
        // 移动亮块当内容，模拟画面在动。
        let bx = (tick * 7) % (w - 200);
        for row in 200..360usize {
            for col in bx..bx + 200 {
                y[row * w + col] = 235;
            }
        }
        tick += 1;
        tex.update_yuv(None, &y, w, &u, w / 2, &v, w / 2).ok();
        let (ww, wh) = canvas.window().size();
        canvas.set_draw_color(Color::RGB(10, 10, 12));
        canvas.clear();
        canvas.copy(&tex, None, Rect::new(0, 0, w as u32, h as u32)).ok();
        ui.draw(&mut canvas, &creator, font.as_ref(), ww, wh);
        canvas.present();
        std::thread::sleep(Duration::from_millis(33));
    }
}

/// --home-preview：合成设备列表，渲染主页若干帧后抓一张 RGBA 快照（验证顶栏按钮组排版）。
fn run_home_preview(snap: Option<&str>) {
    force_argb();
    let sdl = sdl2::init().expect("SDL 初始化失败");
    let video = sdl.video().expect("视频子系统失败");
    let fonts = load_fonts();
    let mut canvas = video.window("UU 远程 · 主页预览", WIN_W, WIN_H).position_centered().borderless().build().expect("窗口创建失败").into_canvas().build().expect("渲染器创建失败");
    set_app_icon(canvas.window_mut());
    let creator = canvas.texture_creator();
    let mut home = Home::new(true);
    home.devices = vec![
        DeviceRow { id: "d1".into(), alias: "Dev-Box".into(), online: true, controllable: true },
        DeviceRow { id: "d2".into(), alias: "Backup".into(), online: false, controllable: true },
    ];
    home.selected = Some(1);
    home.mouse = (CARD_W - 20, 20); // 悬停模拟（卡片坐标）
    let mut text_cache: HashMap<String, Texture> = HashMap::new();
    let shadow_tex = shadow_texture(&creator);
    let corner_tl = corner_texture(&creator, SIDE_BG, true, true);
    let corner_tr = corner_texture(&creator, MAIN_BG, false, true);
    let corner_bl = corner_texture(&creator, SIDE_BG, true, false);
    let corner_br = corner_texture(&creator, MAIN_BG, false, false);
    let corners = [&corner_tl, &corner_tr, &corner_bl, &corner_br];
    for _ in 0..8 {
        let (hx, hy) = home.mouse;
        draw_home(&mut canvas, &creator, &fonts, &mut home, &mut text_cache, None, &shadow_tex, corners, hx, hy);
        canvas.present();
        std::thread::sleep(Duration::from_millis(30));
    }
    if let Some(path) = snap {
        let (w, h) = canvas.window().size();
        if let Ok(surf) = canvas.read_pixels(Rect::new(0, 0, w, h), PixelFormatEnum::RGBA32) {
            std::fs::write(path, surf).ok();
        }
    }
}

fn clone_creds(c: &Credentials) -> Credentials {
    Credentials { user_id: c.user_id.clone(), device_id: c.device_id.clone(), client_id: c.client_id.clone(), system_id: c.system_id.clone(), token: c.token.clone(), csrf: c.csrf.clone(), sign_key: c.sign_key.clone() }
}

async fn viewer_identity(api: &ApiClient) -> Result<(String, String, String), String> {
    let path = std::path::Path::new(&std::env::var("HOME").unwrap_or_default()).join(".config/uuremote/viewer-identity.json");
    if let Ok(txt) = std::fs::read_to_string(&path) {
        if let Ok(v) = serde_json::from_str::<Value>(&txt) {
            let get = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
            if let (Some(d), Some(c), Some(s)) = (get("device_id"), get("client_id"), get("system_id")) {
                if !d.is_empty() { return Ok((d, c, s)); }
            }
        }
    }
    let cid = wire::uuid();
    let sid = wire::uuid().to_uppercase();
    let guid = wire::uuid().to_uppercase();
    let did = linux_uuremote::api::register_device_with(api, "1920x1080", Some(&cid), Some(&sid), Some(&guid)).await?;
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    let _ = std::fs::write(&path, serde_json::json!({"device_id": did, "client_id": cid, "system_id": sid, "machine_guid": guid}).to_string());
    Ok((did, cid, sid))
}

async fn room_or_die(creds: Credentials, target: &str) -> linux_uuremote::viewer::RoomData {
    let api = ApiClient::new(creds);
    match join(&api, target).await {
        Ok(room) => room,
        Err(e) => { eprintln!("   join 失败：{e}"); std::process::exit(1); }
    }
}

fn map_button(button: MouseButton) -> Option<i32> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Right => Some(2),
        MouseButton::Middle => Some(4),
        _ => None,
    }
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split('x');
    let w = parts.next()?.parse::<u32>().ok()?;
    let h = parts.next()?.parse::<u32>().ok()?;
    if (320..=7680).contains(&w) && (240..=4320).contains(&h) { Some((w, h)) } else { None }
}

fn load_font() -> Option<sdl2::ttf::Font<'static, 'static>> {
    let ctx = Box::leak(Box::new(sdl2::ttf::init().ok()?));
    let candidates: [(&str, Option<u32>); 3] = [
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", Some(0)),
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-DemiLight.ttc", Some(0)),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", None),
    ];
    for (path, index) in candidates {
        let font = match index {
            Some(i) => ctx.load_font_at_index(path, i, 16),
            None => ctx.load_font(path, 16),
        };
        if let Ok(font) = font { return Some(font); }
    }
    None
}

fn text_texture<'a>(creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, text: &str, color: Color) -> Option<Texture<'a>> {
    let font = font?;
    let surface = font.render(text).blended(color).ok()?;
    creator.create_texture_from_surface(&surface).ok()
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Quality(u64),
    Fps(u64),
    Resolution(Option<(u32, u32)>),
    Chroma(u64),
    MouseMode(i32),
    Sound,
    ViewOnly,
    Fullscreen,
    ToggleToolbar,
    Quit,
}

/// 官方 Windows 控制端底部悬浮工具栏 + 左上 HUD + 下拉菜单 + 降档提示（Toast）。
/// 词表全部取自 GameViewer.exe 内嵌文案；图标取自同 exe 提取的内嵌 SVG（viewer::icons）。
/// 鼠标模式四档（智能/仿真/被控端/游戏）与真彩互斥规则按官方菜单还原。
struct Ui {
    toolbar_open: bool,
    panel: Option<usize>,
    fullscreen: bool,
    sound_on: bool,
    view_only: bool,
    mouse_mode: i32,
    quality_label: String,
    fps_label: String,
    res_label: String,
    chroma_label: String,
    toast: Option<(String, Instant)>,
    rects: Vec<Rect>,
    option_rects: Vec<(Rect, Action)>,
    hud: Vec<String>,
    /// 官方悬浮球行为：鼠标 3 秒不碰底部区域，工具栏自动收起（点悬浮球或 F1 恢复）。
    last_near_bottom: Instant,
}

const MOUSE_MODES: [&str; 4] = ["智能鼠标", "仿真鼠标", "使用被控端鼠标", "游戏鼠标"];

impl Ui {
    fn new(quality: u64, fps: u32, view_only: bool) -> Self {
        Self {
            toolbar_open: true,
            panel: None,
            fullscreen: false,
            sound_on: true,
            view_only,
            mouse_mode: 0,
            quality_label: quality_name(quality).to_owned(),
            fps_label: format!("{fps} 帧"),
            res_label: "跟随被控端".to_owned(),
            chroma_label: "常规色彩".to_owned(),
            toast: None,
            rects: Vec::new(),
            option_rects: Vec::new(),
            hud: Vec::new(),
            last_near_bottom: Instant::now(),
        }
    }
    fn notify(&mut self, msg: &str) {
        self.toast = Some((msg.to_owned(), Instant::now()));
    }
    /// 工具栏占用高度（收起时只剩底部居中的悬浮球，占用 26px）。
    fn bar_h(&self) -> i32 {
        if self.toolbar_open { 44 } else { 26 }
    }
    /// 命中判定：底部悬浮条 / 收起时的悬浮球 / 下拉面板选项区。
    fn hit_menu(&self, x: i32, y: i32, wh: i32) -> bool {
        let h = self.bar_h();
        if y >= wh - h - 6 { return true; }
        self.option_rects.iter().any(|(r, _)| r.contains_point(Point::new(x, y)))
    }
    fn pick(&mut self, x: i32, y: i32, wh: i32) -> Option<Action> {
        let h = self.bar_h();
        if y >= wh - h - 6 {
            if !self.toolbar_open {
                self.toolbar_open = true;
                self.panel = None;
                return None;
            }
            if let Some(i) = self.rects.iter().position(|r| r.contains_point(Point::new(x, y))) {
                self.panel = if self.panel == Some(i) { None } else { Some(i) };
            } else {
                self.panel = None;
            }
            return None;
        }
        let action = self.option_rects.iter().find(|(r, _)| r.contains_point(Point::new(x, y))).map(|(_, a)| *a);
        if action.is_some() { self.panel = None; }
        action
    }
    fn options(&self, i: usize) -> Vec<(String, Action)> {
        match i {
            0 => vec![("自动".into(), Action::Quality(5)), ("原画 30M".into(), Action::Quality(4)), ("超清 14M".into(), Action::Quality(3)), ("高清 8M".into(), Action::Quality(2)), ("流畅".into(), Action::Quality(1))],
            1 => vec![("真彩".into(), Action::Chroma(3)), ("常规色彩".into(), Action::Chroma(1))],
            2 => vec![("帧率上限 30".into(), Action::Fps(1)), ("帧率上限 60".into(), Action::Fps(2)), ("帧率上限 90".into(), Action::Fps(3)), ("帧率上限 144".into(), Action::Fps(4))],
            3 => vec![("跟随被控端".into(), Action::Resolution(None)), ("2K 2560×1440".into(), Action::Resolution(Some((2560, 1440)))), ("1080P".into(), Action::Resolution(Some((1920, 1080)))), ("720P".into(), Action::Resolution(Some((1280, 720))))],
            4 => MOUSE_MODES.iter().enumerate().map(|(m, s)| (s.to_string(), Action::MouseMode(m as i32))).collect(),
            5 => vec![("开".into(), Action::Sound), ("关".into(), Action::Sound)],
            6 => vec![("启用观看模式".into(), Action::ViewOnly)],
            7 => vec![("全屏/退出全屏".into(), Action::Fullscreen)],
            8 => vec![("隐藏工具栏".into(), Action::ToggleToolbar)],
            _ => vec![("断开连接".into(), Action::Quit)],
        }
    }
    /// 工具栏图标槽位（与 buttons 一一对应）。
    fn icon_for(i: usize) -> &'static [u8] {
        match i {
            0 => icons::MONITOR,     // 画质
            1 => icons::GEAR,        // 色彩
            2 => icons::SPEED,       // 帧率
            3 => icons::MONITOR,     // 分辨率
            4 => icons::MOUSE,       // 鼠标模式
            5 => icons::SPEAKER,     // 声音
            6 => icons::EYE,         // 观看模式
            7 => icons::EXPAND,      // 全屏
            8 => icons::BALL,        // 悬浮球/工具栏
            _ => icons::CLOSE,       // 断开
        }
    }
    fn draw(&mut self, canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, width: u32, height: u32) {
        let h = self.bar_h();
        self.rects.clear();
        self.option_rects.clear();
        let by = height as i32 - h;
        if self.toolbar_open {
            // 官方悬浮条：深色圆角条居中（SDL 无圆角 API，用整条深色底 + 两侧渐隐近似）。
            canvas.set_draw_color(Color::RGBA(20, 22, 28, 235));
            canvas.fill_rect(Rect::new(0, by, width, h as u32)).ok();
            let buttons: Vec<String> = vec![
                format!("画质 {}", self.quality_label),
                self.chroma_label.clone(),
                self.fps_label.clone(),
                format!("分辨率 {}", self.res_label),
                MOUSE_MODES[self.mouse_mode as usize].to_owned(),
                if self.sound_on { "声音 开".into() } else { "声音 关".into() },
                if self.view_only { "观看模式".into() } else { "操控模式".into() },
                if self.fullscreen { "退出全屏".into() } else { "全屏".into() },
                "隐藏工具栏".into(),
                "断开".into(),
            ];
            let mut x = 8i32;
            for (i, label) in buttons.iter().enumerate() {
                let tex = text_texture(creator, font, label, Color::RGB(235, 235, 235));
                let tw = tex.as_ref().map(|t| t.query().width as i32).unwrap_or(70);
                let w = tw + 30;
                let r = Rect::new(x, by + 8, w as u32, (h - 16) as u32);
                if self.panel == Some(i) {
                    canvas.set_draw_color(Color::RGBA(80, 120, 220, 235));
                    canvas.fill_rect(r).ok();
                }
                if let Some(tex) = icon_texture(creator, Ui::icon_for(i)) { canvas.copy(&tex, None, Rect::new(x + 6, by + 14, 16, 16)).ok(); }
                if let Some(tex) = tex { canvas.copy(&tex, None, Rect::new(x + 26, by + 14, tex.query().width, tex.query().height)).ok(); }
                self.rects.push(r);
                x += w + 4;
            }
        } else {
            // 收起态：底部居中的悬浮球（官方默认形态），点它重新展开。
            let ball = Rect::new(width as i32 / 2 - 13, by, 26, 26);
            canvas.set_draw_color(Color::RGBA(20, 22, 28, 200));
            canvas.fill_rect(Rect::new(ball.x() - 6, ball.y() - 4, ball.width() + 12, ball.height() + 8)).ok();
            if let Some(tex) = icon_texture(creator, icons::BALL) { canvas.copy(&tex, None, Rect::new(ball.x(), ball.y(), 24, 24)).ok(); }
        }
        if let Some(i) = self.panel {
            let options = self.options(i);
            let bx = self.rects.get(i).map(|r| r.x()).unwrap_or(10).min(width as i32 - 200);
            let ph = (options.len() as u32) * 26 + 8;
            let py = height as i32 - h - ph as i32 - 2;
            canvas.set_draw_color(Color::RGBA(28, 30, 36, 246));
            canvas.fill_rect(Rect::new(bx, py, 200, ph)).ok();
            for (j, (label, action)) in options.iter().enumerate() {
                let r = Rect::new(bx + 4, py + 4 + (j as i32) * 26, 192, 24);
                canvas.set_draw_color(Color::RGBA(44, 48, 58, 246));
                canvas.fill_rect(r).ok();
                if let Some(tex) = text_texture(creator, font, label, Color::RGB(235, 235, 235)) {
                    canvas.copy(&tex, None, Rect::new(r.x() + 10, r.y() + 4, tex.query().width, tex.query().height)).ok();
                }
                self.option_rects.push((r, *action));
            }
        }
        // 左上 HUD：官方「帧率：/带宽占用：」两行 + frm/延迟。
        if !self.hud.is_empty() {
            let mut y = 8i32;
            for line in &self.hud {
                if let Some(tex) = text_texture(creator, font, line, Color::RGB(235, 235, 235)) {
                    let q = tex.query();
                    canvas.set_draw_color(Color::RGBA(15, 15, 18, 170));
                    canvas.fill_rect(Rect::new(6, y - 4, q.width + 12, q.height + 8)).ok();
                    canvas.copy(&tex, None, Rect::new(12, y, q.width, q.height)).ok();
                    y += q.height as i32 + 4;
                }
            }
        }
        // Toast（降档/延迟高/观看模式提示）：居中偏上，4 秒消失。
        if let Some((msg, since)) = self.toast.clone() {
            if since.elapsed() < Duration::from_secs(4) {
                if let Some(tex) = text_texture(creator, font, &msg, Color::RGB(255, 245, 230)) {
                    let q = tex.query();
                    let tx = (width as i32 - q.width as i32) / 2;
                    canvas.set_draw_color(Color::RGBA(60, 55, 45, 220));
                    canvas.fill_rect(Rect::new(tx - 14, 46, q.width + 28, q.height + 16)).ok();
                    canvas.copy(&tex, None, Rect::new(tx, 54, q.width, q.height)).ok();
                }
            } else {
                self.toast = None;
            }
        }
    }
}

fn quality_name(q: u64) -> &'static str {
    match q {
        5 => "自动",
        4 => "原画",
        3 => "超清",
        2 => "高清",
        1 => "流畅",
        _ => "自动",
    }
}

/// 把内嵌的官方图标 PNG（白色 alpha）解成 SDL 纹理；每次新建（图标小、无缓存必要）。
fn icon_texture<'a>(creator: &'a TextureCreator<WindowContext>, png: &[u8]) -> Option<Texture<'a>> {
    let img = linux_uuremote::viewer::png::decode(png).ok()?;
    let mut tex = creator.create_texture(PixelFormatEnum::RGBA32, sdl2::render::TextureAccess::Streaming, img.width, img.height).ok()?;
    // SDL 纹理默认不混合 alpha——不打开 Blend 的话透明像素按不透明黑渲染，图标就是一块黑底。
    tex.set_blend_mode(sdl2::render::BlendMode::Blend);
    let pitch = img.width as usize * 4;
    let rgba = img.rgba;
    tex.with_lock(None, |buf, bp| {
        for row in 0..img.height as usize {
            buf[row * bp..row * bp + pitch].copy_from_slice(&rgba[row * pitch..row * pitch + pitch]);
        }
        Ok::<(), String>(())
    })
    .ok()?;
    Some(tex)
}
