pub mod x11;

/// A screen change reported by the capture backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyArea {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

impl DirtyArea {
    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}
