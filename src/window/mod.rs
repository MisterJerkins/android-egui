//! The overlay's window: a layer of the system's compositor laid over the whole display.
//! [`ANativeWindowCreator`] makes it, and a [`NativeWindow`] is the handle to it afterwards.

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod libgui;
mod mirror;
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod vsync;

#[cfg(target_os = "android")]
pub use android::{ANativeWindowCreator, NativeWindow, Probe};
#[cfg(target_os = "android")]
pub(crate) use native::ANativeWindow;
#[cfg(target_os = "android")]
pub use native::Buffer;

const FORMAT_RGBA_8888: i32 = 1;

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

/// A part of the layer under which the system blurs the screen, in the layer's own pixels and
/// laid out the way the system's library takes it. Whatever the layer draws there goes on top
/// of the blurred picture, so it shows where the layer is not opaque.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub(crate) struct BlurRegion {
    pub radius: u32,
    /// Top left, top right, bottom left, bottom right.
    pub corners: [f32; 4],
    pub alpha: f32,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[cfg(target_os = "android")]
mod android {
    use std::cell::{Cell, RefCell};
    use std::ffi::CString;
    use std::rc::{Rc, Weak};
    use std::time::Duration;

    use super::libgui::{Handle, LayerKind, LibGui, Ptr};
    use super::mirror::Mirrors;
    use super::native::{ANativeWindow, Buffer};
    use super::vsync::{self, Vsync};
    use super::{BlurRegion, Capture, Display, Rotation};
    use crate::graphics::Backend;
    use crate::utils::{self, logger};
    use crate::{Result, bail, err};

    /// Makes the overlay's window. The first of the three steps that bring an overlay up:
    /// the window, then [`crate::Graphics::create`], then [`crate::Touch::init`].
    pub struct ANativeWindowCreator;

    impl ANativeWindowCreator {
        /// Connects to the system's compositor and lays a see-through layer the size of the
        /// display over everything else on it. `name` is what the system lists the layer under.
        pub fn create(name: &str) -> Result<NativeWindow> {
            logger::init();
            let composer = Composer::connect()?;
            let display = composer.display()?;
            let layer = composer.create_layer(name, display.width, display.height, Capture::Visible)?;
            let state = State {
                mirrors: None,
                mirroring: false,
                layer,
                untouched: true,
                composer,
                display,
                name: name.to_owned(),
                capture: Capture::Visible,
                drawing: None,
                changes: 0,
                blur: Vec::new(),
            };
            Ok(NativeWindow(Rc::new(RefCell::new(state))))
        }

        /// Connects to the system's compositor and makes nothing: for a look at what a window
        /// would be made with on this system.
        pub fn probe() -> Result<Probe> {
            logger::init();
            Composer::connect().map(Probe)
        }
    }

    /// The system's compositor as [`ANativeWindowCreator::probe`] found it.
    pub struct Probe(Composer);

    impl Probe {
        pub fn api_level(&self) -> u32 {
            self.0.api_level()
        }

        /// The functions of the system's library that a window is made with: what each one is,
        /// and the symbol it was found under.
        pub fn symbols(&self) -> Vec<(String, String)> {
            self.0.describe()
        }

        pub fn display(&self) -> Result<Display> {
            self.0.display()
        }

        /// The display's unique id as Android's input configuration names it.
        pub fn display_id(&self) -> Option<String> {
            self.0.display_id()
        }
    }

    /// The overlay's window: the layer over the screen, the display it covers, and what
    /// screenshots and recordings see of it. Cheap to clone, and every clone is the same window.
    #[derive(Clone)]
    pub struct NativeWindow(Rc<RefCell<State>>);

    struct State {
        mirrors: Option<Mirrors>,
        mirroring: bool,
        layer: Layer,
        /// No graphics API has had the layer yet.
        untouched: bool,
        composer: Composer,
        display: Display,
        name: String,
        capture: Capture,
        /// What draws into the layer, to be moved along when the layer is replaced or resized.
        drawing: Option<Weak<RefCell<dyn Backend>>>,
        /// How many times the layer was replaced or resized.
        changes: u64,
        /// The parts of the layer the system blurs under, kept for the layer that replaces this one.
        blur: Vec<BlurRegion>,
    }

    impl NativeWindow {
        /// The display the window covers: its size in pixels, how it is turned, and its density.
        pub fn display(&self) -> Display {
            self.0.borrow().display
        }

        pub fn capture(&self) -> Capture {
            self.0.borrow().capture
        }

