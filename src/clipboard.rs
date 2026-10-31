//! 剪贴板同步：官方 `Message{17: ClipboardChange}` wire + X11 `CLIPBOARD` 桥。
//!
//! schema 钉死在 Windows GameViewer.exe 内嵌 gv_pb 描述符（描述符直读实测）：
//! - `ClipboardChange{1 content_id uint32, 2 content string, 3 to_be_continued bool}`
//!   —— 长文本按同一 content_id 分段，`to_be_continued` 为真时等下一段。
//! - `RpcRequest{10: ClipboardTextChangeRequest{1 format_id(uint1=文本), 2 data}}`
//!   ↔ `RpcResponse{6: ClipboardTextChangeResponse{1 err ClipboardError}}`，
//!   `ClipboardError` 1=CB_SUCCEEDED / 2=CB_FAILED。
//! - `FeatureFlag` field 8 `ff_clipboard` = `FF_CLIPBOARD_SUPPORT_TEXT_CHANGE`(3)。
//!
//! 机密纪律：剪贴板**内容永不进日志**，只报字节数与方向。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::control::ClipboardChange;
use crate::initialization::envelope;
use crate::wire::{nested, varint_field, WireValue};

/// 与参照实现同限：UTF-16 + protobuf framing 必须留在数据通道单消息上限以内。
pub const MAX_TEXT: usize = 30 * 1024;
/// `ClipboardTextChangeRequest.format_id`：1 = 纯文本。
pub const FORMAT_TEXT: u64 = 1;
/// `ClipboardError` 枚举值（内嵌描述符实测）：0 未指定 / 1 成功 / 2 失败。
pub const CB_SUCCEEDED: u64 = 1;
pub const CB_FAILED: u64 = 2;

