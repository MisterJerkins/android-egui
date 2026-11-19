//! The threads that read the input devices, the control they can take of touches, and the
//! frames' end of everything they read.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use egui::{AreaState, Context, Order, PlatformOutput, Rect, pos2};

use super::device::{
    ABS_COUNT, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, AxisState, BTN_TOUCH, Capabilities, Confidence, DeviceParser, EV_ABS, EV_KEY, EV_SYN, Idc,
    PROP_DIRECT, Protocol, RawEvent, Roles, SYN_DROPPED, SYN_REPORT, Sample, classify, kernel,
};
use super::keyboard::Keyboard;
use super::router::{Event, Queue, Router, Target, View};
use super::touch::{Devices, Fingers, Motion, Owner, Phase, RANGE, SLOTS, Touch, VIRTUAL_TOUCHSCREEN, pointer_to_screen, screen_to_touch, touch_to_screen};
use super::{announce_touch_screen, to_egui};
use crate::graphics::Graphics;
use crate::helper::{Helper, User};
use crate::utils::{self, Watch};
use crate::window::Display;
use crate::{Result, bail, err};

impl Touch {
    /// Starts reading touches, mice and keyboards and gives the frames of `graphics` their input.
    /// The last of the three steps that bring an overlay up. Touches are only read: whatever is
    /// under the overlay keeps them, until [`Touch::set_control`] says otherwise.
    pub fn init(graphics: &mut Graphics) -> Result<Self> {
        if graphics.has_input() {
            bail!("touch is set up for these graphics already");
        }
        let (display, display_id, api_level) = (graphics.window().display(), graphics.window().display_id(), graphics.window().api_level());
        let hub = Hub::start(display_id);
        let mut view = View { size: [display.width as f32, display.height as f32], rotation: display.rotation, ..Default::default() };
        hub.swap_view(&mut view);
        // The input devices are looked through before anything else happens, so that a touch
        // right after the start is not missed. Without a touchscreen among them, as inside a
        // container, the system is asked for the touches.
        let screen = hub.has_screen(Duration::from_secs(5));
        let now = Instant::now();
        let mut helper = Helper::new(User::Root);
        // `ANDROID_EGUI_TOUCH=system` takes touches from Android itself even where a touchscreen can be read.
        let asked = std::env::var("ANDROID_EGUI_TOUCH").is_ok_and(|source| source == "system");
        // Android has had an input monitor since version 10.
        if api_level >= 29 && (asked || !screen) {
            log::info!("touches come from the system's input monitor");
            match Helper::watch(Box::new(hub.system_touches(true))) {
                Ok(watching) => helper = watching,
                Err(error) => {
                    log::warn!("no touches from the system: {error}");
                    drop(hub.system_touches(false));
                }
            }
        }
        let mut keyboard = Keyboard::default();
        keyboard.share_clipboard(Box::new(Helper::new(User::Shell)));
        let keyboard = Arc::new(Mutex::new(keyboard));
        graphics.set_input(Feed { hub: hub.clone(), keyboard: keyboard.clone(), helper, reported: now, events: Vec::new(), view, first_frame: true, key_repeat: None });
        Ok(Self::new(hub, keyboard))
    }
}

/// The frames' end of the input: what [`Touch::init`] hands the graphics, which ask it for the input
/// of every frame and tell it where the interface was drawn.
pub struct Feed {
    hub: Arc<Hub>,
    keyboard: Arc<Mutex<Keyboard>>,
    helper: Helper,
    reported: Instant,
    events: Vec<Event>,
    view: View,
    first_frame: bool,
    key_repeat: Option<Duration>,
}

impl Feed {
    /// Blocks until input is waiting or `timeout` passes. Returns whether input is waiting.
    pub fn wait(&self, timeout: Duration) -> bool {
        self.hub.wait(timeout)
    }

    /// Called now and then between frames, to let go of helpers nobody has needed for a while.
    pub fn rest(&mut self) {
        self.helper.rest();
        utils::lock(&self.keyboard).rest();
    }

