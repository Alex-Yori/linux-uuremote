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
use linux_uuremote::viewer::{join, Cmd, Session};

use sdl2::event::{Event, WindowEvent};
use sdl2::mouse::MouseButton;
use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::{Point, Rect};
use sdl2::render::{Texture, TextureCreator};
use sdl2::video::{FullscreenType, WindowContext};
use sdl2::Sdl;
use serde_json::{json, Value};

// ---- 原版配色 ----
const UI_BLUE: Color = Color::RGB(31, 111, 229);
const GREEN: Color = Color::RGB(52, 192, 100);
const SIDE_BG: Color = Color::RGB(245, 246, 248);
const MAIN_BG: Color = Color::RGB(252, 253, 254);
const ROW_HOVER: Color = Color::RGB(235, 238, 242);
const TEXT_DARK: Color = Color::RGB(48, 52, 58);
const TEXT_MUT: Color = Color::RGB(140, 145, 152);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let flag = |name: &str| args.iter().any(|a| a == name);

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

    let sdl = sdl2::init().expect("SDL 初始化失败");
    let video = sdl.video().expect("视频子系统失败");
    let font = load_font();
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
        match run_session(&sdl, &video, font.as_ref(), &creds, &target, "", quality, fps, win_w, win_h, view_only, no_audio, &mut pump) {
            Ok(()) => println!("已断开。"),
            Err(e) => { eprintln!("{e}"); std::process::exit(1); }
        }
        return;
    }

    // ---- 主页 ----
    let mut canvas = video.window("网易UU远程", 980, 640).position_centered().resizable().build().expect("窗口创建失败").into_canvas().build().expect("渲染器创建失败");
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

    let mut event_pump = sdl.event_pump().expect("事件泵");
    'home: loop {
        while let Some(event) = event_pump.poll_event() {
            match event {
                Event::Quit { .. } => break 'home,
                Event::Window { win_event: WindowEvent::Close, .. } => break 'home,
                Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, .. } => {
                    match home.hit(x, y) {
                        HomeAction::None => {}
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
                                        let res = run_session(&sdl, &video, font.as_ref(), &creds, &id, &alias, quality, fps, win_w, win_h, view_only_click || view_only, no_audio, &mut event_pump);
                                        canvas.window_mut().show();
                                        home.session_busy = false;
                                        home.status_msg = match res { Ok(()) => "已断开。".to_owned(), Err(e) => format!("会话结束：{e}") };
                                    }
                                    None => home.status_msg = "还没有登录态，先点「立即登录」。".into(),
                                }
                            }
                        }
                        HomeAction::Stub(label) => home.status_msg = format!("{label}（暂未实现）"),
                    }
                }
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

        draw_home(&mut canvas, &creator, font.as_ref(), &mut home, &mut text_cache, qr_tex.as_ref());
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
enum HomeAction { None, Select(usize), Login, Enter(bool), Stub(String) }

