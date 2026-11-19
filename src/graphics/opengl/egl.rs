use std::ffi::{CStr, c_char, c_void};
use std::ptr::null_mut;
use std::sync::Arc;

use egui_glow::glow::{self, HasContext};

use crate::utils::Library;
use crate::{Result, bail};

type Handle = *mut c_void;

const NONE: i32 = 0x3038;
const ALPHA_SIZE: i32 = 0x3021;
const BLUE_SIZE: i32 = 0x3022;
const GREEN_SIZE: i32 = 0x3023;
const RED_SIZE: i32 = 0x3024;
const SURFACE_TYPE: i32 = 0x3033;
const WINDOW_BIT: i32 = 0x0004;
const RENDERABLE_TYPE: i32 = 0x3040;
const OPENGL_ES2_BIT: i32 = 0x0004;
const OPENGL_ES3_BIT: i32 = 0x0040;
const CONTEXT_CLIENT_VERSION: i32 = 0x3098;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetDisplay(native: Handle) -> Handle;
    fn eglInitialize(display: Handle, major: *mut i32, minor: *mut i32) -> u32;
    fn eglChooseConfig(display: Handle, attributes: *const i32, configs: *mut Handle, size: i32, count: *mut i32) -> u32;
    fn eglCreateContext(display: Handle, config: Handle, share: Handle, attributes: *const i32) -> Handle;
    fn eglCreateWindowSurface(display: Handle, config: Handle, window: Handle, attributes: *const i32) -> Handle;
    fn eglMakeCurrent(display: Handle, draw: Handle, read: Handle, context: Handle) -> u32;
    fn eglSwapBuffers(display: Handle, surface: Handle) -> u32;
    fn eglSwapInterval(display: Handle, interval: i32) -> u32;
    fn eglDestroySurface(display: Handle, surface: Handle) -> u32;
    fn eglDestroyContext(display: Handle, context: Handle) -> u32;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
    fn eglGetError() -> i32;
}

pub struct Context {
    display: Handle,
    config: Handle,
    context: Handle,
    surface: Handle,
    gl: Arc<glow::Context>,
    version: u8,
}

impl Context {
    pub fn new(window: *mut c_void) -> Result<Self> {
        unsafe {
            let display = eglGetDisplay(null_mut());
            if display.is_null() || eglInitialize(display, null_mut(), null_mut()) == 0 {
                bail!("EGL is unavailable ({})", error());
            }

            let mut found = None;
            for (version, bit) in [(3u8, OPENGL_ES3_BIT), (2, OPENGL_ES2_BIT)] {
                let attributes = [RED_SIZE, 8, GREEN_SIZE, 8, BLUE_SIZE, 8, ALPHA_SIZE, 8, SURFACE_TYPE, WINDOW_BIT, RENDERABLE_TYPE, bit, NONE];
                let (mut config, mut count) = (null_mut(), 0);
                if eglChooseConfig(display, attributes.as_ptr(), &mut config, 1, &mut count) == 0 || count == 0 {
                    continue;
                }
                let context = eglCreateContext(display, config, null_mut(), [CONTEXT_CLIENT_VERSION, version as i32, NONE].as_ptr());
                if !context.is_null() {
                    found = Some((version, config, context));
                    break;
                }
            }
            let Some((version, config, context)) = found else {
                bail!("no OpenGL ES context could be created ({})", error());
            };

            let surface = eglCreateWindowSurface(display, config, window, null_mut());
            if surface.is_null() || eglMakeCurrent(display, surface, surface, context) == 0 {
                let reason = error();
                eglDestroyContext(display, context);
                bail!("EGL window surface could not be created ({reason})");
            }
            eglSwapInterval(display, 1);

            let gles = Library::open(c"libGLESv2.so").ok();
            let gl = glow::Context::from_loader_function_cstr(|name: &CStr| {
                let address = eglGetProcAddress(name.as_ptr());
                if !address.is_null() {
                    return address;
                }
                gles.as_ref().and_then(|library| library.find(name)).map_or(std::ptr::null(), |symbol| symbol.as_ptr().cast_const())
            });
            // Listing the extensions leaves an error behind on some drivers, which the painter
            // would report as its own. A driver keeps at most a handful of errors waiting to be read.
            for _ in 0..16 {
                if gl.get_error() == glow::NO_ERROR {
                    break;
                }
            }
            Ok(Self { display, config, context, surface, gl: Arc::new(gl), version })
        }
    }

    pub fn gl(&self) -> &Arc<glow::Context> {
        &self.gl
    }

    pub fn version(&self) -> u8 {
        self.version
    }

    pub fn rebind(&mut self, window: *mut c_void) -> Result<()> {
        unsafe {
            eglMakeCurrent(self.display, null_mut(), null_mut(), null_mut());
            eglDestroySurface(self.display, self.surface);
            self.surface = eglCreateWindowSurface(self.display, self.config, window, null_mut());
            if self.surface.is_null() || eglMakeCurrent(self.display, self.surface, self.surface, self.context) == 0 {
                bail!("EGL window surface could not be recreated ({})", error());
            }
            eglSwapInterval(self.display, 1);
        }
        Ok(())
    }

    pub fn swap(&self) -> Result<()> {
        if unsafe { eglSwapBuffers(self.display, self.surface) } == 0 {
            bail!("eglSwapBuffers failed ({})", error());
        }
        Ok(())
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            eglMakeCurrent(self.display, null_mut(), null_mut(), null_mut());
            eglDestroySurface(self.display, self.surface);
            eglDestroyContext(self.display, self.context);
        }
    }
}

fn error() -> String {
    format!("EGL error 0x{:04x}", unsafe { eglGetError() })
}
