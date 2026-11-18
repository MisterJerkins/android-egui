use std::path::Path;
use std::time::{Duration, Instant};

use egui::epaint::Primitive;
use egui::{AreaState, ClippedPrimitive, Context, Order, Pos2, RawInput, Rect, TextureId, ViewportCommand, ViewportId, pos2, vec2};

use crate::draw::texture::{self, Texture};
use crate::graphics::{self, Backend, Frame, Graphics};
use crate::helper::{Helper, User};
use crate::input::{self, Input, Keyboard, Touch, View};
use crate::window::{Capture, Composer, Display, Layer, Mirrors};
use crate::{Result, err};

const DISPLAY_POLL: Duration = Duration::from_millis(100);
const IDLE_POLL: Duration = Duration::from_millis(250);
const SHORTEST_FRAME: Duration = Duration::from_millis(4);
const GRAB_MARGIN: f32 = 8.0;
const ACTIVITY_REPORT: Duration = Duration::from_secs(1);
const TOUCHSCREEN_WAIT: Duration = Duration::from_secs(5);
/// Set to `system` to take touches from Android itself even where a touchscreen can be read.
const TOUCH_SOURCE: &str = "ANDROID_EGUI_TOUCH";
const FIRST_API_WITH_INPUT_MONITOR: u32 = 29;

pub struct Overlay {
    context: Context,
    input: Input,
    graphics: Box<dyn Graphics>,
    mirrors: Option<Mirrors>,
    mirroring: bool,
    layer: Layer,
    composer: Composer,
    display: Display,
    name: String,
    capture: Capture,
    started: Instant,
    time: f64,
    polled: Instant,
    frame_at: Instant,
    repaint: Option<Instant>,
    fingerprint: u64,
    shown: bool,
    running: bool,
    events: Vec<input::Event>,
    view: View,
    keyboard: Keyboard,
    key_repeat: Option<Duration>,
    first_frame: bool,
    helper: Helper,
    reported: Instant,
}