        pub fn set_capture(&self, capture: Capture) -> Result<()> {
            let mut state = self.0.borrow_mut();
            if capture != state.capture {
                state.capture = capture;
                state.rebuild_layer()?;
            }
            Ok(())
        }

        pub fn mirroring(&self) -> bool {
            self.0.borrow().mirroring
        }

        /// From Android 14 a screen recording misses the overlay unless a copy of it is put on the
        /// recorder's display. That is off to begin with, since keeping the copies up means asking
        /// the system for its displays every second. Nothing is copied while [`Capture`] hides the
        /// overlay, and before Android 14 recordings see it without help.
        pub fn set_mirroring(&self, mirroring: bool) {
            let state = &mut *self.0.borrow_mut();
            if mirroring != state.mirroring {
                state.mirroring = mirroring;
                state.mirrors = if mirroring { state.composer.mirrors() } else { None };
                state.mirror_layer();
            }
        }

        /// Looks at the display again and, when it was turned or resized, brings the layer in
        /// line with it. Returns whether it has changed. Graphics do this between frames by
        /// themselves; only a window that is drawn on without them has to be asked.
        pub fn follow_display(&self) -> Result<bool> {
            self.0.borrow_mut().follow_display()
        }

        /// Hands `draw` the layer's next buffer to paint on the CPU, and shows it when `draw`
        /// returns. This is for a window without graphics; graphics draw by themselves.
        pub fn lock<T>(&self, draw: impl FnOnce(&mut Buffer<'_>) -> T) -> Result<T> {
            let state = self.0.borrow();
            let mut buffer = state.layer.window().lock(None)?;
            Ok(draw(&mut buffer))
        }

        pub(crate) fn api_level(&self) -> u32 {
            self.0.borrow().composer.api_level()
        }

        pub(crate) fn display_id(&self) -> Option<String> {
            self.0.borrow().composer.display_id()
        }

        /// When the display takes its frames, where the system tells.
        pub(crate) fn vsync(&self) -> Option<Vsync> {
            // An old system says with the display how often it takes a frame, and not when.
            vsync::get().or_else(|| self.0.borrow().composer.reported_period.get().map(Vsync::anytime))
        }

        /// Whether the system blurs what is under a layer: Android 12 and later, on devices
        /// whose compositor is built for it, which it says in this property.
        pub(crate) fn can_blur(&self) -> bool {
            self.0.borrow().composer.gui.can_blur() && utils::property(c"ro.surface_flinger.supports_background_blur").as_deref() == Some("1")
        }

        /// Blurs the screen under the given parts of the layer; none of them, no blur. The system
        /// does the blurring itself, every time what is underneath changes, with nothing drawn
        /// here. To be called before the frame drawn for the change goes to the window: the two
        /// are then shown in the same display frame. What fails is not tried again until the
        /// parts change.
        pub(crate) fn set_blur(&self, regions: &[BlurRegion]) -> Result<()> {
            let state = &mut *self.0.borrow_mut();
            if regions == state.blur {
                // The frame drawn for the last change has gone: those after it are shown as soon as they can be.
                state.layer.stamp(None);
                return Ok(());
            }
            state.blur = regions.to_vec();
            // The system takes a change like this at once, and a frame only after its turn in the
            // window's queue, two of the display's frames later: a blur that moves with what is
            // drawn would run ahead of it. So the change and the frame drawn for it are both told
            // to be shown at one moment that far ahead, which keeps the frame no longer than the
            // queue does anyway.
            let period = vsync::get().map_or(Duration::from_nanos(16_666_667), |vsync| vsync.period);
            let shown_at = vsync::nanos() + 2 * period.as_nanos() as i64;
            state.layer.stamp(Some(shown_at));
            state.layer.set_blur(regions, Some(shown_at))
        }

        /// A number that changes whenever the layer is replaced or resized: what was drawn
        /// before is not on it any more.
        pub(crate) fn changes(&self) -> u64 {
            self.0.borrow().changes
        }

        /// Shows or hides the layer, when it is not that way already.
        pub(crate) fn set_visible(&self, visible: bool) -> Result<()> {
            let mut state = self.0.borrow_mut();
            if visible != state.layer.shown { state.layer.set_visible(visible) } else { Ok(()) }
        }

        /// Lends out the NDK's window of the layer, which graphics APIs draw into.
        pub(crate) fn with_native<T>(&self, draw: impl FnOnce(&ANativeWindow) -> T) -> T {
            draw(self.0.borrow().layer.window())
        }

        /// Gets a layer ready for a graphics API to try: one of its own, since a window that
        /// one API has claimed is not given up to another.
        pub(crate) fn fresh_layer(&self) -> Result<()> {
            let state = &mut *self.0.borrow_mut();
            if state.is_drawn() {
                bail!("the window is drawn into by other graphics");
            }
            if !state.untouched {
                state.rebuild_layer()?;
            }
            state.untouched = false;
            Ok(())
        }

        /// Names what draws into the layer from now on.
        pub(crate) fn bind(&self, backend: &Rc<RefCell<dyn Backend>>) {
            self.0.borrow_mut().drawing = Some(Rc::downgrade(backend));
        }
    }