    /// Takes the input for one frame from the device threads. Returns when the last of it happened.
    pub fn take(&mut self) -> Option<Instant> {
        let taken = self.hub.drain(&mut self.events);
        if taken.kept {
            self.report_activity();
        }
        taken.at
    }

    /// Adds the input taken for the frame to `events`, the way egui takes it: in points, with
    /// keys and touches on the on-screen keyboard already turned into text. `time` is the frame's.
    pub fn events(&mut self, time: f64, scale: f32, viewport: Rect, zoom: f32, events: &mut Vec<egui::Event>) {
        if std::mem::take(&mut self.first_frame) {
            events.push(announce_touch_screen());
        }
        let mut keyboard = utils::lock(&self.keyboard);
        for event in self.events.drain(..) {
            match event {
                Event::Key { code, pressed, .. } => keyboard.key(code, pressed, time, events),
                Event::KeyboardTouch { id, phase, position: [x, y] } => keyboard.touch(id, phase, pos2(x * scale, y * scale), time, events),
                event => events.extend(to_egui(event, scale, keyboard.modifiers())),
            }
        }
        self.key_repeat = keyboard.tick(time, events).map(|due| Duration::from_secs_f64((due - time).max(0.0)));
        keyboard.arrange(viewport, zoom);
    }

    /// Draws the on-screen keyboard on top of the frame, and has a held key repeat in time.
    pub fn paint(&self, context: &Context) {
        utils::lock(&self.keyboard).paint(context);
        if let Some(delay) = self.key_repeat {
            context.request_repaint_after(delay);
        }
    }

    /// Takes what the frame asked of the platform. Returns whether another frame is due.
    pub fn finish(&mut self, context: &Context, output: &mut PlatformOutput) -> bool {
        utils::lock(&self.keyboard).finish(context, output)
    }

    /// Tells the input threads where the interface is, so they can tell its touches from the rest.
    pub fn publish(&mut self, context: &Context, display: Display, unused: Rect, pixels_per_point: f32) {
        let pixels = |rect: Rect| [rect.min.x * pixels_per_point, rect.min.y * pixels_per_point, rect.max.x * pixels_per_point, rect.max.y * pixels_per_point];
        let view = &mut self.view;
        let layers = context.memory(|memory| memory.areas().visible_layer_ids());
        // An area knows its place before any transform of its layer, such as the lift above the keyboard.
        let place = |layer: egui::LayerId| Some((AreaState::load(context, layer.id)?, context.layer_transform_to_global(layer).unwrap_or_default()));
        let areas = layers.into_iter().filter(|layer| layer.order != Order::Background).filter_map(place);
        view.regions.clear();
        // A window can be taken hold of a little outside its edge, so it counts as eight points larger all round.
        view.regions.extend(areas.filter(|(area, _)| area.interactable).map(|(area, to_screen)| pixels((to_screen * area.rect()).expand(8.0))));
        view.size = [display.width as f32, display.height as f32];
        view.rotation = display.rotation;
        view.unused = (unused != context.content_rect()).then(|| pixels(unused));
        view.keyboard = utils::lock(&self.keyboard).rect().map(pixels);
        view.dismissable = context.any_popup_open() || context.memory(|memory| memory.focused().is_some());
        view.wants_keys = context.egui_wants_keyboard_input();
        self.hub.swap_view(view);
    }

    /// Input the interface kept to itself never reached the system, which would let the screen
    /// time out under the user's fingers. The helper tells it they are there, once a second at most.
    fn report_activity(&mut self) {
        if self.reported.elapsed() >= Duration::from_secs(1) {
            self.reported = Instant::now();
            self.helper.activity();
        }
    }
}

/// Where event devices live; the second place is where containers put the ones they create.
const DEVICES: [&str; 2] = ["/dev/input", "/dev"];

struct Device {
    path: String,
    name: String,
    roles: Roles,
    parser: DeviceParser,
    slots: usize,
    screen: bool,
    file: Arc<File>,
    axes: Vec<u16>,
    keys_down: u32,
    grabbed: bool,
    refused: bool,
    /// What the axes held when the device was grabbed, which is how Android remembers them.
    left: Option<AxisState>,
}