impl Overlay {
    pub fn new(name: &str, backend: Backend) -> Result<Self> {
        crate::sys::logger::init();
        let composer = Composer::connect()?;
        let display = composer.display()?;
        let (layer, graphics) = start_graphics(&composer, name, display, backend)?;
        log::info!("{}x{} at {} degrees, Android API {}, {}", display.width, display.height, display.rotation.degrees(), composer.api_level(), graphics.name());

        let input = Input::start(composer.display_id());
        let mut view = View { size: [display.width as f32, display.height as f32], rotation: display.rotation, ..Default::default() };
        input.swap_view(&mut view);
        // The input devices are looked through before anything else happens, so that a touch
        // right after the start is not missed. Without a touchscreen among them, as inside a
        // container, the system is asked for the touches.
        let screen = input.has_screen(TOUCHSCREEN_WAIT);
        let now = Instant::now();
        let mut helper = Helper::new(User::Root);
        let asked = std::env::var(TOUCH_SOURCE).is_ok_and(|source| source == "system");
        if composer.api_level() >= FIRST_API_WITH_INPUT_MONITOR && (asked || !screen) {
            log::info!("touches come from the system's input monitor");
            match Helper::watch(Box::new(input.system_touches(true))) {
                Ok(watching) => helper = watching,
                Err(error) => {
                    log::warn!("no touches from the system: {error}");
                    drop(input.system_touches(false));
                }
            }
        }
        let mut keyboard = Keyboard::default();
        keyboard.share_clipboard(Box::new(Helper::new(User::Shell)));
        Ok(Self {
            context: Context::default(),
            input,
            graphics,
            mirrors: None,
            mirroring: false,
            layer,
            composer,
            display,
            name: name.to_owned(),
            capture: Capture::Visible,
            started: now,
            time: 0.0,
            polled: now,
            frame_at: now.checked_sub(IDLE_POLL).unwrap_or(now),
            repaint: Some(now),
            fingerprint: 0,
            shown: true,
            running: true,
            events: Vec::new(),
            view,
            keyboard,
            key_repeat: None,
            first_frame: true,
            helper,
            reported: now,
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

        let input = self.take_input();
        let mut unused = Rect::NOTHING;
        let (keyboard, key_repeat) = (&self.keyboard, self.key_repeat);
        let mut output = self.context.run_ui(input, |root| {
            ui(root);
            unused = root.available_rect_before_wrap();
            keyboard.paint(root.ctx());
            if let Some(delay) = key_repeat {
                root.ctx().request_repaint_after(delay);
            }
        });
        if let Some(viewport) = output.viewport_output.get(&ViewportId::ROOT) {
            self.repaint = Instant::now().checked_add(viewport.repaint_delay);
            self.running &= !viewport.commands.contains(&ViewportCommand::Close);
        }
        if self.keyboard.finish(&self.context, &mut output.platform_output) {
            self.repaint = Some(Instant::now());
        }
        self.publish_view(unused, output.pixels_per_point);

        let primitives = self.context.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
        let size = [self.display.width, self.display.height];
        let fingerprint = fingerprint(&primitives, size);
        if fingerprint != self.fingerprint || !output.textures_delta.is_empty() {
            self.graphics.render(Frame {
                window: self.layer.window(),
                size,
                pixels_per_point: output.pixels_per_point,
                primitives: &primitives,
                textures: &mut output.textures_delta,
            })?;
            self.fingerprint = fingerprint;
        }

        let visible = !primitives.is_empty();
        if visible != self.shown {
            self.layer.set_visible(visible)?;
            self.shown = visible;
        }
        Ok(self.running)
    }

    pub fn context(&self) -> &Context {
        &self.context
    }

    pub fn display(&self) -> Display {
        self.display
    }

    pub fn backend(&self) -> &'static str {
        self.graphics.name()
    }

    pub fn capture(&self) -> Capture {
        self.capture
    }

    pub fn set_capture(&mut self, capture: Capture) -> Result<()> {
        if capture != self.capture {
            self.capture = capture;
            self.rebuild_layer()?;
        }
        Ok(())
    }

    pub fn mirroring(&self) -> bool {
        self.mirroring
    }

    /// From Android 14 a screen recording misses the overlay unless a copy of it is put on the
    /// recorder's display. That is off to begin with, since keeping the copies up means asking
    /// the system for its displays every second. Nothing is copied while [`Capture`] hides the
    /// overlay, and before Android 14 recordings see it without help.
    pub fn set_mirroring(&mut self, mirroring: bool) {
        if mirroring != self.mirroring {
            self.mirroring = mirroring;
            self.mirrors = if mirroring { self.composer.mirrors() } else { None };
            self.mirror_layer();
        }
    }

    pub fn close(&mut self) {
        self.running = false;
    }

    /// Loads a PNG or JPEG picture, to be shown with `ui.image` or [`crate::Draw::image`].
    /// `name` tells the picture from others in messages.
    pub fn load_image(&self, name: &str, data: &[u8]) -> Result<Texture> {
        let (size, pixels) = texture::decode(name, data)?;
        self.load_pixels(name, size, &pixels)
    }

    /// Loads a PNG or JPEG picture from a file.
    pub fn load_image_file(&self, path: impl AsRef<Path>) -> Result<Texture> {
        let path = path.as_ref();
        self.load_image(&path.to_string_lossy(), &texture::read(path)?)
    }

    /// Loads a picture from RGBA pixels, four bytes each, row after row from the top.
    pub fn load_pixels(&self, name: &str, size: [usize; 2], rgba: &[u8]) -> Result<Texture> {
        texture::load(&self.context, name, size, rgba, self.graphics.max_texture_side())
    }

    pub fn keyboard(&mut self) -> &mut Keyboard {
        &mut self.keyboard
    }

    /// Touch control from code; see [`Touch`].
    pub fn touch(&self) -> Touch {
        self.input.touch()
    }

    /// Input devices in use, one line each.
    pub fn input_devices(&self) -> Vec<String> {
        self.input.describe()
    }

    fn pixels_per_point(&self) -> f32 {
        self.display.density * self.context.zoom_factor()
    }

    fn take_input(&mut self) -> RawInput {
        let scale = 1.0 / self.pixels_per_point();
        let mut events = Vec::new();
        if std::mem::take(&mut self.first_frame) {
            events.push(input::announce_touch_screen());
        }
        let taken = self.input.drain(&mut self.events);
        if taken.kept {
            self.report_activity();
        }
        let happened = taken.at.map(|at| at.saturating_duration_since(self.started).as_secs_f64());
        self.time = input::frame_time(self.time, happened, self.started.elapsed().as_secs_f64());
        let time = self.time;
        for event in self.events.drain(..) {
            match event {
                input::Event::Key { code, pressed, .. } => self.keyboard.key(code, pressed, time, &mut events),
                input::Event::KeyboardTouch { id, phase, position: [x, y] } => self.keyboard.touch(id, phase, pos2(x * scale, y * scale), time, &mut events),
                event => events.extend(input::to_egui(event, scale, self.keyboard.modifiers())),
            }
        }
        self.key_repeat = self.keyboard.tick(time, &mut events).map(|due| Duration::from_secs_f64((due - time).max(0.0)));

        let viewport = Rect::from_min_size(Pos2::ZERO, vec2(self.display.width as f32, self.display.height as f32) * scale);
        self.keyboard.arrange(viewport, self.context.zoom_factor());
        let mut raw =
            RawInput { screen_rect: Some(viewport), max_texture_side: Some(self.graphics.max_texture_side()), time: Some(time), focused: true, events, ..Default::default() };
        raw.viewports.entry(ViewportId::ROOT).or_default().native_pixels_per_point = Some(self.display.density);
        raw
    }

    /// Input the interface kept to itself never reached the system, which would let the screen
    /// time out under the user's fingers. The helper tells it they are there.
    fn report_activity(&mut self) {
        if self.reported.elapsed() >= ACTIVITY_REPORT {
            self.reported = Instant::now();
            self.helper.activity();
        }
    }

    /// Tells the input threads where the interface is, so they can tell its touches from the rest.
    fn publish_view(&mut self, unused: Rect, pixels_per_point: f32) {
        let pixels = |rect: Rect| [rect.min.x * pixels_per_point, rect.min.y * pixels_per_point, rect.max.x * pixels_per_point, rect.max.y * pixels_per_point];
        let (context, view) = (&self.context, &mut self.view);
        let layers = context.memory(|memory| memory.areas().visible_layer_ids());
        // An area knows its place before any transform of its layer, such as the lift above the keyboard.
        let place = |layer: egui::LayerId| Some((AreaState::load(context, layer.id)?, context.layer_transform_to_global(layer).unwrap_or_default()));
        let areas = layers.into_iter().filter(|layer| layer.order != Order::Background).filter_map(place);
        view.regions.clear();
        view.regions.extend(areas.filter(|(area, _)| area.interactable).map(|(area, to_screen)| pixels((to_screen * area.rect()).expand(GRAB_MARGIN))));
        view.size = [self.display.width as f32, self.display.height as f32];
        view.rotation = self.display.rotation;
        view.unused = (unused != context.content_rect()).then(|| pixels(unused));
        view.keyboard = self.keyboard.rect().map(pixels);
        view.dismissable = context.any_popup_open() || context.memory(|memory| memory.focused().is_some());
        view.wants_keys = context.egui_wants_keyboard_input();
        self.input.swap_view(view);
    }

    fn wait(&mut self) -> Result<()> {
        let earliest = self.frame_at + self.composer.vsync_period().unwrap_or(SHORTEST_FRAME);
        let mut pending = false;
        loop {
            let now = Instant::now();
            if now.duration_since(self.polled) >= DISPLAY_POLL {
                self.helper.rest();
                self.keyboard.rest();
                pending |= self.follow_display()?;
            }
            let due = if pending { Some(earliest) } else { self.repaint.map(|at| at.max(earliest)) };
            let pause = due.map_or(IDLE_POLL, |at| at.saturating_duration_since(now)).min(IDLE_POLL);
            if pause.is_zero() {
                break;
            }
            if pending {
                std::thread::sleep(pause);
            } else {
                pending = self.input.wait(pause);
            }
        }
        self.frame_at = Instant::now();
        Ok(())
    }

    fn follow_display(&mut self) -> Result<bool> {
        self.polled = Instant::now();
        let display = self.composer.display()?;
        if display == self.display {
            return Ok(false);
        }
        log::info!("display is now {}x{} at {} degrees", display.width, display.height, display.rotation.degrees());
        self.display = display;
        if self.layer.resize(display.width, display.height)? {
            self.graphics.rebind(self.layer.window())?;
            self.fingerprint = 0;
            self.mirror_layer();
        } else {
            self.rebuild_layer()?;
        }
        self.context.request_repaint();
        Ok(true)
    }

    fn rebuild_layer(&mut self) -> Result<()> {
        let layer = self.composer.create_layer(&self.name, self.display.width, self.display.height, self.capture)?;
        self.graphics.rebind(layer.window())?;
        self.layer = layer;
        self.shown = true;
        self.fingerprint = 0;
        self.repaint = Some(Instant::now());
        self.mirror_layer();
        Ok(())
    }

    /// Recordings are shown the layer when asked to, for as long as it is not hidden from them.
    fn mirror_layer(&mut self) {
        if let Some(mirrors) = &mut self.mirrors {
            let visible = self.capture == Capture::Visible;
            mirrors.follow(visible.then(|| (self.layer.handle(), [self.display.width, self.display.height])));
        }
    }
}

/// Brings up the first backend that works. Each attempt gets a layer of its own: a window that
/// one graphics API has claimed is not given up to another.
fn start_graphics(composer: &Composer, name: &str, display: Display, backend: Backend) -> Result<(Layer, Box<dyn Graphics>)> {
    let mut failure = err!("no graphics backend to try");
    for &candidate in backend.candidates() {
        let layer = composer.create_layer(name, display.width, display.height, Capture::Visible)?;
        match graphics::create(candidate, layer.window()) {
            Ok(graphics) => return Ok((layer, graphics)),
            Err(error) => {
                log::warn!("{candidate:?} backend is unavailable: {error}");
                failure = error;
            }
        }
    }
    Err(failure)
}

fn fingerprint(primitives: &[ClippedPrimitive], size: [u32; 2]) -> u64 {
    const SEED: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut hash = SEED ^ (u64::from(size[0]) << 32 | u64::from(size[1]));
    let mut mix = |bytes: &[u8]| {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            hash = (hash.rotate_left(5) ^ u64::from_le_bytes(*word)).wrapping_mul(SEED);
        }
        for &byte in rest {
            hash = (hash.rotate_left(5) ^ u64::from(byte)).wrapping_mul(SEED);
        }
    };
    for clipped in primitives {
        if let Primitive::Mesh(mesh) = &clipped.primitive {
            let texture = match mesh.texture_id {
                TextureId::Managed(id) => id << 1,
                TextureId::User(id) => id << 1 | 1,
            };
            mix(bytemuck::bytes_of(&[clipped.clip_rect.min.x, clipped.clip_rect.min.y, clipped.clip_rect.max.x, clipped.clip_rect.max.y]));
            mix(&texture.to_le_bytes());
            mix(bytemuck::cast_slice(&mesh.indices));
            mix(bytemuck::cast_slice(&mesh.vertices));
        }
    }
    hash
}
