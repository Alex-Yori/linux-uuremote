//! X11 capture backend for the controlled host.
//!
//! Design mirrors what the official engine does (see `docs/SPEC.md`): decide *whether*
//! to capture from cheap change notifications instead of reading the whole framebuffer
//! unconditionally.
//!
//!   * `DamageNotify` on the root window = "the screen changed", and its report rectangle
//!     *is* the dirty bbox the official `[VideoPreprocessSource] … dirty_bbox=[…]` logs
//!     carry. Report level `NonEmpty` yields the bounding box of the new damage.
//!   * Cursor-only motion is NOT reported by XDamage (the cursor is not window content),
//!     and the official host composites the cursor into the frame (it has a `cursor`
//!     config field), so cursor motion must count as dirty too. That is covered by
//!     polling `QueryPointer` — one cheap request — plus `XFixesSelectCursorInput`, which
//!     fires when the cursor *shape* changes (e.g. an I-beam appearing over text).
//!
//! Pixel readback currently uses core-protocol `GetImage`. `XShm` (feature already
//! declared) is the planned fast path; see README "Roadmap".

use std::error::Error;
use std::fmt;

use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as _, ReportLevel};
use x11rb::protocol::xfixes::{ConnectionExt as _, CursorNotifyMask};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

use super::DirtyArea;

/// Assumed cursor sprite size for dirty-area reporting when only the position is known.
/// `XFixesGetCursorImage` can report the real sprite; a conservative box is enough to
/// mark the frame dirty and to bound a later dirty-region encode.
const CURSOR_BOX: u16 = 32;

#[derive(Debug)]
pub struct X11Error(pub String);

impl fmt::Display for X11Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "x11: {}", self.0)
    }
}

impl Error for X11Error {}

/// Build an error-context closure formatting any Debug error into `X11Error`.
///
/// The `use<'_, E>` bound is required on Rust 1.99: without it the returned `impl Fn`
/// may not capture the anonymous lifetime of the borrowed `ctx`.
fn text<E: fmt::Debug>(ctx: &str) -> impl Fn(E) -> X11Error + use<'_, E> {
    move |e| X11Error(format!("{ctx}: {e:?}"))
}

pub struct X11Screen {
    conn: RustConnection,
    root: Window,
    width: u16,
    height: u16,
    damage: u32,
    /// Last reported cursor position, used to detect cursor-only motion.
    last_cursor: Option<(i32, i32)>,
    /// Last cursor image serial, used to ignore repeated same-cursor notifications.
    last_cursor_serial: Option<u32>,
}

impl X11Screen {
    /// Connects to `display` (e.g. `:1`), sets up XDamage on the root window and XFixes
    /// cursor notification. A returned major version of 0 means the extension is not
    /// supported, so the error names exactly which one is missing.
    pub fn open(display: Option<&str>) -> Result<Self, X11Error> {
        let (conn, screen_num) = x11rb::connect(display).map_err(text("connect"))?;
        let screen = &conn.setup().roots[screen_num];

        let dmg = conn
            .damage_query_version(1, 1)
            .map_err(text("damage_query_version"))?
            .reply()
            .map_err(text("damage_query_version reply"))?;
        if dmg.major_version == 0 {
            return Err(X11Error(
                "XDamage extension is not supported by this X server".into(),
            ));
        }

        let xfixes_ver = conn
            .xfixes_query_version(1, 0)
            .map_err(text("xfixes_query_version"))?
            .reply()
            .map_err(text("xfixes_query_version reply"))?;
        if xfixes_ver.major_version == 0 {
            return Err(X11Error(
                "XFixes extension is not supported by this X server".into(),
            ));
        }

        let root = screen.root;
        // Copy geometry out before `conn` is moved, so the `setup()` borrow ends here.
        let width = screen.width_in_pixels;
        let height = screen.height_in_pixels;
        let damage = conn.generate_id().map_err(text("generate_id"))?;
        // NonEmpty: report once each time the root goes clean -> damaged; re-armed by subtract().
        conn.damage_create(damage, root, ReportLevel::NON_EMPTY)
            .map_err(text("damage_create"))?;
        conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)
            .map_err(text("xfixes_select_cursor_input"))?;
        conn.flush().map_err(text("flush"))?;

