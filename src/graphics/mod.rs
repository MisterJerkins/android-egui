//! Drawing. [`Graphics::create`] picks a graphics API for the window, and the [`Graphics`] it
//! returns runs the frames: input in, egui's picture out to the window.

mod blur;
#[cfg(target_os = "android")]
mod opengl;
mod software;
mod texture;
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod vulkan;

use egui::TextureId;

#[cfg(target_os = "android")]
pub use android::Graphics;
#[cfg(target_os = "android")]
pub(crate) use android::{Backend, Frame};
pub use blur::Blur;
pub use texture::Texture;

/// The graphics API to draw with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GraphicsApi {
    /// The first that works: OpenGL ES, then Vulkan, then the CPU.
    #[default]
    Auto,
    Software,
    OpenGl,
    Vulkan,
}

impl GraphicsApi {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "software" | "cpu" => Some(Self::Software),
            "opengl" | "gl" | "gles" => Some(Self::OpenGl),
            "vulkan" | "vk" => Some(Self::Vulkan),
            _ => None,
        }
    }

    /// The APIs to try, best first.
    pub(crate) fn candidates(self) -> &'static [Self] {
        match self {
            Self::Auto => &[Self::OpenGl, Self::Vulkan, Self::Software],
            Self::Software => &[Self::Software],
            Self::OpenGl => &[Self::OpenGl],
            Self::Vulkan => &[Self::Vulkan],
        }
    }
}

/// A texture's id as one number, for the hashes that tell a changed picture from an unchanged one.
pub(crate) fn texture_key(id: TextureId) -> u64 {
    match id {
        TextureId::Managed(id) => id << 1,
        TextureId::User(id) => id << 1 | 1,
    }
}

#[cfg(target_os = "android")]
mod android {
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use egui::epaint::Primitive;
    use egui::{ClippedPrimitive, Context, Pos2, RawInput, Rect, TexturesDelta, ViewportCommand, ViewportId, vec2};

    use super::texture::{self, Texture};
    use super::{Blur, GraphicsApi, opengl, software, texture_key, vulkan};
    use crate::input::{self, Feed};
    use crate::utils::{self, HASH_SEED};
    use crate::window::{ANativeWindow, Display, NativeWindow};
    use crate::{Result, err};

    const IDLE_POLL: Duration = Duration::from_millis(250);