    impl State {
        fn is_drawn(&self) -> bool {
            self.drawing.as_ref().is_some_and(|backend| backend.strong_count() > 0)
        }

        fn rebind(&self, window: &ANativeWindow) -> Result<()> {
            match self.drawing.as_ref().and_then(Weak::upgrade) {
                Some(backend) => backend.borrow_mut().rebind(window),
                None => Ok(()),
            }
        }

        fn follow_display(&mut self) -> Result<bool> {
            let display = self.composer.display()?;
            if display == self.display {
                return Ok(false);
            }
            log::info!("display is now {}x{} at {} degrees", display.width, display.height, display.rotation.degrees());
            self.display = display;
            if self.layer.resize(display.width, display.height)? {
                self.rebind(self.layer.window())?;
                self.changes += 1;
                self.mirror_layer();
            } else {
                self.rebuild_layer()?;
            }
            Ok(true)
        }

        /// Replaces the layer with a new one, as a change of capture mode takes, and a change of
        /// size on systems that cannot resize a layer in place.
        fn rebuild_layer(&mut self) -> Result<()> {
            let layer = self.composer.create_layer(&self.name, self.display.width, self.display.height, self.capture)?;
            self.rebind(layer.window())?;
            self.layer = layer;
            self.untouched = !self.is_drawn();
            self.changes += 1;
            self.mirror_layer();
            // The new layer starts without the blur the old one had.
            if let Err(error) = self.layer.set_blur(&self.blur, None) {
                log::warn!("{error}");
            }
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

    struct Composer {
        gui: &'static LibGui,
        client: Ptr,
        token: Ptr,
        display_id: Option<u64>,
        transaction: Ptr,
        reported_period: Cell<Option<Duration>>,
    }

    impl Composer {
        fn connect() -> Result<Self> {
            let gui = LibGui::get()?;
            let client = gui.connect()?;
            let (token, display_id) = gui.display_token()?;
            let transaction = gui.new_transaction()?;
            Ok(Self { gui, client, token, display_id, transaction, reported_period: Cell::new(None) })
        }

        fn display_id(&self) -> Option<String> {
            self.display_id.map(|id| format!("local:{id}"))
        }

        fn api_level(&self) -> u32 {
            self.gui.api()
        }

        fn display(&self) -> Result<Display> {
            let state = self.gui.display_state(self.token)?;
            if state.width == 0 || state.height == 0 {
                return Err(err!("SurfaceFlinger reported an empty display"));
            }
            self.reported_period.set(state.refresh_rate.map(|rate| Duration::from_secs_f32(1.0 / rate)));
            Ok(Display { width: state.width, height: state.height, rotation: Rotation::from_index(state.rotation), density: utils::density() })
        }

        fn create_layer(&self, name: &str, width: u32, height: u32, capture: Capture) -> Result<Layer> {
            let name = CString::new(name).map_err(|_| err!("layer name contains a NUL byte"))?;
            let control = self.gui.create_surface(self.client, &name, width, height, capture, LayerKind::Drawn)?;
            let (gui, client, transaction) = (self.gui, self.client, self.transaction);
            let mut layer = Layer { gui, client, transaction, control, capture, surface: std::ptr::null_mut(), window: None, shown: false, stamped: false, blur: None };
            let (surface, window) = self.gui.window(control)?;
            window.set_size(width, height)?;
            layer.surface = surface;
            layer.window = Some(window);
            self.gui.present(self.transaction, control, i32::MAX)?;
            layer.shown = true;
            Ok(layer)
        }

        /// Starts mirroring to recording displays, where the system needs it and allows it.
        fn mirrors(&self) -> Option<Mirrors> {
            Mirrors::start(self.gui, Handle::share(self.gui, self.client))
        }

        fn describe(&self) -> Vec<(String, String)> {
            self.gui.picks().iter().map(|(what, symbol)| (what.to_string(), symbol.to_string_lossy().into_owned())).collect()
        }
    }

    struct Layer {
        gui: &'static LibGui,
        client: Ptr,
        transaction: Ptr,
        control: Ptr,
        capture: Capture,
        surface: Ptr,
        window: Option<ANativeWindow>,
        shown: bool,
        /// Whether the window's next buffers carry a moment to be shown at.
        stamped: bool,
        /// The layer the blur is asked on, once any was asked for.
        blur: Option<Handle>,
    }

    impl Layer {
        fn window(&self) -> &ANativeWindow {
            self.window.as_ref().expect("layer window is alive until drop")
        }

        /// The layer itself, for mirroring; it stays alive for as long as the handle does.
        fn handle(&self) -> Handle {
            Handle::share(self.gui, self.control)
        }

        fn set_visible(&mut self, visible: bool) -> Result<()> {
            self.gui.set_visible(self.transaction, self.control, visible)?;
            self.shown = visible;
            Ok(())
        }

        /// The moment the window's next buffers ask to be shown at, in nanoseconds on the
        /// system's clock; none, as soon as they can be.
        fn stamp(&mut self, shown_at: Option<i64>) {
            if shown_at.is_some() || self.stamped {
                self.window().stamp(shown_at);
                self.stamped = shown_at.is_some();
            }
        }

        /// Asks the system for the blur under the given parts. The layer it is asked on is made
        /// when there first is something to blur.
        fn set_blur(&mut self, regions: &[BlurRegion], shown_at: Option<i64>) -> Result<()> {
            let layer = match &self.blur {
                Some(layer) => layer.as_ptr(),
                None if regions.is_empty() => return Ok(()),
                None => self.blur.insert(self.gui.create_blur_layer(self.client, self.transaction, self.control, self.capture)?).as_ptr(),
            };
            self.gui.set_blur(self.transaction, layer, regions, shown_at)
        }

        /// Resizes the layer in place. Returns `false` when this Android version cannot do that
        /// and the layer has to be created again.
        fn resize(&mut self, width: u32, height: u32) -> Result<bool> {
            if !self.gui.resize(self.transaction, self.control, width, height)? {
                return Ok(false);
            }
            self.window().set_size(width, height)?;
            Ok(true)
        }
    }

    impl Drop for Layer {
        fn drop(&mut self) {
            let _ = self.set_visible(false);
            self.blur = None;
            self.window = None;
            if !self.surface.is_null() {
                self.gui.release(self.surface);
            }
            self.gui.disconnect(self.control);
            self.gui.release(self.control);
        }
    }
}

/// The NDK's side of a layer: the window that graphics APIs draw into, and its buffers.
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod native {
    use std::ffi::{c_int, c_void};
    use std::ptr::NonNull;

    use super::{FORMAT_RGBA_8888, Region};
    use crate::{Result, bail};

    #[repr(C)]
    struct RawBuffer {
        width: i32,
        height: i32,
        stride: i32,
        format: i32,
        bits: *mut c_void,
        reserved: [u32; 6],
    }

    #[link(name = "android")]
    unsafe extern "C" {
        fn ANativeWindow_acquire(window: *mut c_void);
        fn ANativeWindow_release(window: *mut c_void);
        fn ANativeWindow_setBuffersGeometry(window: *mut c_void, width: i32, height: i32, format: i32) -> i32;
        fn ANativeWindow_lock(window: *mut c_void, buffer: *mut RawBuffer, dirty: *mut Region) -> i32;
        fn ANativeWindow_unlockAndPost(window: *mut c_void) -> i32;
    }

    /// The start of an `ANativeWindow` as the system lays it out, up to the call that changes
    /// what its next buffers carry.
    #[repr(C)]
    struct Calls {
        magic: i32,
        version: i32,
        reserved: [*mut c_void; 4],
        inc_ref: *mut c_void,
        dec_ref: *mut c_void,
        flags: u32,
        min_swap_interval: i32,
        max_swap_interval: i32,
        xdpi: f32,
        ydpi: f32,
        oem: [isize; 4],
        set_swap_interval: *mut c_void,
        dequeue_old: *mut c_void,
        lock_old: *mut c_void,
        queue_old: *mut c_void,
        query: *mut c_void,
        perform: unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int,
    }

    pub struct ANativeWindow(NonNull<c_void>);

    impl ANativeWindow {
        /// # Safety
        /// `surface` must point to a live `android::Surface`.
        pub(crate) unsafe fn from_surface(surface: *mut c_void) -> Result<Self> {
            let window = unsafe { surface.byte_add(2 * size_of::<usize>()) };
            // A native window begins with the letters "_wnd".
            if unsafe { window.cast::<u32>().read() } != u32::from_be_bytes(*b"_wnd") {
                bail!("android::Surface has an unexpected layout on this system");
            }
            unsafe { ANativeWindow_acquire(window) };
            Ok(Self(unsafe { NonNull::new_unchecked(window) }))
        }

        pub fn as_ptr(&self) -> *mut c_void {
            self.0.as_ptr()
        }

        /// The moment the window's next buffers ask to be shown at, in nanoseconds on the
        /// system's clock; none, as soon as they can be.
        pub(crate) fn stamp(&self, shown_at: Option<i64>) {
            // `NATIVE_WINDOW_SET_BUFFERS_TIMESTAMP`, with `NATIVE_WINDOW_TIMESTAMP_AUTO` for none.
            unsafe { (self.0.cast::<Calls>().as_ref().perform)(self.0.as_ptr(), 7, shown_at.unwrap_or(i64::MIN)) };
        }

        pub fn set_size(&self, width: u32, height: u32) -> Result<()> {
            let status = unsafe { ANativeWindow_setBuffersGeometry(self.as_ptr(), width as i32, height as i32, FORMAT_RGBA_8888) };
            if status != 0 {
                bail!("ANativeWindow_setBuffersGeometry failed (status {status})");
            }
            Ok(())
        }

        /// Locks the next buffer for CPU drawing. `dirty` is the area the caller intends to redraw;
        /// the returned buffer reports the area that actually has to be redrawn.
        pub fn lock(&self, dirty: Option<Region>) -> Result<Buffer<'_>> {
            let mut raw = RawBuffer { width: 0, height: 0, stride: 0, format: 0, bits: std::ptr::null_mut(), reserved: [0; 6] };
            let mut region = dirty.unwrap_or_default();
            let region_ptr = if dirty.is_some() { &raw mut region } else { std::ptr::null_mut() };
            let status = unsafe { ANativeWindow_lock(self.as_ptr(), &mut raw, region_ptr) };
            if status != 0 || raw.bits.is_null() {
                bail!("ANativeWindow_lock failed (status {status})");
            }
            if dirty.is_none() {
                region = Region { left: 0, top: 0, right: raw.width, bottom: raw.height };
            }
            Ok(Buffer {
                window: self,
                pixels: raw.bits.cast(),
                width: raw.width.max(0) as usize,
                height: raw.height.max(0) as usize,
                stride: raw.stride.max(0) as usize,
                dirty: region,
            })
        }
    }

