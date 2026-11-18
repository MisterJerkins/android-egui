//! egui overlay for Android, drawn from a root binary through SurfaceFlinger.
//!
//! The crate builds for Android. On a PC only its unit tests run, over the parts that need no
//! device: `cargo test --lib --target <host>`.

#![deny(unsafe_code)]
#![cfg_attr(not(target_os = "android"), allow(dead_code, unused_imports, unused_macros))]

mod draw;
mod graphics;
#[cfg(target_os = "android")]
mod helper;
mod input;
#[cfg(target_os = "android")]
mod overlay;
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod sys;
pub mod window;

pub use draw::{Draw, Texture};
pub use egui;
pub use graphics::Backend;
pub use input::{Keyboard, Language, Touch};
#[cfg(target_os = "android")]
pub use overlay::Overlay;
#[cfg(target_os = "android")]
pub use sys::logger;
pub use window::{Capture, Display, Rotation};

#[derive(Debug)]
pub struct Error(String);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

macro_rules! err {
    ($($arg:tt)*) => {
        $crate::Error::new(format!($($arg)*))
    };
}

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::err!($($arg)*))
    };
}

pub(crate) use {bail, err};
