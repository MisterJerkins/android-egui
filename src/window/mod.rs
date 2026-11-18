#[cfg(target_os = "android")]
mod composer;
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod libgui;
mod mirror;
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod native;

#[cfg(target_os = "android")]
pub use composer::{Composer, Layer};
#[cfg(target_os = "android")]
pub use mirror::Mirrors;
#[cfg(target_os = "android")]
pub use native::{Frame, NativeWindow};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

impl Rotation {
    pub fn from_index(index: u8) -> Self {
        match index & 3 {
            0 => Self::Deg0,
            1 => Self::Deg90,
            2 => Self::Deg180,
            _ => Self::Deg270,
        }
    }

    pub fn degrees(self) -> u32 {
        self as u32 * 90
    }
}

/// Whether screenshots and screen recordings see the layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Capture {
    #[default]
    Visible,
    Hidden,
    Secure,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub rotation: Rotation,
    pub density: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Region {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Region {
    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    pub fn area(&self) -> i64 {
        if self.is_empty() { 0 } else { i64::from(self.right - self.left) * i64::from(self.bottom - self.top) }
    }

    pub fn intersect(self, other: Self) -> Self {
        Self { left: self.left.max(other.left), top: self.top.max(other.top), right: self.right.min(other.right), bottom: self.bottom.min(other.bottom) }
    }

    /// The smallest region holding both; an empty one adds nothing.
    pub fn union(self, other: Self) -> Self {
        match (self.is_empty(), other.is_empty()) {
            (true, _) => other,
            (_, true) => self,
            _ => Self { left: self.left.min(other.left), top: self.top.min(other.top), right: self.right.max(other.right), bottom: self.bottom.max(other.bottom) },
        }
    }
}