fn put_varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 {
        out.push((v as u8 & 127) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}

/// protobuf tag 也是 varint：字段号 ≥16 时 `(n<<3)|2` ≥128，必须整条 varint 写。
fn put_int(number: u32, value: u64, out: &mut Vec<u8>) {
    if value == 0 {
        return; // protobuf 不序列化标量默认值
    }
    put_varint((u64::from(number) << 3) | 0, out);
    put_varint(value, out);
}

fn put_bytes(number: u32, payload: &[u8], out: &mut Vec<u8>) {
    put_varint((u64::from(number) << 3) | 2, out);
    put_varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

/// RequestHeader：照参照实现无条件写 field 1（request_id=0 也写，客户端按它配对）。
fn put_header(request_id: u64, out: &mut Vec<u8>) {
    out.push(8);
    put_varint(request_id, out);
}

/// 攒分段流：同一 `content_id`、5 秒窗口内拼接；换号或超时丢弃残段。
#[derive(Default)]
pub struct Assembly {
    pending: Option<(u32, String, Instant)>,
}

impl Assembly {
    /// 解完但还没收全 → `Ok(None)`；收全 → `Ok(Some(全文))`；畸形 → `Err`。
    pub fn accept(&mut self, change: &ClipboardChange) -> Result<Option<String>, String> {
        if change.content.len() > MAX_TEXT {
            self.pending = None;
            return Err("剪贴板分段超过 30 KiB 上限".into());
        }
        let now = Instant::now();
        let (mut text, started) = match self.pending.take() {
            Some((prev, text, started)) if prev == change.content_id && now.duration_since(started).as_secs() < 5 => (text, started),
            _ => (String::new(), now),
        };
        if text.len() + change.content.len() > MAX_TEXT {
            self.pending = None;
            return Err("剪贴板总长超过 30 KiB 上限".into());
        }
        text.push_str(&change.content);
        if change.to_be_continued {
            self.pending = Some((change.content_id, text, started));
            Ok(None)
        } else {
            Ok(Some(text))
        }
    }
}

/// `Message{17: ClipboardChange{content_id, content, more}}`。
pub fn encode_clipboard_message(seq: u64, timestamp: u64, content_id: u32, chunk: &str, more: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    put_int(1, u64::from(content_id), &mut payload);
    put_bytes(2, chunk.as_bytes(), &mut payload);
    if more {
        put_int(3, 1, &mut payload);
    }
    envelope(seq, timestamp, 17, &payload)
}

/// 按字节切成 ≤8 KiB 段，段边界绝不切开 UTF-8 字符。
pub fn chunks(text: &str) -> Result<Vec<&str>, String> {
    if text.len() > MAX_TEXT {
        return Err("剪贴板文本超过 30 KiB".into());
    }
    if text.is_empty() {
        return Ok(vec![""]);
    }
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(8192);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    Ok(out)
}

/// 分段号自增（u32 回绕；客户端按号拼接，号只要求会话内一致）。
pub struct ChunkIds(AtomicU32);

impl Default for ChunkIds {
    fn default() -> Self {
        Self(AtomicU32::new(1))
    }
}

impl ChunkIds {
    pub fn next(&self) -> u32 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

/// 解 `RpcRequest` body 里的 `ClipboardTextChangeRequest`（field 10）。
/// 返回 (format_id, data)；缺字段/坏 UTF-8 → None（调用方回 CB_FAILED，绝不猜）。
pub fn read_text_change_request(body: &[crate::wire::WireField<'_>]) -> Option<(u64, String)> {
    let inner = nested(body, 10)?;
    let format = varint_field(&inner, 1)?;
    let data = match inner.iter().find(|f| f.number == 2) {
        Some(f) => match &f.value {
            WireValue::Bytes(b) => b,
            _ => return None,
        },
        None => return None,
    };
    if data.len() > MAX_TEXT {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    Some((format, text.to_owned()))
}

/// `Message{22: RpcResponse{1 header, 6 ClipboardTextChangeResponse{1 err}}}`。
pub fn encode_text_change_response(request_id: u64, own_seq: u64, timestamp: u64, succeeded: bool) -> Vec<u8> {
    let mut header = Vec::new();
    put_header(request_id, &mut header);
    let mut inner = Vec::new();
    put_int(1, if succeeded { CB_SUCCEEDED } else { CB_FAILED }, &mut inner);
    let mut payload = Vec::new();
    put_bytes(1, &header, &mut payload);
    put_bytes(6, &inner, &mut payload);
    envelope(own_seq, timestamp, 22, &payload)
}

/// `RpcResponse{1 header, 5 ClipboardResponse{1 ClipboardFormatListResponse{}}}`：
/// 空格式清单 = 本机只支持文本，文件/富媒体剪贴板客户端会自己收敛。
pub fn encode_clip_response(request_id: u64, own_seq: u64, timestamp: u64) -> Vec<u8> {
    let mut header = Vec::new();
    put_header(request_id, &mut header);
    let mut list = Vec::new();
    put_bytes(1, &[], &mut list);
    let mut payload = Vec::new();
    put_bytes(1, &header, &mut payload);
    put_bytes(5, &list, &mut payload);
    envelope(own_seq, timestamp, 22, &payload)
}

/// 桥线程维护的最新剪贴板快照。`version` 每次内容变化 +1；
/// `from_remote` = 这条是远端写进来的（出帧任务不能再把它发回去）。
#[derive(Clone)]
pub struct Snapshot {
    pub version: u64,
    pub text: Option<String>,
    pub from_remote: bool,
}

/// 桥句柄克隆用（set 的 mpsc Sender 与 state 的 Arc 都可克隆）。
#[derive(Clone)]
pub struct Handle {
    /// 远端 → 本机：把文本写进 X11 `CLIPBOARD` 并抢所有权。
    pub set: std::sync::mpsc::Sender<String>,
    /// 本机剪贴板最新快照（桥线程写，会话出帧任务读）。
    pub state: Arc<Mutex<Snapshot>>,
    /// X11/XFixes 桥是否真的起来了（起不来时 echo 的 ff_clipboard 必须报 0）。
    pub ready: bool,
}

/// 起 X11 CLIPBOARD 桥（独立线程、独立连接，照参照实现 c_engine/clipboard.c 的行为）：
/// - 抢 `CLIPBOARD` 所有权时替远端内容应答 `SelectionRequest`（TARGETS/UTF8_STRING/TEXT）。
/// - XFixes 报"别人换了所有权"→ `ConvertSelection` 抓 UTF-8 → 更新快照。
/// - 现有内容不主动抓（桥启动时不猜）；INCR 与 >30 KiB 一律拒绝，不猜语义。
pub fn start_x11(display: Option<&str>) -> Option<Handle> {
    let (set_tx, set_rx) = std::sync::mpsc::channel::<String>();
    let state = Arc::new(Mutex::new(Snapshot { version: 0, text: None, from_remote: false }));
    let thread_state = state.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
    let display = display.map(str::to_owned);
    std::thread::Builder::new()
        .name("uu-clipboard".into())
        .spawn(move || bridge_thread(display, set_rx, thread_state, ready_tx))
        .ok()?;
    let ready = ready_rx.recv().ok()?;
    if !ready {
        return Some(Handle { set: set_tx, state, ready: false });
    }
    Some(Handle { set: set_tx, state, ready: true })
}

fn bridge_thread(
    display: Option<String>,
    set_rx: std::sync::mpsc::Receiver<String>,
    state: Arc<Mutex<Snapshot>>,
    ready_tx: std::sync::mpsc::Sender<bool>,
) {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xfixes::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::{self, ConnectionExt as _, PropMode};

    let (conn, screen) = match x11rb::connect(display.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("   剪贴板桥：连不上 X11（{e}），剪贴板同步不可用");
            let _ = ready_tx.send(false);
            return;
        }
    };
    let root = conn.setup().roots[screen].root;
    let root_depth = conn.setup().roots[screen].root_depth;
    let root_visual = conn.setup().roots[screen].root_visual;
    let intern = |name: &[u8]| -> Option<u32> {
        match conn.intern_atom(false, name) {
            Ok(c) => c.reply().ok().map(|r| r.atom),
            Err(_) => None,
        }
    };
    let clipboard = match intern(b"CLIPBOARD") { Some(a) => a, None => { let _ = ready_tx.send(false); eprintln!("   剪贴板桥：CLIPBOARD atom 不可用，剪贴板同步不可用"); return; } };
    let utf8 = intern(b"UTF8_STRING").unwrap_or(x11rb::NONE);
    let targets = intern(b"TARGETS").unwrap_or(x11rb::NONE);
    let text_atom = intern(b"TEXT").unwrap_or(x11rb::NONE);
    let property = intern(b"UU_NATIVE_CLIPBOARD").unwrap_or(x11rb::NONE);
    if let Err(e) = xfixes::query_version(&conn, 1, 0) {
        let _ = ready_tx.send(false);
        eprintln!("   剪贴板桥：XFixes 扩展不可用（{e}），剪贴板同步不可用");
        return;
    }
    let win = match conn.generate_id() { Ok(w) => w, Err(_) => { let _ = ready_tx.send(false); return; } };
    if conn.create_window(root_depth, win, root, 0, 0, 1, 1, 0, xproto::WindowClass::INPUT_OUTPUT, root_visual, &Default::default()).is_err() {
        let _ = ready_tx.send(false);
        return;
    }
    // 所有权变更 / 窗口销毁 / 客户端退出三种通知都要（照 clipboard.c 的掩码）。
    let mask = xfixes::SelectionEventMask::from(0b111u32);
    if conn.xfixes_select_selection_input(win, clipboard, mask).is_err() {
        let _ = ready_tx.send(false);
        return;
    }
    if conn.flush().is_err() {
        let _ = ready_tx.send(false);
        return;
    }
    let _ = ready_tx.send(true);

    let mut owned: Vec<u8> = Vec::new();
    let mut owns = false;
    let mut convert_time: u32 = 0;
    let mut version: u64 = 0;
    loop {
        // 远端 → 本机：抢所有权，内容替别人应答 SelectionRequest。
        while let Ok(text) = set_rx.try_recv() {
            let bytes = text.clone().into_bytes();
            let mut got = false;
            if conn.set_selection_owner(win, clipboard, 0u32).map(|c| c.ignore_error()).is_ok() {
                if let Ok(cookie) = conn.get_selection_owner(clipboard) {
                    got = cookie.reply().map(|r| r.owner == win).unwrap_or(false);
                }
            }
            if got {
                owned = bytes;
                owns = true;
                version += 1;
                *state.lock().unwrap() = Snapshot { version, text: Some(text), from_remote: true };
                let _ = conn.flush();
            } else {
                eprintln!("   剪贴板桥：抢 CLIPBOARD 所有权失败（被别的程序占了？）");
            }
        }
        // 事件排空（10 ms 节拍，照 clipboard.c 的 XPending 循环）。
        let mut events = Vec::new();
        loop {
            match conn.poll_for_event() {
                Ok(Some(ev)) => events.push(ev),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        for ev in events {
            match ev {
                x11rb::protocol::Event::SelectionRequest(r) => {
                    let prop = if r.property == x11rb::NONE { r.target } else { r.property };
                    let mut answered = x11rb::NONE;
                    if r.selection == clipboard && owns {
                        if r.target == targets {
                            let list: Vec<u8> = [targets, utf8, text_atom].iter().flat_map(|a| a.to_le_bytes()).collect();
                            if conn.change_property(PropMode::REPLACE, r.requestor, prop, xproto::AtomEnum::ATOM, 32, 3, &list).is_ok() {
                                answered = prop;
                            }
                        } else if r.target == utf8 || r.target == text_atom {
                            if conn.change_property(PropMode::REPLACE, r.requestor, prop, utf8, 8, owned.len() as u32, &owned).is_ok() {
                                answered = prop;
                            }
                        }
                    }
                    // SelectionNotify 手拼 32 字节（type 31 + time/requestor/selection/target/property）。
                    let mut buf = [0u8; 32];
                    buf[0] = xproto::SELECTION_NOTIFY_EVENT;
                    buf[4..8].copy_from_slice(&r.time.to_le_bytes());
                    buf[8..12].copy_from_slice(&r.requestor.to_le_bytes());
                    buf[12..16].copy_from_slice(&r.selection.to_le_bytes());
                    buf[16..20].copy_from_slice(&r.target.to_le_bytes());
                    buf[20..24].copy_from_slice(&answered.to_le_bytes());
                    let _ = conn.send_event(false, r.requestor, xproto::EventMask::NO_EVENT, buf);
                    let _ = conn.flush();
                }
                x11rb::protocol::Event::SelectionClear(e) if e.selection == clipboard => {
                    owns = false; // 别人抢走了，我们的副本作废
                }
                x11rb::protocol::Event::XfixesSelectionNotify(e) if e.selection == clipboard && e.owner != win && e.owner != x11rb::NONE => {
                    convert_time = e.selection_timestamp;
                    let _ = conn.delete_property(win, property);
                    let _ = conn.convert_selection(win, clipboard, utf8, property, convert_time);
                    let _ = conn.flush();
                }
                x11rb::protocol::Event::SelectionNotify(n)
                    if n.selection == clipboard && !owns && n.property != x11rb::NONE && n.time == convert_time =>
                {
                    // INCR（property=None）与超限内容一律拒绝，不猜。
                    let mut reply_opt = None;
                    if let Ok(cookie) = conn.get_property(true, win, property, x11rb::NONE, 0, ((MAX_TEXT + 3) / 4) as u32) {
                        reply_opt = cookie.reply().ok();
                    }
                    if let Some(reply) = reply_opt {
                        if reply.type_ == utf8 && reply.format == 8 && reply.bytes_after == 0 && reply.value.len() <= MAX_TEXT {
                            if let Ok(text) = std::str::from_utf8(&reply.value) {
                                version += 1;
                                *state.lock().unwrap() = Snapshot { version, text: Some(text.to_owned()), from_remote: false };
                                println!("   剪贴板桥：本机剪贴板已更新（{} 字节）", reply.value.len());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::decode_wire;

    fn change(id: u32, content: &str, more: bool) -> ClipboardChange {
        ClipboardChange { seq: 0, timestamp: 0, content_id: id, content: content.to_owned(), to_be_continued: more }
    }

    #[test]
    fn chunks_never_split_utf8_characters() {
        let text = "中文🙂".repeat(3000); // 20 字节/组，9 字节边界必落在字符中间
        let parts = chunks(&text).unwrap();
        assert!(parts.len() > 1);
        assert_eq!(parts.concat(), text);
        assert!(parts.iter().all(|p| p.len() <= 8192));
        assert_eq!(chunks(&"a".repeat(MAX_TEXT + 1)), Err("剪贴板文本超过 30 KiB".into()));
        assert_eq!(chunks(""), Ok(vec![""]));
    }

    #[test]
    fn assembly_stitches_same_id_and_resets_on_new_id() {
        let mut a = Assembly::default();
        assert_eq!(a.accept(&change(7, "ab", true)).unwrap(), None);
        assert_eq!(a.accept(&change(7, "cd", false)).unwrap().as_deref(), Some("abcd"));
        // 残段没等到续段就换号：旧残段作废，不能把两条剪贴板内容拼一起。
        assert_eq!(a.accept(&change(9, "stale", true)).unwrap(), None);
        assert_eq!(a.accept(&change(10, "new", false)).unwrap().as_deref(), Some("new"));
        // 空文本也要能完整过一遍（复制空串是合法操作）。
        assert_eq!(a.accept(&change(1, "", false)).unwrap().as_deref(), Some(""));
    }

    #[test]
    fn assembly_rejects_oversize_segments_and_total() {
        let mut a = Assembly::default();
        assert!(a.accept(&change(1, &"x".repeat(MAX_TEXT + 1), false)).is_err());
        assert!(a.accept(&change(2, &"x".repeat(MAX_TEXT), true)).is_ok());
        assert!(a.accept(&change(2, "x", false)).is_err());
    }

    #[test]
    fn clipboard_message_wire_uses_varint_tags_and_omits_defaults() {
        // field 17 tag = (17<<3)|2 = 138，多字节 varint；单字节 push 会被解成 field 1。
        let m = encode_clipboard_message(5, 100, 3, "hi", true);
        let fields = decode_wire(&m).unwrap();
        assert_eq!(varint_field(&fields, 1), Some(5));
        assert_eq!(varint_field(&fields, 2), Some(100));
        let inner = nested(&fields, 17).unwrap();
        assert_eq!(varint_field(&inner, 1), Some(3));
        assert_eq!(varint_field(&inner, 3), Some(1));
        // content_id=0 且非续段：field 1/3 都该缺席（protobuf 默认值不序列化）。
        let m2 = encode_clipboard_message(0, 0, 0, "hi", false);
        let inner2 = nested(&decode_wire(&m2).unwrap(), 17).unwrap();
        assert_eq!(varint_field(&inner2, 1), None);
        assert_eq!(varint_field(&inner2, 3), None);
    }

    #[test]
    fn text_change_request_roundtrip_and_response_shape() {
        // RpcRequest body：{1 header{1 rid}, 10 ClipboardTextChangeRequest{1 format_id=1, 2 data}}
        let mut header = Vec::new();
        put_int(1, 0x1234, &mut header);
        let mut data = Vec::new();
        put_int(1, FORMAT_TEXT, &mut data);
        put_bytes(2, "UU-test-中文🙂\n".as_bytes(), &mut data);
        let mut body = Vec::new();
        put_bytes(1, &header, &mut body);
        put_bytes(10, &data, &mut body);
        let fields = decode_wire(&body).unwrap();
        let (format, text) = read_text_change_request(&fields).unwrap();
        assert_eq!(format, FORMAT_TEXT);
        assert_eq!(text, "UU-test-中文🙂\n");
        let mut empty = Vec::new();
        put_int(1, 0x1234, &mut empty); // 只有 header，没有 field 10
        assert_eq!(read_text_change_request(&decode_wire(&empty).unwrap()), None);

        let resp = encode_text_change_response(0x1234, 9, 77, true);
        let rf = decode_wire(&resp).unwrap();
        assert_eq!(varint_field(&rf, 1), Some(9));
        assert_eq!(varint_field(&rf, 2), Some(77));
        let inner = nested(&rf, 22).unwrap();
        let hdr = nested(&inner, 1).unwrap();
        assert_eq!(varint_field(&hdr, 1), Some(0x1234));
        let tr = nested(&inner, 6).unwrap();
        assert_eq!(varint_field(&tr, 1), Some(CB_SUCCEEDED));
    }

    #[test]
    fn clip_response_carries_empty_format_list() {
        let resp = encode_clip_response(42, 5, 50);
        let rf = decode_wire(&resp).unwrap();
        let inner = nested(&rf, 22).unwrap();
        let clip = nested(&inner, 5).unwrap();
        // ClipboardResponse{1: 空 ClipboardFormatListResponse}
        assert!(clip.iter().any(|f| f.number == 1 && matches!(&f.value, WireValue::Bytes(b) if b.is_empty())));
    }
}