    pub struct Frame<'a> {
        pub window: &'a ANativeWindow,
        pub size: [u32; 2],
        pub pixels_per_point: f32,
        pub primitives: &'a [ClippedPrimitive],
        pub textures: &'a mut TexturesDelta,
    }

    /// One graphics API, drawing egui's picture into the window.
    pub trait Backend {
        fn name(&self) -> &'static str;
        fn max_texture_side(&self) -> usize;
        fn rebind(&mut self, window: &ANativeWindow) -> Result<()>;
        fn render(&mut self, frame: Frame) -> Result<()>;
    }

    /// Brings up the first backend that works. Each attempt gets a layer of its own: a window that
    /// one graphics API has claimed is not given up to another.
    fn start(window: &NativeWindow, api: GraphicsApi) -> Result<Rc<RefCell<dyn Backend>>> {
        let mut failure = err!("no graphics backend to try");
        for &candidate in api.candidates() {
            window.fresh_layer()?;
            match window.with_native(|native| backend(candidate, native)) {
                Ok(backend) => return Ok(backend),
                Err(error) => {
                    log::warn!("{candidate:?} backend is unavailable: {error}");
                    failure = error;
                }
            }
        }
        Err(failure)
    }

    fn backend(api: GraphicsApi, window: &ANativeWindow) -> Result<Rc<RefCell<dyn Backend>>> {
        match api {
            GraphicsApi::Auto | GraphicsApi::OpenGl => Ok(Rc::new(RefCell::new(opengl::OpenGl::new(window)?))),
            GraphicsApi::Software => Ok(Rc::new(RefCell::new(software::Software::new()))),
            GraphicsApi::Vulkan => Ok(Rc::new(RefCell::new(vulkan::Vulkan::new(window)?))),
        }
    }

    /// The graphics of an overlay: the egui context and the frame loop, input in and egui's
    /// picture out to the window. Without [`crate::Touch::init`] frames get no input, and one is
    /// drawn only when the interface asks to be repainted.
    pub struct Graphics {
        context: Context,
        backend: Rc<RefCell<dyn Backend>>,
        name: &'static str,
        max_texture_side: usize,
        window: NativeWindow,
        /// The window's count of layer changes that the picture has caught up with.
        changes: u64,
        started: Instant,
        time: f64,
        polled: Instant,
        frame_at: Instant,
        repaint: Option<Instant>,
        fingerprint: u64,
        running: bool,
        input: Option<Feed>,
        blur: Blur,
    }

    impl Graphics {
        /// Starts drawing into the window with the graphics API asked for. The second of the
        /// three steps that bring an overlay up; [`crate::Touch::init`] is the third.
        pub fn create(window: &NativeWindow, api: GraphicsApi) -> Result<Self> {
            let backend = start(window, api)?;
            window.bind(&backend);
            let (name, max_texture_side) = (backend.borrow().name(), backend.borrow().max_texture_side());
            let display = window.display();
            log::info!("{}x{} at {} degrees, Android API {}, {name}", display.width, display.height, display.rotation.degrees(), window.api_level());
            let now = Instant::now();
            Ok(Self {
                context: Context::default(),
                backend,
                name,
                max_texture_side,
                window: window.clone(),
                changes: window.changes(),
                started: now,
                time: 0.0,
                polled: now,
                frame_at: now.checked_sub(IDLE_POLL).unwrap_or(now),
                repaint: Some(now),
                fingerprint: 0,
                running: true,
                input: None,
                blur: Blur::new(window.can_blur()),
            })
        }

        pub fn run(mut self, mut ui: impl FnMut(&mut egui::Ui)) -> Result<()> {
            while self.frame(&mut ui)? {}
            Ok(())
        }

        /// Runs one frame. Returns `false` once the overlay has been asked to close.
        pub fn frame(&mut self, mut ui: impl FnMut(&mut egui::Ui)) -> Result<bool> {
            self.wait()?;
            if !self.running {
                return Ok(false);
            }

            let display = self.window.display();
            let raw = self.take_input(display);
            self.blur.new_frame();
            let mut unused = Rect::NOTHING;
            let input = &self.input;
            let mut output = self.context.run_ui(raw, |root| {
                ui(root);
                unused = root.available_rect_before_wrap();
                if let Some(input) = input {
                    input.paint(root.ctx());
                }
            });
            if let Some(viewport) = output.viewport_output.get(&ViewportId::ROOT) {
                self.repaint = Instant::now().checked_add(viewport.repaint_delay);
                self.running &= !viewport.commands.contains(&ViewportCommand::Close);
            }
            if let Some(input) = &mut self.input {
                if input.finish(&self.context, &mut output.platform_output) {
                    self.repaint = Some(Instant::now());
                }
                input.publish(&self.context, display, unused, output.pixels_per_point);
            }

            let size = [display.width, display.height];
            // Before the frame goes to the window, which then has the two shown together.
            let blurred = self.blur.collect(&self.context, output.pixels_per_point, size);
            if let Err(error) = self.window.set_blur(&blurred) {
                // The interface is all there without it: the reason is told and the blur given up.
                log::warn!("{error}");
                self.blur.give_up();
            }

            let primitives = self.context.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
            let fingerprint = fingerprint(&primitives, size);
            if fingerprint != self.fingerprint || !output.textures_delta.is_empty() {
                let (backend, pixels_per_point, textures) = (&self.backend, output.pixels_per_point, &mut output.textures_delta);
                self.window.with_native(|window| backend.borrow_mut().render(Frame { window, size, pixels_per_point, primitives: &primitives, textures }))?;
                self.fingerprint = fingerprint;
            }

            self.window.set_visible(!primitives.is_empty() || !blurred.is_empty())?;
            Ok(self.running)
        }

        pub fn context(&self) -> &Context {
            &self.context
        }

        /// The screen behind the interface, blurred: behind its windows, or behind any rectangle.
        pub fn blur(&self) -> Blur {
            self.blur.clone()
        }

        /// The graphics API in use, by name.
        pub fn name(&self) -> &'static str {
            self.name
        }

        pub fn close(&mut self) {
            self.running = false;
        }

        /// Loads a PNG or JPEG picture, to be shown with `ui.image` or drawn by a painter with
        /// `painter.image(texture.id(), ..)`.
        /// `name` tells the picture from others in messages.
        pub fn load_image(&self, name: &str, data: &[u8]) -> Result<Texture> {
            let (size, pixels) = texture::decode(name, data)?;
            self.load_pixels(name, size, &pixels)
        }

        /// Loads a PNG or JPEG picture from a file.
        pub fn load_image_file(&self, path: impl AsRef<Path>) -> Result<Texture> {
            let path = path.as_ref();
            self.load_image(&path.to_string_lossy(), &utils::read_file(path)?)
        }

        /// Loads a picture from RGBA pixels, four bytes each, row after row from the top.
        pub fn load_pixels(&self, name: &str, size: [usize; 2], rgba: &[u8]) -> Result<Texture> {
            texture::load(&self.context, name, size, rgba, self.max_texture_side)
        }

        pub(crate) fn window(&self) -> &NativeWindow {
            &self.window
        }

        pub(crate) fn has_input(&self) -> bool {
            self.input.is_some()
        }

        /// Gives the frames their input from now on.
        pub(crate) fn set_input(&mut self, input: Feed) {
            self.input = Some(input);
        }

        fn take_input(&mut self, display: Display) -> RawInput {
            let zoom = self.context.zoom_factor();
            let scale = 1.0 / (display.density * zoom);
            let happened = self.input.as_mut().and_then(Feed::take).map(|at| at.saturating_duration_since(self.started).as_secs_f64());
            self.time = input::frame_time(self.time, happened, self.started.elapsed().as_secs_f64());

            let viewport = Rect::from_min_size(Pos2::ZERO, vec2(display.width as f32, display.height as f32) * scale);
            let mut events = Vec::new();
            if let Some(input) = &mut self.input {
                input.events(self.time, scale, viewport, zoom, &mut events);
            }
            let mut raw =
                RawInput { screen_rect: Some(viewport), max_texture_side: Some(self.max_texture_side), time: Some(self.time), focused: true, events, ..Default::default() };
            raw.viewports.entry(ViewportId::ROOT).or_default().native_pixels_per_point = Some(display.density);
            raw
        }

        fn wait(&mut self) -> Result<()> {
            // One frame for each of the display's: a frame begins once the display has taken its
            // turn after the frame before began. Begun sooner it would wait in the window's queue,
            // and hold back every frame after it for as long. Without the display's rhythm to go
            // by, no display is faster than a frame in four milliseconds. Only frames that come
            // one after another are held to that rhythm, so only then is the system asked for it.
            let following = self.frame_at.elapsed() < Duration::from_millis(100);
            let earliest = following.then(|| self.window.vsync()).flatten().and_then(|vsync| {
                let period = vsync.period.as_nanos();
                match self.frame_at.checked_duration_since(vsync.at) {
                    Some(since) => vsync.at.checked_add(Duration::from_nanos(((since.as_nanos() / period + 1) * period) as u64)),
                    None => vsync.at.checked_sub(Duration::from_nanos((vsync.at.duration_since(self.frame_at).as_nanos() / period * period) as u64)),
                }
            });
            let earliest = earliest.unwrap_or(self.frame_at + Duration::from_millis(4));
            let mut pending = false;
            loop {
                let now = Instant::now();
                // Ten times a second the display is asked whether it was turned or resized.
                if now.duration_since(self.polled) >= Duration::from_millis(100) {
                    if let Some(input) = &mut self.input {
                        input.rest();
                    }
                    pending |= self.follow_display()?;
                }
                // A layer that was replaced or resized is empty: it gets a frame at once, and
                // the frame is drawn whether or not the picture has changed.
                let changes = self.window.changes();
                if changes != self.changes {
                    (self.changes, self.fingerprint, self.repaint) = (changes, 0, Some(now));
                }
                let due = if pending { Some(earliest) } else { self.repaint.map(|at| at.max(earliest)) };
                let pause = due.map_or(IDLE_POLL, |at| at.saturating_duration_since(now)).min(IDLE_POLL);
                if pause.is_zero() {
                    break;
                }
                match &self.input {
                    Some(input) if !pending => pending = input.wait(pause),
                    _ => std::thread::sleep(pause),
                }
            }
            self.frame_at = Instant::now();
            Ok(())
        }

        fn follow_display(&mut self) -> Result<bool> {
            self.polled = Instant::now();
            if !self.window.follow_display()? {
                return Ok(false);
            }
            self.context.request_repaint();
            Ok(true)
        }
    }

    impl Drop for Graphics {
        /// The picture leaves the screen before anything else is wound up: the helpers that
        /// input has started take a moment to go.
        fn drop(&mut self) {
            let _ = self.window.set_visible(false);
        }
    }

    fn fingerprint(primitives: &[ClippedPrimitive], size: [u32; 2]) -> u64 {
        let mut hash = HASH_SEED ^ (u64::from(size[0]) << 32 | u64::from(size[1]));
        let mut mix = |bytes: &[u8]| hash = utils::mix_bytes(hash, bytes);
        for clipped in primitives {
            if let Primitive::Mesh(mesh) = &clipped.primitive {
                mix(bytemuck::bytes_of(&[clipped.clip_rect.min.x, clipped.clip_rect.min.y, clipped.clip_rect.max.x, clipped.clip_rect.max.y]));
                mix(&texture_key(mesh.texture_id).to_le_bytes());
                mix(bytemuck::cast_slice(&mesh.indices));
                mix(bytemuck::cast_slice(&mesh.vertices));
            }
        }
        hash
    }
}