impl Device {
    /// Takes the device away from Android, or gives it back the way Android last saw it.
    fn grab(&mut self, grab: bool) -> std::io::Result<()> {
        if !grab {
            self.rewind();
        }
        kernel::grab(&self.file, grab)?;
        let state = kernel::axis_state(&self.file, &self.axes, self.slots);
        // A finger that came down just before the grab is Android's, which saw it land.
        if grab && self.parser.touching(&state, kernel::key_down(&self.file, BTN_TOUCH)) {
            return kernel::grab(&self.file, false);
        }
        self.grabbed = grab;
        self.left = grab.then_some(state);
        Ok(())
    }

    fn rewind(&mut self) {
        let Some(left) = self.left.take() else { return };
        let (mut events, mut bytes) = (Vec::new(), Vec::new());
        kernel::axis_state(&self.file, &self.axes, self.slots).rewind(&left, &mut events);
        events.iter().for_each(|event| event.encode(kernel::TIME_SIZE, &mut bytes));
        if !bytes.is_empty()
            && let Err(error) = OpenOptions::new().write(true).open(&self.path).and_then(|mut device| device.write_all(&bytes))
        {
            log::debug!("{}: cannot rewind: {error}", self.path);
        }
    }
}

/// The virtual touchscreen that stands in for the real one while touches are under control.
struct Control {
    device: File,
    fingers: Fingers,
    ready: Duration,
    events: Vec<RawEvent>,
    bytes: Vec<u8>,
}

impl Control {
    fn send(&mut self, owner: Owner, phase: Phase, at: (f32, f32)) -> bool {
        let changed = self.fingers.apply(owner, phase, at, &mut self.events);
        self.flush();
        changed
    }

    fn flush(&mut self) {
        self.bytes.clear();
        for event in self.events.drain(..) {
            event.encode(kernel::TIME_SIZE, &mut self.bytes);
        }
        if !self.bytes.is_empty()
            && let Err(error) = (&self.device).write_all(&self.bytes)
        {
            log::warn!("virtual touchscreen: {error}");
        }
    }
}

/// Where input goes once a device has reported it.
#[derive(Default)]
struct Routing {
    view: View,
    router: Router,
    queue: Queue,
    events: Vec<Event>,
    control: Option<Control>,
    controlling: bool,
    /// Input has gone to the interface alone since this was last asked.
    kept: bool,
    /// Touches come from the system, and those of touch devices are left unread.
    system: bool,
}

#[derive(Default)]
struct State {
    devices: HashMap<u32, Device>,
    samples: Vec<Sample>,
    routing: Routing,
    /// Every device that was there at the start has been looked at.
    scanned: bool,
}

/// Reads touch, mouse and keyboard devices, one thread per device. Devices are opened for
/// reading and left alone, so whatever is under the overlay keeps its input, until [`Touch`]
/// is told to take control.
struct Hub {
    state: Mutex<State>,
    wake: Condvar,
    started: Instant,
}

/// What [`Hub::drain`] says about the input it handed over.
struct Taken {
    /// When the last of it happened.
    at: Option<Instant>,
    /// Whether some of the input since the last call was kept from the system, which then
    /// does not know the user is there.
    kept: bool,
}

impl Hub {
    /// `display` is the unique id of the display the overlay is on, for systems that tie each
    /// touchscreen to one display.
    fn start(display: Option<String>) -> Arc<Self> {
        let hub = Arc::new(Self { state: Mutex::new(State::default()), wake: Condvar::new(), started: Instant::now() });
        let scanner = hub.clone();
        let _ = utils::spawn("input-scan", move || scan(&scanner, display.as_deref()));
        hub
    }

    /// Hands over the latest view and takes the previous one back, so its buffers are reused.
    fn swap_view(&self, view: &mut View) {
        std::mem::swap(&mut utils::lock(&self.state).routing.view, view);
    }