struct Home {
    devices: Vec<DeviceRow>,
    selected: Option<usize>,
    logged_in: bool,
    login_open: bool,
    login_status: String,
    status_msg: String,
    session_busy: bool,
    hit_map: Vec<(Rect, HomeAction)>,
}
impl Home {
    fn new(logged_in: bool) -> Self {
        Self { devices: Vec::new(), selected: None, logged_in, login_open: false, login_status: String::new(), status_msg: String::new(), session_busy: false, hit_map: Vec::new() }
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

/// 在线圆点（没有圆形 API，三行横条拼一个近似的圆）。
fn dot(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, x: i32, y: i32, c: Color) {
    canvas.set_draw_color(c);
    canvas.fill_rect(Rect::new(x - 2, y - 3, 5, 2)).ok();
    canvas.fill_rect(Rect::new(x - 3, y - 1, 7, 2)).ok();
    canvas.fill_rect(Rect::new(x - 2, y + 1, 5, 2)).ok();
}

fn draw_home<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, home: &mut Home, cache: &mut HashMap<String, Texture<'a>>, qr: Option<&Texture<'a>>) {
    let (ww, wh) = canvas.window().size();
    canvas.set_draw_color(MAIN_BG);
    canvas.clear();
    home.hit_map.clear();

    // 左侧栏。
    canvas.set_draw_color(SIDE_BG);
    canvas.fill_rect(Rect::new(0, 0, 200, wh)).ok();
    text(canvas, creator, font, cache, "网易UU远程", 16, 12, UI_BLUE);
    let mut y = 46i32;
    canvas.set_draw_color(UI_BLUE);
    canvas.fill_rect(Rect::new(4, y + 1, 3, 16)).ok();
    text(canvas, creator, font, cache, "我的设备", 12, y + 2, TEXT_DARK);
    y += 30;
    for (i, d) in home.devices.iter().enumerate() {
        let sel = home.selected == Some(i);
        let r = Rect::new(6, y, 188, 34);
        if sel {
            canvas.set_draw_color(ROW_HOVER);
            canvas.fill_rect(r).ok();
            canvas.set_draw_color(UI_BLUE);
            canvas.fill_rect(Rect::new(6, y + 6, 3, 22)).ok();
        }
        dot(canvas, 30, y + 17, if d.online { GREEN } else { Color::RGB(185, 188, 192) });
        text(canvas, creator, font, cache, &d.alias, 40, y + 8, TEXT_DARK);
        home.hit_map.push((r, HomeAction::Select(i)));
        y += 36;
    }
    if home.devices.is_empty() { text(canvas, creator, font, cache, if home.logged_in { "（无设备）" } else { "（未登录）" }, 30, y + 6, TEXT_MUT); y += 30; }
    y += 6;
    let r_all = Rect::new(6, y, 188, 34);
    text(canvas, creator, font, cache, "全部设备", 30, y + 8, TEXT_MUT);
    home.hit_map.push((r_all, HomeAction::Stub("全部设备".into())));
    y += 40;
    let r_help = Rect::new(6, y, 188, 34);
    text(canvas, creator, font, cache, "远程协助", 30, y + 8, TEXT_MUT);
    home.hit_map.push((r_help, HomeAction::Stub("远程协助".into())));
    let r_set = Rect::new(6, wh as i32 - 40, 188, 34);
    text(canvas, creator, font, cache, "设置", 30, wh as i32 - 32, TEXT_MUT);
    home.hit_map.push((r_set, HomeAction::Stub("设置".into())));

    // 主区。
    let mx = 224i32;
    if !home.logged_in {
        text(canvas, creator, font, cache, "全部设备", mx, 18, TEXT_DARK);
        text(canvas, creator, font, cache, "随时访问个人设备，可用于办公、游戏、文件传输。", mx, 56, TEXT_MUT);
        if !home.login_open {
            let r = Rect::new(mx + 60, 220, 200, 46);
            canvas.set_draw_color(UI_BLUE);
            canvas.fill_rect(r).ok();
            text_center(canvas, creator, font, cache, "立即登录", r, Color::RGB(255, 255, 255));
            home.hit_map.push((r, HomeAction::Login));
        } else {
            draw_login_panel(canvas, creator, font, cache, home, qr, mx, 64);
        }
    } else if home.session_busy {
        text(canvas, creator, font, cache, "远控会话进行中……", mx, 120, TEXT_MUT);
        text(canvas, creator, font, cache, "请在远程窗口断开后再连接其他设备。", mx, 150, TEXT_MUT);
    } else if let Some((alias, online, controllable)) = home.selected_device().map(|d| (d.alias.clone(), d.online, d.controllable)) {
        dot(canvas, mx + 8, 30, if online { GREEN } else { Color::RGB(185, 188, 192) });
        text(canvas, creator, font, cache, if online { "在线" } else { "离线" }, mx + 20, 22, if online { GREEN } else { TEXT_MUT });
        text(canvas, creator, font, cache, &alias, mx + 62, 22, TEXT_DARK);

        // 蓝色大卡（进入桌面）。
        let card = Rect::new(mx, 64, (ww as i32 - mx - 40).max(320) as u32, 300);
        for k in 0..30u32 {
            let t = k as f64 / 29.0;
            let c = if online {
                Color::RGB(lerp(31, 78, t) as u8, lerp(111, 165, t) as u8, lerp(229, 245, t) as u8)
            } else {
                Color::RGB(lerp(150, 185, t) as u8, lerp(156, 190, t) as u8, lerp(166, 198, t) as u8)
            };
            canvas.set_draw_color(c);
            let yy = card.y() + (k as i32 * card.height() as i32 / 30);
            canvas.fill_rect(Rect::new(card.x(), yy, card.width(), card.height() / 30 + 1)).ok();
        }
        if online && controllable {
            let r = Rect::new(card.x() + card.width() as i32 / 2 - 90, card.y() + card.height() as i32 / 2 - 24, 180, 48);
            canvas.set_draw_color(Color::RGB(255, 255, 255));
            canvas.fill_rect(r).ok();
            text_center(canvas, creator, font, cache, "进入桌面 →", r, UI_BLUE);
            home.hit_map.push((r, HomeAction::Enter(false)));
        } else if online {
            text_center(canvas, creator, font, cache, "该设备不可远控", card, Color::RGB(255, 255, 255));
        } else {
            text_center(canvas, creator, font, cache, "设备离线", card, Color::RGB(255, 255, 255));
        }

        // 功能按钮行。
        let by = card.y() + card.height() as i32 + 14;
        let feats: Vec<(String, HomeAction)> = vec![
            ("文件传输".into(), HomeAction::Stub("文件传输".into())),
            ("观看模式".into(), HomeAction::Enter(true)),
            ("终端".into(), HomeAction::Stub("终端".into())),
            ("端口映射".into(), HomeAction::Stub("端口映射".into())),
        ];
        for (i, (label, action)) in feats.into_iter().enumerate() {
            let r = Rect::new(mx + (i as i32) * 138, by, 130, 40);
            let enabled = !matches!(action, HomeAction::Enter(_)) || (online && controllable);
            canvas.set_draw_color(if enabled { ROW_HOVER } else { Color::RGB(243, 244, 246) });
            canvas.fill_rect(r).ok();
            text_center(canvas, creator, font, cache, &label, r, if enabled { TEXT_DARK } else { TEXT_MUT });
            if enabled { home.hit_map.push((r, action)); }
        }

        // 快速启动。
        let qb = Rect::new(mx, by + 56, card.width(), 120);
        canvas.set_draw_color(Color::RGB(248, 249, 251));
        canvas.fill_rect(qb).ok();
        canvas.set_draw_color(Color::RGB(210, 214, 220));
        canvas.draw_rect(qb).ok();
        text_center(canvas, creator, font, cache, "+ 添加", qb, TEXT_MUT);
        home.hit_map.push((qb, HomeAction::Stub("快速启动".into())));
    } else {
        text(canvas, creator, font, cache, "选择左侧的设备后点「进入桌面」。", mx, 60, TEXT_MUT);
    }

    if !home.status_msg.is_empty() {
        text(canvas, creator, font, cache, &home.status_msg, 216, wh as i32 - 26, TEXT_MUT);
    }
}

fn draw_login_panel<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, cache: &mut HashMap<String, Texture<'a>>, home: &mut Home, qr: Option<&Texture<'a>>, mx: i32, my: i32) {
    let panel = Rect::new(mx, my, 520, 340);
    canvas.set_draw_color(Color::RGB(255, 255, 255));
    canvas.fill_rect(panel).ok();
    canvas.set_draw_color(Color::RGB(214, 218, 224));
    canvas.draw_rect(panel).ok();
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

fn lerp(a: i32, b: i32, t: f64) -> i32 { (a as f64 + (b - a) as f64 * t) as i32 }

/// 渲染文本缓存；缺字体时静默（形状仍然可见）。
fn text<'a>(canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, cache: &mut HashMap<String, Texture<'a>>, s: &str, x: i32, y: i32, color: Color) {
    let Some(font) = font else { return };
    let key = format!("{s}|{}|{}|{}", color.r, color.g, color.b);
    if !cache.contains_key(&key) {
        if let Some(tex) = font.render(s).solid(color).ok().and_then(|surf| creator.create_texture_from_surface(&surf).ok()) { cache.insert(key.clone(), tex); }
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
        if let Some(tex) = font.render(s).solid(color).ok().and_then(|surf| creator.create_texture_from_surface(&surf).ok()) { cache.insert(key.clone(), tex); }
    }
    if let Some(tex) = cache.get(&key) {
        let q = tex.query();
        canvas.copy(tex, None, Rect::new(r.x() + (r.width() - q.width) as i32 / 2, r.y() + (r.height() - q.height) as i32 / 2, q.width, q.height)).ok();
    }
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

fn run_session(sdl: &Sdl, video: &sdl2::VideoSubsystem, font: Option<&sdl2::ttf::Font<'static, 'static>>, creds: &Credentials, target: &str, alias: &str, quality: u64, fps: u32, win_w: u32, win_h: u32, view_only: bool, no_audio: bool, event_pump: &mut sdl2::EventPump) -> Result<(), String> {
    let title = if alias.is_empty() { "UU 远程".to_owned() } else { format!("UU 远程 · {alias}") };
    let mut canvas = video.window(&title, win_w, win_h).position_centered().resizable().build().map_err(|e| e.to_string())?.into_canvas().build().map_err(|e| e.to_string())?;
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
    let host = Arc::new(Host { stats: stats.clone(), frames: frame_tx, cursor: cursor_tx, audio: audio_ring.clone(), pings: Arc::new(Mutex::new(HashMap::new())) });

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

    let mut ui = Ui::new(quality, fps);
    let mut video_tex: Option<Texture> = None;
    let mut frame_size = (0usize, 0usize);
    let mut cursor_tex: Option<(Texture, i32, i32)> = None;
    let mut mouse_at = Point::new(0, 0);
    let mut resized = false;
    let mut seq: u64 = 5_000_000;
    let mut rpc_id: u64 = 1;
    let mut stat_cache: Vec<(String, Option<Texture>)> = Vec::new();
    let mut last_frames = 0u64;
    let mut last_bytes = 0u64;
    let mut stat_since = Instant::now();
    let mut audio_next = Instant::now();
    let mut err: Option<String> = None;

    match conn_rx.recv_timeout(Duration::from_secs(40)) {
        Ok(Ok(())) => println!("已连接：键鼠已透传，F1 菜单，F11 全屏，关窗即断开。"),
        Ok(Err(e)) => { drop(cmd_tx); let _ = worker.join(); return Err(e); }
        Err(_) => { drop(cmd_tx); let _ = worker.join(); return Err("连接超时（40 秒）".into()); }
    }

    'main: loop {
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. } => break 'main,
                Event::Window { win_event: WindowEvent::Close, .. } => break 'main,
                Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::F1), repeat: false, .. } => { ui.menu_open = !ui.menu_open; ui.panel = None; }
                Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::F11), repeat: false, .. } => { ui.fullscreen = !ui.fullscreen; canvas.window_mut().set_fullscreen(if ui.fullscreen { FullscreenType::Desktop } else { FullscreenType::Off }).ok(); }
                Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::Escape), repeat: false, .. } if ui.menu_open => { ui.menu_open = false; ui.panel = None; }
                Event::KeyDown { scancode: Some(sc), repeat: false, .. } if !ui.menu_open && !view_only => {
                    if let Some(vk) = linux_uuremote::viewer::input::windows_vk(sc as i32) { let _ = cmd_tx.send(Cmd::Key { vk, pressed: true }); }
                }
                Event::KeyUp { scancode: Some(sc), .. } if !ui.menu_open && !view_only => {
                    if let Some(vk) = linux_uuremote::viewer::input::windows_vk(sc as i32) { let _ = cmd_tx.send(Cmd::Key { vk, pressed: false }); }
                }
                Event::MouseMotion { x, y, .. } => {
                    mouse_at = Point::new(x, y);
                    if !ui.menu_open && !view_only && frame_size.0 > 0 {
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
                    if ui.menu_open && ui.hit_menu(x, y) {
                        if let Some(action) = ui.pick(x, y) {
                            match action {
                                Action::Quality(q) => { ui.quality_label = quality_name(q).into(); let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_quality(seq, rpc_id, q))); seq += 1; rpc_id += 1; }
                                Action::Fps(i) => { ui.fps_label = format!("{} 帧", [30u32, 60, 90, 144][(i as usize - 1).min(3)]); let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_fps(seq, rpc_id, i))); seq += 1; rpc_id += 1; }
                                Action::Resolution(s) => { ui.res_label = match s { None => "跟随远程".into(), Some((w, h)) => format!("{w}x{h}") }; let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_resolution(seq, rpc_id, s))); seq += 1; rpc_id += 1; }
                                Action::Chroma(c) => { ui.chroma_label = if c == 3 { "真彩开".into() } else { "真彩关".into() }; let _ = cmd_tx.send(Cmd::Rpc(wire::rpc_chroma(seq, rpc_id, c))); seq += 1; rpc_id += 1; }
                                Action::Sound => { let on = !stats.audio_enabled.load(Ordering::Relaxed); stats.audio_enabled.store(on, Ordering::Relaxed); ui.sound_on = on; }
                                Action::Fullscreen => { ui.fullscreen = !ui.fullscreen; canvas.window_mut().set_fullscreen(if ui.fullscreen { FullscreenType::Desktop } else { FullscreenType::Off }).ok(); }
                                Action::Quit => break 'main,
                            }
                        }
                    } else if !view_only && frame_size.0 > 0 {
                        if let Some(button) = map_button(mouse_btn) { let _ = cmd_tx.send(Cmd::MouseButton { button, pressed: true }); }
                    }
                }
                Event::MouseButtonUp { mouse_btn, .. } => {
                    if !ui.menu_open && !view_only && frame_size.0 > 0 {
                        if let Some(button) = map_button(mouse_btn) { let _ = cmd_tx.send(Cmd::MouseButton { button, pressed: false }); }
                    }
                }
                Event::MouseWheel { x, y, .. } if !ui.menu_open && !view_only => {
                    let _ = cmd_tx.send(Cmd::Scroll { dx: x * 120, dy: y * 120 });
                }
                _ => {}
            }
        }

        if let Ok(result) = conn_rx.try_recv() {
            if let Err(e) = result { err = Some(e); }
            break 'main;
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
        if ui.menu_open {
            ui.draw(&mut canvas, &texture_creator, font, ww);
        }
        mouse_sys.show_cursor(ui.menu_open || view_only);
        if !ui.menu_open && !view_only {
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
            let mbps = (bytes - last_bytes) as f64 * 8.0 / secs / 1_000_000.0;
            last_frames = frames;
            last_bytes = bytes;
            stat_since = Instant::now();
            let frm = stats.last_gap_ms.load(Ordering::Relaxed);
            let ping = stats.rtt_ms.load(Ordering::Relaxed);
            let rw = stats.remote_w.load(Ordering::Relaxed);
            let rh = stats.remote_h.load(Ordering::Relaxed);
            let chroma = stats.chroma.load(Ordering::Relaxed);
            stat_cache = vec![(format!("{fps_now:.0} fps · {mbps:.1} Mbps · frm {frm} ms · {ping} ms · {rw}x{rh}{}", if chroma == 3 { " · 真彩" } else { "" }), None)];
        }
        for (textline, cache) in stat_cache.iter_mut() {
            if cache.is_none() {
                if let Some(tex) = text_texture(&texture_creator, font, textline, Color::RGB(230, 230, 230)) { *cache = Some(tex); }
            }
            if let Some(tex) = cache {
                let q = tex.query();
                let x = (ww - q.width - 12) as i32;
                let y = (wh - q.height - 10) as i32;
                canvas.set_draw_color(Color::RGBA(15, 15, 18, 200));
                canvas.fill_rect(Rect::new(x - 8, y - 6, q.width + 16, q.height + 12)).ok();
                canvas.copy(tex, None, Rect::new(x, y, q.width, q.height)).ok();
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
            Some(i) => ctx.load_font_at_index(path, i, 15),
            None => ctx.load_font(path, 15),
        };
        if let Ok(font) = font { return Some(font); }
    }
    None
}