    impl Drop for ANativeWindow {
        fn drop(&mut self) {
            unsafe { ANativeWindow_release(self.as_ptr()) };
        }
    }

    /// A buffer of the window, locked for drawing on the CPU. It is shown when it is dropped.
    pub struct Buffer<'a> {
        window: &'a ANativeWindow,
        pixels: *mut [u8; 4],
        width: usize,
        height: usize,
        stride: usize,
        dirty: Region,
    }

    impl Buffer<'_> {
        pub fn width(&self) -> usize {
            self.width
        }

        pub fn height(&self) -> usize {
            self.height
        }

        pub fn dirty(&self) -> Region {
            self.dirty
        }

        /// Every row of premultiplied RGBA pixels, and how many pixels apart their starts are.
        pub fn pixels(&mut self) -> (&mut [[u8; 4]], usize) {
            assert!(self.width <= self.stride);
            let length = if self.height == 0 { 0 } else { (self.height - 1) * self.stride + self.width };
            (unsafe { std::slice::from_raw_parts_mut(self.pixels, length) }, self.stride)
        }

        /// One row of premultiplied RGBA pixels.
        pub fn row(&mut self, y: usize) -> &mut [[u8; 4]] {
            assert!(y < self.height && self.width <= self.stride);
            unsafe { std::slice::from_raw_parts_mut(self.pixels.add(y * self.stride), self.width) }
        }
    }

    impl Drop for Buffer<'_> {
        fn drop(&mut self) {
            unsafe { ANativeWindow_unlockAndPost(self.window.as_ptr()) };
        }
    }
}