    /// Whether there is a touchscreen among the input devices. The answer waits until all of
    /// them have been looked at once, which a busy system can take its time over, but for no
    /// longer than `patience`.
    fn has_screen(&self, patience: Duration) -> bool {
        let until = Instant::now() + patience;
        loop {
            let state = utils::lock(&self.state);
            if state.devices.values().any(|device| device.screen) {
                return true;
            }
            if state.scanned || Instant::now() >= until {
                return false;
            }
            drop(state);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Switches between touches reported by the system and those of touch devices, and
    /// returns where to send the system's.
    fn system_touches(self: &Arc<Self>, enabled: bool) -> impl FnMut(Motion) + Send + 'static {
        let hub = self.clone();
        utils::lock(&hub.state).routing.system = enabled;
        move |motion| {
            let mut state = utils::lock(&hub.state);
            let Routing { view, router, queue, events, .. } = &mut state.routing;
            for (contact, position) in motion.contacts() {
                // Touches reported by the system go under the device number zero.
                router.touch(0, contact, position, view, events);
            }
            let now = Instant::now();
            events.drain(..).for_each(|event| queue.push(event, now));
            hub.wake.notify_one();
        }
    }

    /// Blocks until input is waiting or `timeout` passes. Returns whether input is waiting.
    fn wait(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        let mut state = utils::lock(&self.state);
        loop {
            let now = self.started.elapsed();
            let deadline = state.tick(now);
            if !state.routing.queue.is_empty() {
                return true;
            }
            let remaining = until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let pause = deadline.map_or(remaining, |at| remaining.min(at.saturating_sub(now)));
            state = self.wake.wait_timeout(state, pause.max(Duration::from_millis(1))).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Takes the input for one frame; whatever is left makes [`Hub::wait`] return at once.
    fn drain(&self, into: &mut Vec<Event>) -> Taken {
        let mut state = utils::lock(&self.state);
        let at = state.routing.queue.take_frame(into);
        Taken { at, kept: std::mem::take(&mut state.routing.kept) }
    }
}

impl Routing {
    /// Routes what a device just reported: to the interface, and in control mode on to Android.
    fn dispatch(&mut self, index: u32, device: &mut Device, samples: &mut Vec<Sample>) {
        let Self { view, router, queue, events, control, kept, system, .. } = self;
        *kept |= device.grabbed && !samples.is_empty();
        for sample in samples.drain(..) {
            match sample {
                Sample::Contact(contact) => {
                    let Some(panel) = device.roles.touch.filter(|_| device.screen && !*system) else { continue };
                    let position = touch_to_screen(contact.u, contact.v, &panel, view.size, view.rotation);
                    let target = router.touch(index, contact, position, view, events);
                    let passes = target == Target::Outside && (device.grabbed || contact.phase != Phase::Start);
                    if let Some(control) = control.as_mut().filter(|_| passes) {
                        control.send(Owner::Panel { device: index, contact: contact.id }, contact.phase, screen_to_touch(position, view.size, view.rotation));
                        *kept = false;
                    }
                }
                Sample::Hover { u, v } => router.hover(pointer_to_screen(u, v, view.size), view, events),
                Sample::Button { button, pressed } => router.button(button, pressed, view, events),
                Sample::Wheel { x, y } => router.wheel(x, y, view, events),
                Sample::Key { code, pressed, repeat } => {
                    if !repeat {
                        device.keys_down = if pressed { device.keys_down + 1 } else { device.keys_down.saturating_sub(1) };
                    }
                    router.key(code, pressed, repeat, view, events);
                }
            }
        }
        let now = Instant::now();
        for event in events.drain(..) {
            queue.push(event, now);
        }
    }
}

impl State {
    fn tick(&mut self, now: Duration) -> Option<Duration> {
        let Self { devices, samples, routing, .. } = self;
        let mut next: Option<Duration> = None;
        for (&index, device) in devices.iter_mut() {
            if device.parser.deadline().is_some_and(|at| now >= at) {
                device.parser.tick(now, samples);
                routing.dispatch(index, device, samples);
            }
            next = [next, device.parser.deadline()].into_iter().flatten().min();
        }
        self.sync_control(now);
        let settling = self.routing.control.as_ref().map(|control| control.ready).filter(|&ready| self.routing.controlling && now < ready);
        [next, settling].into_iter().flatten().min()
    }

    /// Brings every device's grab in line with the mode. A device changes hands only while
    /// nothing on it is held down, so neither side is left with a stuck finger or key.
    fn sync_control(&mut self, now: Duration) {
        let Routing { view, control, controlling, system, .. } = &mut self.routing;
        let Some(active) = control else { return };
        let ready = *controlling && now >= active.ready;
        let mut grabbed = false;
        for (index, device) in &mut self.devices {
            // A screen whose touches come by way of the system has to stay with the system.
            let wanted = ready && !device.refused && ((device.screen && !*system) || (device.roles.keyboard && view.wants_keys));
            let settled = device.parser.is_idle() && device.keys_down == 0 && !active.fingers.holds_panel(*index);
            if device.grabbed != wanted
                && settled
                && let Err(error) = device.grab(wanted)
            {
                log::warn!("{} stays shared: {error}", device.path);
                device.refused = true;
            }
            grabbed |= device.grabbed;
        }
        if !*controlling {
            active.fingers.lift(|owner| matches!(owner, Owner::Code(_)), &mut active.events);
            active.flush();
            if !grabbed {
                *control = None;
            }
        }
    }

    /// Only surfaces known to be the screen take part; guesses are a fallback for systems
    /// that describe none.
    fn pick_screens(&mut self) {
        let best = self.devices.values().filter_map(|device| device.roles.touch.map(|panel| panel.confidence)).max();
        for device in self.devices.values_mut() {
            device.screen = device.roles.touch.is_some_and(|panel| panel.confidence == Confidence::Certain || Some(panel.confidence) == best);
        }
    }
}

impl Devices for Hub {
    fn set_control(&self, enabled: bool) -> Result<()> {
        let mut state = utils::lock(&self.state);
        let now = self.started.elapsed();
        if enabled && state.routing.control.is_none() {
            let device = virtual_touchscreen(VIRTUAL_TOUCHSCREEN, RANGE, SLOTS).map_err(|error| err!("cannot create a virtual touchscreen: {error}"))?;
            // Android gets half a second to pick up the virtual touchscreen before the real one is taken away.
            state.routing.control = Some(Control { device, fingers: Fingers::default(), ready: now + Duration::from_millis(500), events: Vec::new(), bytes: Vec::new() });
        }
        state.routing.controlling = enabled;
        state.sync_control(now);
        self.wake.notify_one();
        Ok(())
    }

    fn is_control(&self) -> bool {
        utils::lock(&self.state).routing.controlling
    }

    fn finger(&self, finger: u32, phase: Phase, position: [f32; 2]) -> Result<()> {
        let mut state = utils::lock(&self.state);
        let settling = state.routing.control.as_ref().map_or(Duration::ZERO, |control| control.ready.saturating_sub(self.started.elapsed()));
        if !settling.is_zero() {
            drop(state);
            std::thread::sleep(settling);
            state = utils::lock(&self.state);
        }
        let Routing { view, control, controlling, .. } = &mut state.routing;
        let Some(control) = control.as_mut().filter(|_| *controlling) else { bail!("touch control is off") };
        match (control.send(Owner::Code(finger), phase, screen_to_touch(position, view.size, view.rotation)), phase) {
            (true, _) => Ok(()),
            (false, Phase::Start) => bail!("every touch slot is taken"),
            (false, _) => bail!("finger {finger} is not down"),
        }
    }

    fn describe(&self) -> Vec<String> {
        let state = utils::lock(&self.state);
        let mut lines: Vec<String> = state
            .devices
            .values()
            .map(|device| {
                let mut roles = Vec::new();
                if let Some(panel) = device.roles.touch {
                    roles.push(format!("touch {:?} {}x{}{}", panel.protocol, panel.x.max, panel.y.max, if device.screen { " (screen)" } else { "" }));
                }
                for (role, held) in [("pointer", device.roles.pointer.is_some()), ("keyboard", device.roles.keyboard), ("wheel", device.roles.wheel), ("grabbed", device.grabbed)] {
                    roles.extend(held.then(|| role.to_owned()));
                }
                format!("{} \"{}\": {}", device.path, device.name, roles.join(", "))
            })
            .collect();
        lines.sort();
        lines
    }
}

fn scan(hub: &Arc<Hub>, display: Option<&str>) {
    let mut seen: HashMap<String, (u64, i64)> = HashMap::new();
    let mut next_index = 0u32;
    // Set up before the first look, so a device that appears during it is not missed.
    let mut watch = Watch::new(&DEVICES);
    loop {
        let entries = DEVICES.iter().flat_map(|directory| fs::read_dir(directory).into_iter().flatten().flatten());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Only real event devices, which are character devices of major number 13: a pipe or
            // socket under the same name would lose what is read from it.
            let node = |meta: &fs::Metadata| meta.file_type().is_char_device() && meta.rdev() >> 8 & 0xfff == 13;
            let Some(identity) = entry.metadata().ok().filter(|meta| name.starts_with("event") && node(meta)).map(|meta| (meta.ino(), meta.ctime())) else {
                continue;
            };
            if seen.insert(name.clone(), identity) == Some(identity) {
                continue;
            }
            let Some((file, device)) = open(&entry.path(), display) else { continue };
            next_index += 1;
            let index = next_index;
            log::debug!("input device {name}: {} {:?}", device.name, device.roles);

            let mut state = utils::lock(&hub.state);
            state.devices.insert(index, device);
            state.pick_screens();
            restore(&mut state, index, &file);
            state.sync_control(hub.started.elapsed());
            drop(state);

            let reader = hub.clone();
            let _ = utils::spawn(format!("input-{name}"), move || read(&reader, index, &file));
        }
        utils::lock(&hub.state).scanned = true;
        // Devices come and go rarely, and their directories tell when: no looking in between.
        let changed = watch.as_mut().is_some_and(Watch::wait);
        // A device that has just appeared gets a moment for its owner and mode to be set. Where
        // the directories cannot be watched, the devices are looked through once a second.
        std::thread::sleep(if changed { Duration::from_millis(200) } else { Duration::from_secs(1) });
    }
}

fn open(path: &std::path::Path, display: Option<&str>) -> Option<(Arc<File>, Device)> {
    let file = Arc::new(File::open(path).ok()?);
    let capabilities = kernel::capabilities(&file).filter(|device| device.name != VIRTUAL_TOUCHSCREEN)?;
    let roles = classify(&capabilities, &load_idc(&capabilities), display);
    if roles.is_empty() {
        return None;
    }
    let parser = DeviceParser::new(&capabilities, &roles);
    // No more than 32 slots of a panel are followed.
    let slots = capabilities.axis(ABS_MT_SLOT).map_or(0, |axis| axis.max as usize + 1).min(32);
    let axes = (0u16..).zip(&capabilities.axes).filter_map(|(code, axis)| axis.map(|_| code)).collect();
    let path = path.display().to_string();
    Some((file.clone(), Device { path, name: capabilities.name, roles, parser, slots, screen: false, file, axes, keys_down: 0, grabbed: false, refused: false, left: None }))
}

fn load_idc(device: &Capabilities) -> Idc {
    let names = Idc::file_names(device);
    ["/product/usr/idc", "/system_ext/usr/idc", "/odm/usr/idc", "/vendor/usr/idc", "/system/usr/idc", "/data/system/devices/idc"]
        .iter()
        .flat_map(|directory| names.iter().map(move |name| format!("{directory}/{name}")))
        .find_map(|path| fs::read_to_string(path).ok())
        .map(|text| Idc::parse(&text))
        .unwrap_or_default()
}

/// Picks up fingers that were already down when the device was opened or lost events.
fn restore(state: &mut State, index: u32, file: &File) {
    let State { devices, samples, routing, .. } = state;
    let Some(device) = devices.get_mut(&index) else { return };
    if device.slots == 0 || !device.roles.touch.is_some_and(|panel| panel.protocol == Protocol::Slots) {
        return;
    }
    let mut contacts = Vec::new();
    if let Some(parser) = device.parser.touch_mut() {
        parser.restore_slots(&kernel::axis_state(file, &device.axes, device.slots), kernel::key_down(file, BTN_TOUCH), &mut contacts);
    }
    samples.extend(contacts.into_iter().map(Sample::Contact));
    routing.dispatch(index, device, samples);
}

fn read(hub: &Hub, index: u32, mut file: &File) {
    let mut bytes = [0u8; (kernel::TIME_SIZE + 8) * 64];
    let mut dropped = false;
    loop {
        let length = match file.read(&mut bytes) {
            Ok(0) => break,
            Ok(length) => length,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let now = hub.started.elapsed();
        let mut state = utils::lock(&hub.state);
        let State { devices, samples, routing, .. } = &mut *state;
        let Some(device) = devices.get_mut(&index) else { return };
        let mut resync = false;
        for event in RawEvent::decode(&bytes[..length], kernel::TIME_SIZE) {
            if event.kind == EV_SYN && event.code == SYN_DROPPED {
                dropped = true;
                device.parser.cancel(samples);
            } else if !dropped {
                device.parser.feed(event, now, samples);
            } else if event.kind == EV_SYN && event.code == SYN_REPORT {
                dropped = false;
                resync = true;
            }
        }
        routing.dispatch(index, device, samples);
        let waiting = device.parser.deadline().is_some();
        if resync {
            restore(&mut state, index, file);
        }
        state.sync_control(now);
        if waiting || !state.routing.queue.is_empty() {
            hub.wake.notify_one();
        }
    }

    let mut state = utils::lock(&hub.state);
    let State { devices, samples, routing, .. } = &mut *state;
    if let Some(mut device) = devices.remove(&index) {
        device.parser.cancel(samples);
        routing.dispatch(index, &mut device, samples);
    }
    state.pick_screens();
    state.sync_control(hub.started.elapsed());
    hub.wake.notify_one();
}

const NAME_SIZE: usize = 80;
const BUS_VIRTUAL: u16 = 0x06;
/// The letter that requests to the kernel's maker of input devices go under, and its requests.
const UINPUT: u8 = b'U';
const DEVICE_CREATE: u32 = 1;
const SET_EVENT: u32 = 100;
const SET_KEY: u32 = 101;
const SET_ABSOLUTE: u32 = 103;
const SET_PROPERTY: u32 = 110;

/// Creates a multi-touch screen whose axes run from zero to `range - 1`. It lives as long as
/// the returned file stays open.
fn virtual_touchscreen(name: &str, range: i32, slots: usize) -> io::Result<File> {
    let mut failure = io::Error::from(io::ErrorKind::NotFound);
    let device = ["/dev/uinput", "/dev/input/uinput", "/dev/misc/uinput"]
        .iter()
        .find_map(|path| OpenOptions::new().write(true).open(path).map_err(|error| failure = error).ok())
        .ok_or(failure)?;
    let axes = [(ABS_MT_SLOT, slots as i32 - 1), (ABS_MT_TRACKING_ID, 0xffff), (ABS_MT_POSITION_X, range - 1), (ABS_MT_POSITION_Y, range - 1)];
    let set = |request: u32, value: u32| utils::ioctl_set(&device, UINPUT, request, Some(value));

    for kind in [EV_SYN, EV_KEY, EV_ABS] {
        set(SET_EVENT, kind.into())?;
    }
    set(SET_KEY, BTN_TOUCH.into())?;
    set(SET_PROPERTY, PROP_DIRECT)?;
    for (axis, _) in axes {
        set(SET_ABSOLUTE, axis.into())?;
    }

    // struct uinput_user_dev: name, input_id, ff_effects_max, then absmax, absmin, absfuzz and absflat.
    let mut setup = vec![0u8; NAME_SIZE + 12 + 4 * 4 * ABS_COUNT];
    let length = name.len().min(NAME_SIZE - 1);
    setup[..length].copy_from_slice(&name.as_bytes()[..length]);
    setup[NAME_SIZE..NAME_SIZE + 2].copy_from_slice(&BUS_VIRTUAL.to_ne_bytes());
    for (axis, maximum) in axes {
        let offset = NAME_SIZE + 12 + 4 * usize::from(axis);
        setup[offset..offset + 4].copy_from_slice(&maximum.to_ne_bytes());
    }
    (&device).write_all(&setup)?;
    utils::ioctl_set(&device, UINPUT, DEVICE_CREATE, None)?;
    Ok(device)
}
