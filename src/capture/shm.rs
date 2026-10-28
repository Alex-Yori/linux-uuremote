//! MIT-SHM fast pixel readback.
//!
//! `GetImage` sends the whole framebuffer through the X socket: on this machine one
//! 2560x1440 frame costs 15–17 ms (release build) and 58 ms in debug, which caps capture
//! at roughly 55 fps no matter how smart the gate is. MIT-SHM instead has the server write
//! the pixels into a shared memory segment, so the socket only carries a small reply.
//!
//! Two flavours exist:
//!   * classic `ShmAttach` wants a **SysV** `shmid` (needs `shmget`/`shmat`, i.e. libc IPC)
//!   * `ShmCreateSegment` (extension minor >= 1) has the **server** create the segment and
//!     hand us a file descriptor over the socket, which we `mmap` ourselves.
//!
//! The fd flavour is used here: no SysV IPC, no shared-memory key collisions, and the
//! descriptor is closed automatically when dropped. It only works over a connection that
//! can pass descriptors (a Unix socket) — over TCP the create request fails, and the caller
//! falls back to `GetImage`.

use std::os::unix::io::{AsRawFd, OwnedFd};
use std::ptr::NonNull;

use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as _;

use super::x11::X11Error;

fn text<E: std::fmt::Debug>(ctx: &str) -> impl Fn(E) -> X11Error + use<'_, E> {
    move |e| X11Error(format!("{ctx}: {e:?}"))
}

/// A shared memory segment the X server writes image data into.
pub struct ShmSegment {
    seg: u32,
    /// Keeps the descriptor alive; closing it on drop is the point.
    _fd: OwnedFd,
    map: NonNull<u8>,
    len: usize,
}

impl ShmSegment {
    /// Asks the server for a segment of `size` bytes and maps it read/write.
    ///
    /// Returns an error (not a panic) when the extension is too old, the connection cannot
    /// pass file descriptors, or the server refuses — callers treat that as "use GetImage".
    pub fn create<C: Connection>(conn: &C, size: usize) -> Result<Self, X11Error> {
        let ver = conn
            .shm_query_version()
            .map_err(text("shm_query_version"))?
            .reply()
            .map_err(text("shm_query_version reply"))?;
        if ver.major_version == 0 {
            return Err(X11Error("MIT-SHM extension is not supported".into()));
        }
        if ver.minor_version < 1 {
            return Err(X11Error(format!(
                "MIT-SHM {}.{} has no ShmCreateSegment (needs >= 1.1)",
                ver.major_version, ver.minor_version
            )));
        }

        let seg = conn.generate_id().map_err(text("generate_id"))?;
        // read_only = false: we both read pixels and (later) may write scratch into it.
        let reply = conn
            .shm_create_segment(seg, size as u32, false)
            .map_err(text("shm_create_segment"))?
            .reply()
            .map_err(text("shm_create_segment reply"))?;

        let fd: OwnedFd = reply.shm_fd;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(X11Error(format!(
                "mmap of {} shared bytes failed: {}",
                size,
                std::io::Error::last_os_error()
            )));
        }
        let map = NonNull::new(ptr as *mut u8)
            .ok_or_else(|| X11Error("mmap returned a null mapping".into()))?;

        Ok(Self {
            seg,
            _fd: fd,
            map,
            len: size,
        })
    }

    pub fn seg(&self) -> u32 {
        self.seg
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Kept alongside `len()` for the usual slice-style API.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The mapped bytes. Valid after a successful `ShmGetImage` into this segment.
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.map.as_ptr(), self.len) }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        // Unmap our view. The server-side attachment is released by `shm_detach` (caller)
        // or, at the latest, when the connection closes and the server frees client
        // resources; the descriptor closes when `_fd` drops.
        unsafe {
            libc::munmap(self.map.as_ptr() as *mut libc::c_void, self.len);
        }
    }
}
