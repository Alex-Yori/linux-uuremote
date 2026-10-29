//! uu-viewer —— UU 远程 Linux 控制端。
//!
//! 界面照原版客户端的构成做：远程画面窗口 + 顶部设置条（画质/帧率/分辨率/真彩/声音/
//! 全屏/断开）+ 右下角实时状态（fps/码率/frm/ping）。键鼠直接透传进远程，光标由
//! 被控端发来的精灵叠加绘制。
//!
//! 用法：
//!   uu-viewer --list                 列出账号下的设备
//!   uu-viewer --device <id|别名>     连接指定设备
//!   uu-viewer                        列出设备后交互选择
//!   --quality 1..4|auto  --fps 30|60|90|144  初始档位
//!   --no-audio           不播声音
//!   --window 1280x720    初始窗口

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linux_uuremote::api::{ApiClient, Credentials};
use linux_uuremote::config;
use linux_uuremote::viewer::rtc::{CursorImage, Frame, Host, Stats};
use linux_uuremote::viewer::wire;
use linux_uuremote::viewer::{join, Cmd, Session};

use sdl2::event::Event;
use sdl2::mouse::MouseButton;
use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::{Point, Rect};
use sdl2::render::{Texture, TextureCreator};
use sdl2::video::{FullscreenType, WindowContext};
use serde_json::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let flag = |name: &str| args.iter().any(|a| a == name);

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio");
    let (mut creds, target, _devices) = match rt.block_on(select_device(&opt, &flag)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    println!("连接目标：{target}");

    // 初始档位：原画(4)/60 帧接近原客户端默认观感，命令行可改。
    let quality: u64 = match opt("--quality").as_deref() {
        Some("auto") => 5,
        Some("1") => 1,
        Some("2") => 2,
        Some("3") => 3,
        Some("4") => 4,
        _ => 4,
    };
    let fps: u32 = match opt("--fps").as_deref() {
        Some("30") => 30,
        Some("90") => 90,
        Some("144") => 144,
        _ => 60,
    };
    let (win_w, win_h) = match opt("--window").as_deref().and_then(parse_size) { Some(v) => v, None => (1440, 900) };

    let sdl = sdl2::init().expect("SDL 初始化失败");
    let video = sdl.video().expect("视频子系统失败");
    let mouse_sys = sdl.mouse();
    let font = load_font();
    if font.is_none() {
        println!("警告：没找到可用字体，菜单文字将不可见（装 fonts-noto-cjk 试试）");
    }

    let mut canvas = video.window("UU 远程", win_w, win_h).position_centered().resizable().build().expect("窗口创建失败").into_canvas().build().expect("渲染器创建失败");
    let texture_creator = canvas.texture_creator();

    // 声音：SDL queue 模式。opus 解出来的立体声 f32 进环形缓冲，主循环按 10ms 节拍配给
    // 声卡；没数据也要配给静音——SDL queue 空了会播垃圾，必须持续喂。
    let audio_ring: Arc<Mutex<std::collections::VecDeque<f32>>> = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let audio_queue = sdl
        .audio()
        .ok()
        .and_then(|audio| audio.open_queue::<f32, Option<&str>>(None, &sdl2::audio::AudioSpecDesired { freq: Some(48000), channels: Some(2), samples: Some(4096) }).ok());
    if let Some(q) = &audio_queue {
        q.resume();
    }

    let stats = Arc::new(Stats::default());
    stats.audio_enabled.store(!flag("--no-audio") && audio_queue.is_some(), Ordering::Relaxed);
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(2);
    let (cursor_tx, cursor_rx) = std::sync::mpsc::sync_channel::<CursorImage>(2);
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    let host = Arc::new(Host { stats: stats.clone(), frames: frame_tx, cursor: cursor_tx, audio: audio_ring.clone(), pings: Arc::new(Mutex::new(std::collections::HashMap::new())) });
    let shape = wire::Shape::local(win_w, win_h, fps);

    // tokio 线程跑会话；连接结果先送回来，主循环再 poll 断开原因。
    let (conn_tx, conn_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let session_target = target.clone();
    let session_notify = conn_tx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            match Session::open(&room_or_die(creds, &session_target).await, &shape, &session_target, host.clone(), Duration::from_secs(20)).await {
                Ok(mut session) => {
                    let _ = conn_tx.send(Ok(()));
                    let result = session.run(cmd_rx, host.clone()).await;
                    if let Err(e) = &result {
                        println!("   会话结束：{e}");
                    }
                    session.close().await;
                    let _ = session_notify.send(result);
                }
                Err(e) => {
                    println!("   连接失败：{e}");
                    let _ = conn_tx.send(Err(e));
                }
            }
        });
    });

    match conn_rx.recv_timeout(Duration::from_secs(40)) {
        Ok(Ok(())) => println!("已连接：键鼠已透传，F1 菜单，F11 全屏，关窗即断开。"),
        Ok(Err(e)) => { eprintln!("{e}"); std::process::exit(1); }
        Err(_) => { eprintln!("连接超时（40 秒）"); std::process::exit(1); }
    }

    // ---- UI 状态 ----
    let mut ui = Ui::new(quality, fps);
    let mut video_tex: Option<Texture> = None;
    let mut frame_size = (0usize, 0usize);
    let mut cursor_tex: Option<(Texture, i32, i32)> = None;
    let mut mouse_at = Point::new(0, 0);
    let mut resized = false;
    let mut seq: u64 = 5_000_000;
    let mut rpc_id: u64 = 1;
    let mut event_pump = sdl.event_pump().expect("事件泵");

    let mut last_frames = 0u64;
    let mut last_bytes = 0u64;
    let mut stat_since = Instant::now();
    let mut stat_cache: Vec<(String, Option<Texture>)> = Vec::new();
    let mut audio_next = Instant::now();

    'main: loop {
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. } => break 'main,
                Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::F1), repeat: false, .. } => { ui.menu_open = !ui.menu_open; ui.panel = None; }
                Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::F11), repeat: false, .. } => { ui.fullscreen = !ui.fullscreen; canvas.window_mut().set_fullscreen(if ui.fullscreen { FullscreenType::Desktop } else { FullscreenType::Off }).ok(); }
                Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::Escape), repeat: false, .. } if ui.menu_open => { ui.menu_open = false; ui.panel = None; }
                Event::KeyDown { scancode: Some(sc), repeat: false, .. } if !ui.menu_open => {
                    if let Some(vk) = linux_uuremote::viewer::input::windows_vk(sc as i32) { let _ = cmd_tx.send(Cmd::Key { vk, pressed: true }); }
                }
                Event::KeyUp { scancode: Some(sc), .. } if !ui.menu_open => {
                    if let Some(vk) = linux_uuremote::viewer::input::windows_vk(sc as i32) { let _ = cmd_tx.send(Cmd::Key { vk, pressed: false }); }
                }
                Event::MouseMotion { x, y, .. } => {
                    mouse_at = Point::new(x, y);
                    if !ui.menu_open && frame_size.0 > 0 {
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
                    } else if frame_size.0 > 0 {
                        if let Some(button) = map_button(mouse_btn) {
                            let _ = cmd_tx.send(Cmd::MouseButton { button, pressed: true });
                        }
                    }
                }
                Event::MouseButtonUp { mouse_btn, x, y, .. } => {
                    mouse_at = Point::new(x, y);
                    if !ui.menu_open && frame_size.0 > 0 {
                        if let Some(button) = map_button(mouse_btn) {
                            let _ = cmd_tx.send(Cmd::MouseButton { button, pressed: false });
                        }
                    }
                }
                Event::MouseWheel { x, y, .. } if !ui.menu_open => {
                    // 被控端滚轮单位是 Windows notch = 120。
                    let _ = cmd_tx.send(Cmd::Scroll { dx: x * 120, dy: y * 120 });
                }
                _ => {}
            }
        }

        // 会话线程报的断开/错误。
        if let Ok(result) = conn_rx.try_recv() {
            if let Err(e) = result { eprintln!("{e}"); }
            break 'main;
        }

        // 只画最新一帧。
        let mut latest: Option<Frame> = None;
        while let Ok(frame) = frame_rx.try_recv() { latest = Some(frame); }
        if let Some(frame) = latest {
            if frame_size != (frame.width, frame.height) {
                frame_size = (frame.width, frame.height);
                video_tex = Some(texture_creator.create_texture(PixelFormatEnum::YV12, sdl2::render::TextureAccess::Streaming, frame.width as u32, frame.height as u32).expect("纹理创建失败"));
                if !resized {
                    resized = true;
                    // 首帧按远程分辨率收窗口（封顶本机桌面尺寸），像原客户端那样 1:1 起屏。
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

        // 声音配给：每 10ms 补一节 480 帧立体声（缺的补静音）。
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
                if stats.audio_enabled.load(Ordering::Relaxed) {
                    let _ = q.queue_audio(&chunk);
                }
            }
            // 落后太多（比如刚 resume）不追历史节拍，直接对齐现在。
            if audio_next < Instant::now() - Duration::from_millis(100) { audio_next = Instant::now(); }
        }

        // ---- 渲染 ----
        let (ww, wh) = canvas.window().size();
        let vp = linux_uuremote::viewer::input::Viewport::fit((ww, wh), (frame_size.0.max(1) as u32, frame_size.1.max(1) as u32));
        canvas.set_draw_color(Color::RGB(10, 10, 12));
        canvas.clear();
        if let Some(tex) = &video_tex {
            canvas.copy(tex, None, Rect::new(vp.x as i32, vp.y as i32, vp.width as u32, vp.height as u32)).ok();
        }
        if ui.menu_open {
            ui.draw(&mut canvas, &texture_creator, font.as_ref(), ww);
        }
        // 远程光标叠在画面上（位置=本地鼠标，热区按缩放折算）；菜单开着露出系统光标好点按钮。
        mouse_sys.show_cursor(ui.menu_open);
        if !ui.menu_open {
            if let Some((tex, hx, hy)) = &cursor_tex {
                let scale = vp.width / frame_size.0.max(1) as f64;
                let x = mouse_at.x as f64 - *hx as f64 * scale;
                let y = mouse_at.y as f64 - *hy as f64 * scale;
                let q = tex.query();
                canvas.copy(tex, None, Rect::new(x as i32, y as i32, (q.width as f64 * scale) as u32, (q.height as f64 * scale) as u32)).ok();
            }
        }
        // 右下角状态（原客户端同款小条：帧率/码率/frm/ping/分辨率）。
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
        for (text, cache) in stat_cache.iter_mut() {
            if cache.is_none() {
                if let Some(tex) = text_texture(&texture_creator, font.as_ref(), text, Color::RGB(230, 230, 230)) { *cache = Some(tex); }
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

    // 主动退出：cmd_tx 在这里 drop → 桥线程结束 → 会话线程自然收尾。
    drop(cmd_tx);
    println!("已断开。");
}

/// 设备选择：--device 指定，否则列出来让用户选编号。
async fn select_device(opt: &dyn Fn(&str) -> Option<String>, flag: &dyn Fn(&str) -> bool) -> Result<(Credentials, String, Vec<Value>), String> {
    let (file, _borrowed) = config::read_with_fallback()?;
    let mut creds = Credentials::load(file.as_ref())?;
    let api = ApiClient::new(clone_creds(&creds));
    let devices = api.device_list().await?;
    if flag("--list") {
        for d in &devices {
            let id = d.get("device_id").and_then(Value::as_str).unwrap_or("?");
            let alias = d.get("alias").and_then(Value::as_str).unwrap_or("(无名)");
            let status = d.get("status").and_then(Value::as_str).unwrap_or("?");
            println!("{alias}  id={id}  状态={status}");
        }
        std::process::exit(0);
    }
    let wanted = opt("--device");
    // 本机自己作为被控端时挂在 current_device 里，不在 binded 列表——直通。
    // 云端不许同一设备控制自己（同身份 join 会把 publisher 踢下线），所以控制端
    // 用一台注册出来的"虚拟第二机"的身份，首次注册后记在 viewer-identity.json。
    if wanted.as_deref() == Some(creds.device_id.as_str()) {
        let target = wanted.unwrap();
        if std::env::var("UU_VIEWER_TEMP_IDENTITY").is_ok_and(|v| v == "1") {
            println!("提示：目标就是本机，控制端身份换成临时 uuid（云端多半不认）。");
            creds.device_id = wire::uuid();
            creds.client_id = wire::uuid();
        } else {
            let api = ApiClient::new(clone_creds(&creds));
            match viewer_identity(&api).await {
                Ok((did, cid, sid)) => {
                    println!("提示：目标就是本机，控制端以注册的虚拟第二机身份接入（device_id={did}）。");
                    creds.device_id = did;
                    creds.client_id = cid;
                    creds.system_id = sid;
                }
                Err(e) => println!("提示：虚拟身份注册失败（{e}），仍用本机身份试连。"),
            }
        }
        return Ok((creds, target, devices));
    }
    let target = match wanted {
        Some(w) => devices
            .iter()
            .find(|d| d.get("device_id").and_then(Value::as_str) == Some(w.as_str()))
            .and_then(|d| d.get("device_id").and_then(Value::as_str).map(str::to_owned))
            .or_else(|| devices.iter().find(|d| d.get("alias").and_then(Value::as_str).is_some_and(|a| a.contains(&w))).and_then(|d| d.get("device_id").and_then(Value::as_str).map(str::to_owned)))
            .ok_or(format!("账号下找不到设备 {w}"))?,
        None => {
            println!("账号下的设备：");
            if devices.is_empty() { return Err("账号下没有设备，先跑 uu-login 绑定".into()); }
            for (i, d) in devices.iter().enumerate() {
                let id = d.get("device_id").and_then(Value::as_str).unwrap_or("?");
                let alias = d.get("alias").and_then(Value::as_str).unwrap_or("(无名)");
                let status = d.get("status").and_then(Value::as_str).unwrap_or("?");
                let ctl = d.get("controllable").and_then(Value::as_bool).unwrap_or(false);
                println!("  {}. {alias}  ({id})  状态={status}{}", i + 1, if ctl { "  可控制" } else { "" });
                if id == creds.device_id { println!("     ↑ 就是本机（自连自测用）"); }
            }
            println!("输入编号回车（默认 1）：");
            let n = std::io::stdin().lines().next().and_then(|l| l.ok()).and_then(|l| l.trim().parse::<usize>().ok()).unwrap_or(1);
            devices.get(n.wrapping_sub(1).min(devices.len() - 1)).and_then(|d| d.get("device_id").and_then(Value::as_str)).ok_or("没有这个编号")?.to_owned()
        }
    };
    // 自连自测：云端可能不认同一台设备控制自己，把控制端身份换成临时 uuid。
    if target == creds.device_id {
        println!("提示：目标就是本机，控制端身份换成临时 device_id/client_id（同账号 token）。");
        creds.device_id = wire::uuid();
        creds.client_id = wire::uuid();
    }
    Ok((creds, target, devices))
}

/// 控制端专用身份：同账号 token 下注册的第二台"机器"，注册一次后复用。
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

/// Credentials 不可 Copy：手工搬字段。
fn clone_creds(c: &Credentials) -> Credentials {
    Credentials { user_id: c.user_id.clone(), device_id: c.device_id.clone(), client_id: c.client_id.clone(), system_id: c.system_id.clone(), token: c.token.clone(), csrf: c.csrf.clone(), sign_key: c.sign_key.clone() }
}

async fn room_or_die(creds: Credentials, target: &str) -> linux_uuremote::viewer::RoomData {
    let api = ApiClient::new(creds);
    match join(&api, target).await {
        Ok(room) => room,
        Err(e) => {
            eprintln!("   join 失败：{e}");
            std::process::exit(1);
        }
    }
}

/// SDL 鼠标键 → 被控端按钮号（1=左 2=右 4=中，X11 映射在对方那边做）。
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
            menu_open: true,
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
    /// 顶栏 + 下拉。选项按下标回 Action，标签由调用方（点击分支）更新。
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