fn text_texture<'a>(creator: &'a TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, text: &str, color: Color) -> Option<Texture<'a>> {
    let font = font?;
    let surface = font.render(text).solid(color).ok()?;
    creator.create_texture_from_surface(&surface).ok()
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Quality(u64),
    Fps(u64),
    Resolution(Option<(u32, u32)>),
    Chroma(u64),
    Sound,
    Fullscreen,
    Quit,
}

struct Ui {
    menu_open: bool,
    panel: Option<usize>,
    fullscreen: bool,
    sound_on: bool,
    quality_label: String,
    fps_label: String,
    res_label: String,
    chroma_label: String,
    rects: Vec<Rect>,
    option_rects: Vec<(Rect, Action)>,
}

impl Ui {
    fn new(quality: u64, fps: u32) -> Self {
        Self {
            menu_open: false,
            panel: None,
            fullscreen: false,
            sound_on: true,
            quality_label: quality_name(quality).to_owned(),
            fps_label: format!("{fps} 帧"),
            res_label: "跟随远程".to_owned(),
            chroma_label: "真彩关".to_owned(),
            rects: Vec::new(),
            option_rects: Vec::new(),
        }
    }
    fn hit_menu(&self, x: i32, y: i32) -> bool {
        y < 32 || self.option_rects.iter().any(|(r, _)| r.contains_point(Point::new(x, y)))
    }
    fn pick(&mut self, x: i32, y: i32) -> Option<Action> {
        if y < 32 {
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
    fn draw(&mut self, canvas: &mut sdl2::render::Canvas<sdl2::video::Window>, creator: &TextureCreator<WindowContext>, font: Option<&sdl2::ttf::Font<'static, 'static>>, width: u32) {
        canvas.set_draw_color(Color::RGBA(18, 18, 22, 230));
        canvas.fill_rect(Rect::new(0, 0, width, 32)).ok();
        let buttons: Vec<String> = vec![
            format!("画质 {}", self.quality_label),
            self.fps_label.clone(),
            format!("分辨率 {}", self.res_label),
            self.chroma_label.clone(),
            if self.sound_on { "声音 开".into() } else { "声音 关".into() },
            if self.fullscreen { "退出全屏".into() } else { "全屏".into() },
            "断开".into(),
        ];
        self.rects.clear();
        self.option_rects.clear();
        let mut x = 10i32;
        for (i, label) in buttons.iter().enumerate() {
            if let Some(tex) = text_texture(creator, font, label, Color::RGB(235, 235, 235)) {
                let q = tex.query();
                let w = q.width as i32 + 18;
                let r = Rect::new(x, 4, w as u32, 24);
                if self.panel == Some(i) {
                    canvas.set_draw_color(Color::RGBA(80, 120, 220, 230));
                    canvas.fill_rect(r).ok();
                }
                canvas.copy(&tex, None, Rect::new(x + 9, 8, q.width, q.height)).ok();
                self.rects.push(r);
                x += w + 6;
            } else {
                self.rects.push(Rect::new(x, 4, 90, 24));
                x += 96;
            }
        }
        if let Some(i) = self.panel {
            let options: Vec<(String, Action)> = match i {
                0 => vec![("自动".into(), Action::Quality(5)), ("流畅".into(), Action::Quality(1)), ("高清".into(), Action::Quality(2)), ("超清".into(), Action::Quality(3)), ("原画".into(), Action::Quality(4))],
                1 => vec![("30 帧".into(), Action::Fps(1)), ("60 帧".into(), Action::Fps(2)), ("90 帧".into(), Action::Fps(3)), ("144 帧".into(), Action::Fps(4))],
                2 => vec![("跟随被控端".into(), Action::Resolution(None)), ("2K 2560×1440".into(), Action::Resolution(Some((2560, 1440)))), ("1080P".into(), Action::Resolution(Some((1920, 1080)))), ("720P".into(), Action::Resolution(Some((1280, 720))))],
                3 => vec![("关".into(), Action::Chroma(1)), ("开".into(), Action::Chroma(3))],
                4 => vec![("开".into(), Action::Sound), ("关".into(), Action::Sound)],
                5 => vec![("切换".into(), Action::Fullscreen)],
                _ => vec![("断开连接".into(), Action::Quit)],
            };
            let bx = self.rects.get(i).map(|r| r.x()).unwrap_or(10);
            canvas.set_draw_color(Color::RGBA(24, 24, 30, 245));
            canvas.fill_rect(Rect::new(bx, 32, 190, (options.len() as u32) * 26 + 6)).ok();
            for (j, (label, action)) in options.iter().enumerate() {
                let r = Rect::new(bx + 4, 35 + (j as i32) * 26, 182, 24);
                canvas.set_draw_color(Color::RGBA(40, 44, 54, 245));
                canvas.fill_rect(r).ok();
                if let Some(tex) = text_texture(creator, font, label, Color::RGB(235, 235, 235)) {
                    let q = tex.query();
                    canvas.copy(&tex, None, Rect::new(r.x() + 8, r.y() + 4, q.width, q.height)).ok();
                }
                self.option_rects.push((r, *action));
            }
        }
    }
}

fn quality_name(q: u64) -> &'static str {
    match q {
        5 => "自动",
        1 => "流畅",
        2 => "高清",
        3 => "超清",
        4 => "原画",
        _ => "自动",
    }
}
