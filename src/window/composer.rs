use std::cell::Cell;
use std::ffi::CString;
use std::time::Duration;

use super::libgui::{Handle, LibGui, Ptr};
use super::{Capture, Display, Mirrors, NativeWindow, Rotation};
use crate::sys::{choreographer, prop};
use crate::{Result, err};

pub struct Composer {
    gui: &'static LibGui,
    client: Ptr,
    token: Ptr,
    display_id: Option<u64>,
    transaction: Ptr,
    reported_period: Cell<Option<Duration>>,
}

impl Composer {
    pub fn connect() -> Result<Self> {
        let gui = LibGui::get()?;
        let client = gui.connect()?;
        let (token, display_id) = gui.display_token()?;
        let transaction = gui.new_transaction()?;
        Ok(Self { gui, client, token, display_id, transaction, reported_period: Cell::new(None) })
    }

    /// The display's unique id as Android's input configuration names it.
    pub fn display_id(&self) -> Option<String> {
        self.display_id.map(|id| format!("local:{id}"))
    }

    pub fn api_level(&self) -> u32 {
        self.gui.api()
    }

    /// Time between two display refreshes, when the system reports it.
    pub fn vsync_period(&self) -> Option<Duration> {
        choreographer::vsync_period().or(self.reported_period.get())
    }

    pub fn display(&self) -> Result<Display> {
        let state = self.gui.display_state(self.token)?;
        if state.width == 0 || state.height == 0 {
            return Err(err!("SurfaceFlinger reported an empty display"));
        }
        self.reported_period.set(state.refresh_rate.map(|rate| Duration::from_secs_f32(1.0 / rate)));
        Ok(Display { width: state.width, height: state.height, rotation: Rotation::from_index(state.rotation), density: prop::density() })
    }

    pub fn create_layer(&self, name: &str, width: u32, height: u32, capture: Capture) -> Result<Layer> {
        let name = CString::new(name).map_err(|_| err!("layer name contains a NUL byte"))?;
        let control = self.gui.create_surface(self.client, &name, width, height, capture, false)?;
        let mut layer = Layer { gui: self.gui, transaction: self.transaction, control, surface: std::ptr::null_mut(), window: None, width, height };
        let (surface, window) = self.gui.window(control)?;
        window.set_size(width, height)?;
        layer.surface = surface;
        layer.window = Some(window);
        self.gui.present(self.transaction, control, i32::MAX)?;
        Ok(layer)
    }

    /// Starts mirroring to recording displays, where the system needs it and allows it.
    pub fn mirrors(&self) -> Option<Mirrors> {
        Mirrors::start(self.gui, Handle::share(self.gui, self.client))
    }

    pub fn describe(&self) -> Vec<(String, String)> {
        self.gui.picks().iter().map(|(what, symbol)| (what.to_string(), symbol.to_string_lossy().into_owned())).collect()
    }
}

pub struct Layer {
    gui: &'static LibGui,
    transaction: Ptr,
    control: Ptr,
    surface: Ptr,
    window: Option<NativeWindow>,
    width: u32,
    height: u32,
}

impl Layer {
    pub fn window(&self) -> &NativeWindow {
        self.window.as_ref().expect("layer window is alive until drop")
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The layer itself, for mirroring; it stays alive for as long as the handle does.
    pub fn handle(&self) -> Handle {
        Handle::share(self.gui, self.control)
    }

    pub fn set_visible(&self, visible: bool) -> Result<()> {
        self.gui.set_visible(self.transaction, self.control, visible)
    }

    /// Resizes the layer in place. Returns `false` when this Android version cannot do that
    /// and the layer has to be created again.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<bool> {
        if !self.gui.resize(self.transaction, self.control, width, height)? {
            return Ok(false);
        }
        self.window().set_size(width, height)?;
        self.width = width;
        self.height = height;
        Ok(true)
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        let _ = self.set_visible(false);
        self.window = None;
        if !self.surface.is_null() {
            self.gui.release(self.surface);
        }
        self.gui.disconnect(self.control);
        self.gui.release(self.control);
    }
}
