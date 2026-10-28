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
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::{ConnectionExt as _, CursorNotifyMask};
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateWindowAux, ImageFormat, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::shm::ShmSegment;
use super::DirtyArea;

/// Assumed cursor sprite size for dirty-area reporting when only the position is known.
/// `XFixesGetCursorImage` can report the real sprite; a conservative box is enough to
/// mark the frame dirty and to bound a later dirty-region encode.
const CURSOR_BOX: u16 = 32;

/// Side length of the test window `verify_readback` paints.
const VERIFY_SIDE: u16 = 256;

/// What `verify_readback` measured.
#[derive(Debug)]
pub struct VerifyReport {
    /// Frame size both paths returned.
    pub bytes: usize,
    /// Share of the test window's own rectangle showing the painted color, per path.
    pub painted_getimage: f64,
    pub painted_shm: f64,
    /// Share of a server-cut tile of that rectangle showing the painted color.
    pub painted_tile: f64,
    /// Whether the tile equals the same rectangle sliced out of a full grab. Informational:
    /// a live desktop can repaint between the two grabs.
    pub tile_matches_full: bool,
    /// Share of that rectangle whose bytes agree between the two paths.
    pub rect_identical: f64,
    /// Share of that rectangle that changed once the window was mapped — proof that a root
    /// grab reflects real screen content rather than a stale backing store.
    pub painted_changed: f64,
    /// Whole-frame equality; informational only, since a live desktop changes between grabs.
    pub frame_identical: bool,
    /// Where the window actually ended up (the window manager may move ordinary windows).
    pub rect: (i16, i16, u16, u16),
    /// Cost of the shared-memory grab (server write + copy out).
    pub shm_ms: f64,
    /// Cost and size of the same rectangle grabbed as a tile.
    pub tile_ms: f64,
    pub tile_bytes: usize,
}

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
    root_depth: u8,
    root_visual: u32,
    /// Bytes per pixel for the root depth, read from the server rather than assumed: a
    /// non-32-bit packed format would shear the whole picture.
    bytes_per_pixel: usize,
    damage_level: DamageLevel,
    damage: u32,
    /// Last reported cursor position, used to detect cursor-only motion.
    last_cursor: Option<(i32, i32)>,
    /// Last cursor image serial, used to ignore repeated same-cursor notifications.
    last_cursor_serial: Option<u32>,
    /// MIT-SHM segment for fast readback; `None` means `GetImage` fallback.
    shm: Option<ShmSegment>,
    /// Why MIT-SHM is unavailable, for diagnostics (`None` when it is active).
    shm_unavailable: Option<String>,
}

/// Which readback path produced a frame, and what each stage cost.
#[derive(Debug, Clone, Copy)]
pub enum GrabTiming {
    /// Server wrote into shared memory; we copied out of the mapping.
    Shm { server_ms: f64, copy_ms: f64 },
    /// Pixels came through the X socket.
    GetImage { ms: f64 },
}

impl GrabTiming {
    pub fn label(&self) -> &'static str {
        match self {
            GrabTiming::Shm { .. } => "shm",
            GrabTiming::GetImage { .. } => "getimage",
        }
    }

    /// End-to-end cost of producing owned pixel bytes.
    pub fn total_ms(&self) -> f64 {
        match self {
            GrabTiming::Shm { server_ms, copy_ms } => *server_ms + *copy_ms,
            GrabTiming::GetImage { ms } => *ms,
        }
    }
}

/// Which XDamage report level to register on the root window.
///
/// This decides how much geometry the server hands back, and therefore whether a dirty-rect
/// grab is possible at all: `NonEmpty` only says "something changed", `BoundingBox` reports
/// the extents of the change, and `RawRectangles` reports each rectangle separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageLevel {
    NonEmpty,
    BoundingBox,
    RawRectangles,
}

