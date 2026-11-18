mod bridge;
mod classify;
mod device;
pub(crate) mod evdev;
mod keyboard;
#[cfg(target_os = "android")]
mod reader;
mod router;
mod touch;

pub use bridge::{announce_touch_screen, frame_time, to_egui};
pub(crate) use keyboard::Clipboard;
pub use keyboard::{Keyboard, Language};
#[cfg(target_os = "android")]
pub use reader::Input;
pub use router::{Event, View};
#[cfg(not(target_os = "android"))]
pub(crate) use touch::Injector;
pub use touch::{Motion, Phase, Touch};
