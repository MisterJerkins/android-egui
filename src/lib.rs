//! egui overlay for Android, drawn from a root binary through SurfaceFlinger.
//!
//! An overlay comes up in three steps: [`ANativeWindowCreator::create`] lays a window over the
//! screen, [`Graphics::create`] starts drawing into it with a graphics API of your choice, and
//! [`Touch::init`] gives it touches and keys.
//!
//! The crate builds for Android. On a PC only its unit tests run, over the parts that need no
//! device: `cargo test --lib --target <host>`.

#![deny(unsafe_code)]
#![cfg_attr(not(target_os = "android"), allow(dead_code, unused_imports, unused_macros))]

mod graphics;
#[cfg(target_os = "android")]
mod helper;
mod input;
mod utils;
pub mod window;

pub use egui;
#[cfg(target_os = "android")]
pub use graphics::Graphics;
pub use graphics::{GraphicsApi, Texture};
pub use input::{Keyboard, Language, Touch};
#[cfg(target_os = "android")]
pub use utils::logger;
pub use utils::{Error, Result};
pub(crate) use utils::{bail, err};
#[cfg(target_os = "android")]
pub use window::{ANativeWindowCreator, NativeWindow};
pub use window::{Capture, Display, Rotation};