        Ok(Self {
            conn,
            root,
            width,
            height,
            damage,
            last_cursor: None,
            last_cursor_serial: None,
        })
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// Bytes per full frame: `GetImage` on a 24/32 bpp root yields 4 bytes per pixel.
    pub fn frame_bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// Drain pending X events. Returns every dirty rectangle reported since the last call.
    ///
    /// `DamageNotify` areas are consumed with `DamageSubtract` so the damage region starts
    /// filling up again. `XFixesCursorNotify` means the cursor shape changed, which also
    /// changes pixels once the cursor is composited into the frame.
    pub fn poll_events(&mut self) -> Result<Vec<DirtyArea>, X11Error> {
        self.conn.flush().map_err(text("flush"))?;
        let mut areas = Vec::new();
        loop {
            let ev = match self.conn.poll_for_event().map_err(text("poll_for_event"))? {
                Some(ev) => ev,
                None => break,
            };
            match ev {
                x11rb::protocol::Event::DamageNotify(e) => {
                    areas.push(DirtyArea {
                        x: e.area.x,
                        y: e.area.y,
                        width: e.area.width,
                        height: e.area.height,
                    });
                    // (NONE, NONE) clears the whole damage region, re-arming the report.
                    self.conn
                        .damage_subtract(self.damage, x11rb::NONE, x11rb::NONE)
                        .map_err(text("damage_subtract"))?;
                }
                x11rb::protocol::Event::XfixesCursorNotify(e) => {
                    // DISPLAY_CURSOR notifications fire whenever the server (re)sets the
                    // cursor image, which happens far more often than the sprite actually
                    // changes (an animated busy cursor, or a compositor re-applying the
                    // same cursor). Only a new serial means pixels really changed; treating
                    // every notify as dirty would pin the gate in the moving regime forever.
                    if self.last_cursor_serial == Some(e.cursor_serial) {
                        continue;
                    }
                    self.last_cursor_serial = Some(e.cursor_serial);
                    let pos = self.cursor_pos()?;
                    areas.push(self.cursor_area_at(pos.0, pos.1));
                }
                _ => {}
            }
        }
        Ok(areas)
    }

    /// Polls the cursor position and reports a dirty box if it moved.
    ///
    /// Cheap (one small round trip), so the caller can run it while the screen is
    /// otherwise unchanged — that is exactly when cursor-only motion matters.
    pub fn poll_cursor(&mut self) -> Result<Option<DirtyArea>, X11Error> {
        let pos = self.cursor_pos()?;
        if self.last_cursor == Some(pos) {
            return Ok(None);
        }
        self.last_cursor = Some(pos);
        Ok(Some(self.cursor_area_at(pos.0, pos.1)))
    }

    fn cursor_pos(&mut self) -> Result<(i32, i32), X11Error> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .map_err(text("query_pointer"))?
            .reply()
            .map_err(text("query_pointer reply"))?;
        Ok((reply.root_x as i32, reply.root_y as i32))
    }

    fn cursor_area_at(&self, x: i32, y: i32) -> DirtyArea {
        let half = (CURSOR_BOX / 2) as i32;
        let max_x = (self.width as i32 - CURSOR_BOX as i32).max(0);
        let max_y = (self.height as i32 - CURSOR_BOX as i32).max(0);
        DirtyArea {
            x: (x - half).clamp(0, max_x) as i16,
            y: (y - half).clamp(0, max_y) as i16,
            width: CURSOR_BOX,
            height: CURSOR_BOX,
        }
    }

    /// Full-frame readback (BGRA/32 bpp for a 24/32 bpp root).
    ///
    /// Returns raw pixel bytes plus the elapsed milliseconds, so callers can report the
    /// readback cost next to the frame rate.
    pub fn grab(&self) -> Result<(Vec<u8>, f64), X11Error> {
        let started = std::time::Instant::now();
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                self.width,
                self.height,
                u32::MAX,
            )
            .map_err(text("get_image"))?
            .reply()
            .map_err(text("get_image reply"))?;
        Ok((reply.data, started.elapsed().as_secs_f64() * 1000.0))
    }
}

/// The report levels this backend may switch between later.
#[allow(dead_code)]
const _REPORT_LEVELS: [ReportLevel; 3] = [
    damage::ReportLevel::RAW_RECTANGLES,
    damage::ReportLevel::BOUNDING_BOX,
    damage::ReportLevel::NON_EMPTY,
];
