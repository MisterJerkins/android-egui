#[allow(unsafe_code)]
mod egl;

use egui_glow::Painter;

use super::{Frame, Graphics};
use crate::window::NativeWindow;
use crate::{Result, err};

pub struct OpenGl {
    painter: Painter,
    egl: egl::Context,
}

impl OpenGl {
    pub fn new(window: &NativeWindow) -> Result<Self> {
        let egl = egl::Context::new(window.as_ptr())?;
        let painter = Painter::new(egl.gl().clone(), "", None, false).map_err(|error| err!("OpenGL painter failed to start: {error}"))?;
        log::info!("OpenGL ES {} backend ready", egl.version());
        Ok(Self { painter, egl })
    }
}

impl Graphics for OpenGl {
    fn name(&self) -> &'static str {
        "OpenGL ES"
    }

    fn max_texture_side(&self) -> usize {
        self.painter.max_texture_side()
    }

    fn rebind(&mut self, window: &NativeWindow) -> Result<()> {
        self.egl.rebind(window.as_ptr())
    }

    fn render(&mut self, frame: Frame) -> Result<()> {
        self.painter.clear(frame.size, [0.0; 4]);
        self.painter.paint_and_update_textures(frame.size, frame.pixels_per_point, frame.primitives, frame.textures);
        self.egl.swap()
    }
}

impl Drop for OpenGl {
    fn drop(&mut self) {
        self.painter.destroy();
    }
}