impl DamageLevel {
    fn report_level(self) -> ReportLevel {
        match self {
            DamageLevel::NonEmpty => ReportLevel::NON_EMPTY,
            DamageLevel::BoundingBox => ReportLevel::BOUNDING_BOX,
            DamageLevel::RawRectangles => ReportLevel::RAW_RECTANGLES,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DamageLevel::NonEmpty => "nonempty",
            DamageLevel::BoundingBox => "bbox",
            DamageLevel::RawRectangles => "raw",
        }
    }
}

impl X11Screen {
    /// Connects to `display` (e.g. `:1`), sets up XDamage on the root window and XFixes
    /// cursor notification. A returned major version of 0 means the extension is not
    /// supported, so the error names exactly which one is missing.
    pub fn open(display: Option<&str>, level: DamageLevel) -> Result<Self, X11Error> {
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
        let root_depth = screen.root_depth;
        let root_visual = screen.root_visual;
        // `GetImage` hands back packed pixels in the format the server declares for this
        // depth. Everything downstream assumes 4 bytes per pixel, so refuse loudly instead
        // of grabbing a sheared picture on an unusual display.
        let format = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == root_depth)
            .ok_or_else(|| X11Error(format!("no pixmap format for root depth {root_depth}")))?;
        if format.bits_per_pixel != 32 {
            return Err(X11Error(format!(
                "root depth {root_depth} is {} bits per pixel; this build needs packed 32-bit pixels",
                format.bits_per_pixel
            )));
        }
        let bytes_per_pixel = (format.bits_per_pixel / 8) as usize;
        let damage = conn.generate_id().map_err(text("generate_id"))?;
        // The level decides the geometry we get; re-arming happens through `subtract()` in
        // `poll_events`, which is common to all three levels.
        conn.damage_create(damage, root, level.report_level())
            .map_err(text("damage_create"))?;
        conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)
            .map_err(text("xfixes_select_cursor_input"))?;
        conn.flush().map_err(text("flush"))?;

        // Try the fast readback path. Failure is expected and normal (TCP connection, an
        // older extension, or a server that refuses), so it degrades rather than errors.
        let (shm, shm_unavailable) = match ShmSegment::create(
            &conn,
            width as usize * height as usize * bytes_per_pixel,
        ) {
            Ok(seg) => (Some(seg), None),
            Err(e) => (None, Some(e.0)),
        };

        Ok(Self {
            conn,
            root,
            width,
            height,
            root_depth,
            root_visual,
            bytes_per_pixel,
            damage_level: level,
            damage,
            last_cursor: None,
            last_cursor_serial: None,
            shm,
            shm_unavailable,
        })
    }

    /// `"shm"` when the fast path is live, otherwise `"getimage"`; plus the reason.
    pub fn readback(&self) -> (&'static str, Option<&str>) {
        match &self.shm {
            Some(_) => ("shm", None),
            None => ("getimage", self.shm_unavailable.as_deref()),
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// Bytes per full frame, using the packed format the server declares (32 bpp here).
    pub fn frame_bytes(&self) -> usize {
        self.width as usize * self.height as usize * self.bytes_per_pixel
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

    /// Recreate the segment when the root geometry outgrew it (resize).
    fn ensure_shm(&mut self) {
        let want = self.frame_bytes();
        if self.shm.as_ref().is_some_and(|seg| seg.len() == want) {
            return;
        }
        self.detach_shm();
        match ShmSegment::create(&self.conn, want) {
            Ok(seg) => {
                self.shm = Some(seg);
                self.shm_unavailable = None;
            }
            Err(e) => {
                self.shm_unavailable = Some(e.0);
                self.shm = None;
            }
        }
    }

    /// Release the server-side segment; the mapping and descriptor go with it.
    fn detach_shm(&mut self) {
        if let Some(seg) = self.shm.take() {
            let _ = self.conn.shm_detach(seg.seg());
            let _ = self.conn.flush();
        }
    }

    /// Take one rectangle of pixel data; `None` is the whole screen.
    ///
    /// A dirty-rect grab is what an incremental encoder wants: the server writes the
    /// rectangle's pixels at the start of the segment with the *rectangle's* stride, so the
    /// caller gets the tile itself rather than a full-frame buffer to slice. What is saved is
    /// the server-side pixel copy it no longer performs for the untouched part of the screen.
    pub fn grab_rect(&mut self, rect: Option<DirtyArea>) -> Result<(Vec<u8>, GrabTiming), X11Error> {
        // The segment has to be the size of the whole screen: a tile lands inside it too, and
        // a root resize would otherwise leave it too small.
        self.ensure_shm();
        if self.shm.is_some() {
            match self.grab_shm(rect) {
                Ok(frame) => return Ok(frame),
                Err(e) => {
                    self.detach_shm();
                    self.shm_unavailable = Some(format!("shm grab failed: {}", e.0));
                }
            }
        }
        self.grab_getimage(rect)
    }

    /// Clamp a damage rectangle into the screen, `None` when it covers nothing.
    ///
    /// XDamage reports overlap and can extend past the screen edges (the probe has measured
    /// more than 100% of the screen in one frame), so a raw report is not a safe grab geometry.
    pub fn clamp_rect(&self, area: DirtyArea) -> Option<DirtyArea> {
        let x0 = area.x.max(0);
        let y0 = area.y.max(0);
        let x1 = (area.x as i32 + area.width as i32).min(self.width as i32);
        let y1 = (area.y as i32 + area.height as i32).min(self.height as i32);
        if x1 <= x0 as i32 || y1 <= y0 as i32 {
            return None;
        }
        Some(DirtyArea {
            x: x0,
            y: y0,
            width: (x1 - x0 as i32) as u16,
            height: (y1 - y0 as i32) as u16,
        })
    }

    /// Bytes in a `w` x `h` rectangle of the packed root format.
    pub fn rect_bytes(&self, w: u16, h: u16) -> usize {
        w as usize * h as usize * self.bytes_per_pixel
    }

    /// Bytes per pixel of the root format.
    pub fn bytes_per_pixel(&self) -> usize {
        self.bytes_per_pixel
    }

    /// The XDamage report level registered on the root window.
    pub fn damage_level(&self) -> DamageLevel {
        self.damage_level
    }

    fn grab_shm(&mut self, rect: Option<DirtyArea>) -> Result<(Vec<u8>, GrabTiming), X11Error> {
        let seg = self
            .shm
            .as_ref()
            .ok_or_else(|| X11Error("no shared memory segment".into()))?;
        if seg.len() != self.frame_bytes() {
            return Err(X11Error(format!(
                "segment is {} bytes, frame needs {}",
                seg.len(),
                self.frame_bytes()
            )));
        }
        let (ox, oy, w, h) = self.grab_geometry(rect)?;
        let want = self.rect_bytes(w, h);

        let started = std::time::Instant::now();
        let reply = self
            .conn
            .shm_get_image(
                self.root,
                ox,
                oy,
                w,
                h,
                u32::MAX,
                ImageFormat::Z_PIXMAP.into(),
                seg.seg(),
                0,
            )
            .map_err(text("shm_get_image"))?
            .reply()
            .map_err(text("shm_get_image reply"))?;
        let server_ms = started.elapsed().as_secs_f64() * 1000.0;

        if reply.size as usize != want {
            return Err(X11Error(format!(
                "server transferred {} bytes, expected {want}",
                reply.size
            )));
        }

        // The copy out is what an encoder that needs owned buffers pays; NVENC can later
        // consume the mapping directly, which removes this stage.
        let copy_started = std::time::Instant::now();
        let data = seg.bytes()[..want].to_vec();
        let copy_ms = copy_started.elapsed().as_secs_f64() * 1000.0;

        Ok((data, GrabTiming::Shm { server_ms, copy_ms }))
    }

    fn grab_getimage(&self, rect: Option<DirtyArea>) -> Result<(Vec<u8>, GrabTiming), X11Error> {
        let (ox, oy, w, h) = self.grab_geometry(rect)?;
        let started = std::time::Instant::now();
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                ox,
                oy,
                w,
                h,
                u32::MAX,
            )
            .map_err(text("get_image"))?
            .reply()
            .map_err(text("get_image reply"))?;
        Ok((
            reply.data,
            GrabTiming::GetImage {
                ms: started.elapsed().as_secs_f64() * 1000.0,
            },
        ))
    }

    /// Resolve a requested rectangle into `(x, y, width, height)` in screen coordinates.
    fn grab_geometry(&self, rect: Option<DirtyArea>) -> Result<(i16, i16, u16, u16), X11Error> {
        match rect {
            Some(area) => {
                let r = self
                    .clamp_rect(area)
                    .ok_or_else(|| X11Error("dirty rectangle covers no screen pixels".into()))?;
                Ok((r.x as i16, r.y as i16, r.width, r.height))
            }
            None => Ok((0i16, 0i16, self.width, self.height)),
        }
    }

    /// Proof that the shared-memory path really returns framebuffer content.
    ///
    /// Grabs the root before and after mapping an override-redirect solid red window, then
    /// grabs once through each path. Three separate questions get answered, because passing
    /// all of them needs more than "the buffers are not empty":
    ///   * does a root grab reflect real screen content? -> `painted_changed`
    ///   * does the shared mapping hold the painted pixels? -> `painted_shm`
    ///   * do both paths agree on that rectangle?          -> `rect_identical`
    /// `frame_identical` is informational: a live desktop keeps redrawing between grabs.
    ///
    /// The window is override-redirect so the window manager cannot reposition it, and its
    /// actual geometry is read back rather than trusting the requested position.
    pub fn verify_readback(&mut self) -> Result<VerifyReport, X11Error> {
        let x = 64i16;
        let y = (self.height as i32 / 2 - (VERIFY_SIDE as i32 / 2)).max(0) as i16;
        // TrueColor root: bits 16..23 carry red, so 0x00FF0000 is pure red. ZPixmap bytes
        // for a packed 32-bit pixel are B, G, R, pad on a little-endian machine.
        let pixel: u32 = 0x00FF_0000;

        let wid = self.conn.generate_id().map_err(text("generate_id"))?;
        // `Bool32` is a plain `u32` in X11, so 1 means True: the window manager must not
        // reposition or resize the test window out from under the measurement.
        let aux = CreateWindowAux::new()
            .background_pixel(pixel)
            .override_redirect(1u32);
        self.conn
            .create_window(
                self.root_depth,
                wid,
                self.root,
                x,
                y,
                VERIFY_SIDE,
                VERIFY_SIDE,
                0,
                WindowClass::INPUT_OUTPUT,
                self.root_visual,
                &aux,
            )
            .map_err(text("create_window"))?;

        // Reference frame with the window still unmapped.
        let before = self.grab_getimage(None)?.0;

        self.conn.map_window(wid).map_err(text("map_window"))?;
        self.conn.sync().map_err(text("sync"))?;
        // Let the compositor actually put the window on screen before grabbing.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let result = (|| -> Result<VerifyReport, X11Error> {
            let g = self
                .conn
                .get_geometry(wid)
                .map_err(text("get_geometry"))?
                .reply()
                .map_err(text("get_geometry reply"))?;
            let (via_getimage, _) = self.grab_getimage(None)?;
            let (via_shm, timing) = self.grab_shm(None)?;
            // The same rectangle cut out by the server instead of sliced by us: this is what
            // an incremental frame would carry, so it has to be correct on its own.
            let tile_area = DirtyArea {
                x: g.x,
                y: g.y,
                width: g.width,
                height: g.height,
            };
            let (tile, tile_timing) = self.grab_shm(Some(tile_area))?;
            Ok(VerifyReport {
                bytes: via_shm.len(),
                painted_getimage: self.paint_ratio(&via_getimage, g.x, g.y, g.width, g.height),
                painted_shm: self.paint_ratio(&via_shm, g.x, g.y, g.width, g.height),
                painted_tile: self.tile_paint_ratio(&tile, g.width, g.height),
                tile_matches_full: self.tile_matches_full(
                    &tile,
                    &via_getimage,
                    g.x,
                    g.y,
                    g.width,
                    g.height,
                ),
                rect_identical: self.equal_ratio(
                    &via_getimage,
                    &via_shm,
                    g.x,
                    g.y,
                    g.width,
                    g.height,
                ),
                painted_changed: 1.0
                    - self.equal_ratio(&before, &via_getimage, g.x, g.y, g.width, g.height),
                frame_identical: via_getimage == via_shm,
                rect: (g.x, g.y, g.width, g.height),
                shm_ms: timing.total_ms(),
                tile_ms: tile_timing.total_ms(),
                tile_bytes: tile.len(),
            })
        })();

        // Always clean up the test window, even if a grab failed.
        let _ = self.conn.destroy_window(wid);
        let _ = self.conn.sync();

        result
    }

    /// Share of `rect` whose pixels are the painted red.
    fn paint_ratio(&self, frame: &[u8], x: i16, y: i16, w: u16, h: u16) -> f64 {
        let stride = self.bytes_per_pixel;
        self.for_each_pixel(x, y, w, h, |off| {
            if off + stride > frame.len() {
                return None;
            }
            // B, G, R, pad -> pure red is (0, 0, 255).
            Some((frame[off] == 0 && frame[off + 1] == 0 && frame[off + 2] == 255) as u64)
        })
    }

    /// Share of `rect` whose bytes are equal in `a` and `b`.
    fn equal_ratio(&self, a: &[u8], b: &[u8], x: i16, y: i16, w: u16, h: u16) -> f64 {
        let stride = self.bytes_per_pixel;
        self.for_each_pixel(x, y, w, h, |off| {
            if off + stride > a.len() || off + stride > b.len() {
                return None;
            }
            Some((a[off..off + stride] == b[off..off + stride]) as u64)
        })
    }

    /// Share of a server-cut tile showing the painted red.
    ///
    /// A tile has the rectangle's own stride, so this cannot reuse the full-screen walk.
    fn tile_paint_ratio(&self, tile: &[u8], w: u16, h: u16) -> f64 {
        let stride = self.bytes_per_pixel;
        let mut hit = 0u64;
        let mut total = 0u64;
        for row in 0..h as usize {
            for col in 0..w as usize {
                let off = (row * w as usize + col) * stride;
                if off + stride > tile.len() {
                    continue;
                }
                total += 1;
                if tile[off] == 0 && tile[off + 1] == 0 && tile[off + 2] == 255 {
                    hit += 1;
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            hit as f64 / total as f64
        }
    }

    /// Whether a tile equals the same rectangle cut out of a full-frame buffer.
    fn tile_matches_full(
        &self,
        tile: &[u8],
        full: &[u8],
        x: i16,
        y: i16,
        w: u16,
        h: u16,
    ) -> bool {
        let stride = self.bytes_per_pixel;
        let row_len = w as usize * stride;
        for row in 0..h as usize {
            let py = y as usize + row;
            if py >= self.height as usize {
                return false;
            }
            let tile_off = row * row_len;
            let full_off = (py * self.width as usize + x as usize) * stride;
            if tile_off + row_len > tile.len() || full_off + row_len > full.len() {
                return false;
            }
            if tile[tile_off..tile_off + row_len] != full[full_off..full_off + row_len] {
                return false;
            }
        }
        true
    }

    /// Walk `rect` clamped to the screen, counting pixels a predicate accepts.
    ///
    /// The predicate returns `None` for pixels it cannot judge (out of buffer), which are
    /// excluded from the denominator rather than counted as failures.
    fn for_each_pixel(
        &self,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        mut accept: impl FnMut(usize) -> Option<u64>,
    ) -> f64 {
        let mut hit = 0u64;
        let mut total = 0u64;
        for row in 0..h as usize {
            let py = y as usize + row;
            if py >= self.height as usize {
                continue;
            }
            for col in 0..w as usize {
                let px = x as usize + col;
                if px >= self.width as usize {
                    continue;
                }
                let off = (py * self.width as usize + px) * self.bytes_per_pixel;
                match accept(off) {
                    Some(ok) => {
                        total += 1;
                        hit += ok;
                    }
                    None => {}
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            hit as f64 / total as f64
        }
    }
}