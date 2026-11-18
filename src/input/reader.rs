use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::classify::{Confidence, IDC_DIRECTORIES, Idc, Protocol, Roles, classify};
use super::device::{DeviceParser, Sample};
use super::evdev::{ABS_MT_SLOT, AxisState, BTN_TOUCH, Capabilities, EV_SYN, RawEvent, SYN_DROPPED, SYN_REPORT};
use super::router::{Event, Queue, Router, Target, View};
use super::touch::{Fingers, Injector, Motion, Owner, Phase, RANGE, SLOTS, Touch, VIRTUAL_TOUCHSCREEN, pointer_to_screen, screen_to_touch, touch_to_screen};
use crate::sys::watch::Watch;
use crate::sys::{evdev, uinput};
use crate::{Result, bail, err};

/// Where event devices live; the second place is where containers put the ones they create.
const DEVICES: [&str; 2] = ["/dev/input", "/dev"];
const INPUT_MAJOR: u64 = 13;
/// How often the devices are looked through where their directories cannot be watched.
const RESCAN: Duration = Duration::from_secs(1);
/// How long a device that has just appeared gets for its owner and mode to be set.
const ARRIVAL: Duration = Duration::from_millis(200);
const MAX_SLOTS: usize = 32;
/// How long Android gets to pick up the virtual touchscreen before the real one is taken away.
const SETTLE: Duration = Duration::from_millis(500);
/// The device number touches reported by the system go under.
const SYSTEM: u32 = 0;

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
        uinput::grab(&self.file, grab)?;
        let state = evdev::axis_state(&self.file, &self.axes, self.slots);
        // A finger that came down just before the grab is Android's, which saw it land.
        if grab && self.parser.touching(&state, evdev::key_down(&self.file, BTN_TOUCH)) {
            return uinput::grab(&self.file, false);
        }
        self.grabbed = grab;
        self.left = grab.then_some(state);
        Ok(())
    }

    fn rewind(&mut self) {
        let Some(left) = self.left.take() else { return };
        let (mut events, mut bytes) = (Vec::new(), Vec::new());
        evdev::axis_state(&self.file, &self.axes, self.slots).rewind(&left, &mut events);
        events.iter().for_each(|event| event.encode(evdev::TIME_SIZE, &mut bytes));
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
            event.encode(evdev::TIME_SIZE, &mut self.bytes);
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

struct Hub {
    state: Mutex<State>,
    wake: Condvar,
    started: Instant,
}

/// What [`Input::drain`] says about the input it handed over.
pub struct Taken {
    /// When the last of it happened.
    pub at: Option<Instant>,
    /// Whether some of the input since the last call was kept from the system, which then
    /// does not know the user is there.
    pub kept: bool,
}

/// Reads touch, mouse and keyboard devices, one thread per device. Devices are opened for
/// reading and left alone, so whatever is under the overlay keeps its input, until [`Touch`]
/// is told to take control.
pub struct Input {
    hub: Arc<Hub>,
}

impl Input {
    /// `display` is the unique id of the display the overlay is on, for systems that tie each
    /// touchscreen to one display.
    pub fn start(display: Option<String>) -> Self {
        let hub = Arc::new(Hub { state: Mutex::new(State::default()), wake: Condvar::new(), started: Instant::now() });
        let scanner = hub.clone();
        let _ = std::thread::Builder::new().name("input-scan".into()).spawn(move || scan(&scanner, display.as_deref()));
        Self { hub }
    }

    /// Hands over the latest view and takes the previous one back, so its buffers are reused.
    pub fn swap_view(&self, view: &mut View) {
        std::mem::swap(&mut self.hub.lock().routing.view, view);
    }

    pub fn touch(&self) -> Touch {
        Touch::new(self.hub.clone())
    }

    /// Whether there is a touchscreen among the input devices. The answer waits until all of
    /// them have been looked at once, which a busy system can take its time over, but for no
    /// longer than `patience`.
    pub fn has_screen(&self, patience: Duration) -> bool {
        let until = Instant::now() + patience;
        loop {
            let state = self.hub.lock();
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
    pub fn system_touches(&self, enabled: bool) -> impl FnMut(Motion) + Send + 'static {
        let hub = self.hub.clone();
        hub.lock().routing.system = enabled;
        move |motion| {
            let mut state = hub.lock();
            let Routing { view, router, queue, events, .. } = &mut state.routing;
            for (contact, position) in motion.contacts() {
                router.touch(SYSTEM, contact, position, view, events);
            }
            let now = Instant::now();
            events.drain(..).for_each(|event| queue.push(event, now));
            hub.wake.notify_one();
        }
    }

    /// Blocks until input is waiting or `timeout` passes. Returns whether input is waiting.
    pub fn wait(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        let mut state = self.hub.lock();
        loop {
            let now = self.hub.started.elapsed();
            let deadline = state.tick(now);
            if !state.routing.queue.is_empty() {
                return true;
            }
            let remaining = until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let pause = deadline.map_or(remaining, |at| remaining.min(at.saturating_sub(now)));
            state = self.hub.wake.wait_timeout(state, pause.max(Duration::from_millis(1))).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Takes the input for one frame; whatever is left makes [`Input::wait`] return at once.
    pub fn drain(&self, into: &mut Vec<Event>) -> Taken {
        let mut state = self.hub.lock();
        let at = state.routing.queue.take_frame(into);
        Taken { at, kept: std::mem::take(&mut state.routing.kept) }
    }

    pub fn describe(&self) -> Vec<String> {
        let state = self.hub.lock();
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

impl Hub {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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

impl Injector for Hub {
    fn set_control(&self, enabled: bool) -> Result<()> {
        let mut state = self.lock();
        let now = self.started.elapsed();
        if enabled && state.routing.control.is_none() {
            let device = uinput::touchscreen(VIRTUAL_TOUCHSCREEN, RANGE, SLOTS).map_err(|error| err!("cannot create a virtual touchscreen: {error}"))?;
            state.routing.control = Some(Control { device, fingers: Fingers::default(), ready: now + SETTLE, events: Vec::new(), bytes: Vec::new() });
        }
        state.routing.controlling = enabled;
        state.sync_control(now);
        self.wake.notify_one();
        Ok(())
    }

    fn is_control(&self) -> bool {
        self.lock().routing.controlling
    }

    fn finger(&self, finger: u32, phase: Phase, position: [f32; 2]) -> Result<()> {
        let mut state = self.lock();
        let settling = state.routing.control.as_ref().map_or(Duration::ZERO, |control| control.ready.saturating_sub(self.started.elapsed()));
        if !settling.is_zero() {
            drop(state);
            std::thread::sleep(settling);
            state = self.lock();
        }
        let Routing { view, control, controlling, .. } = &mut state.routing;
        let Some(control) = control.as_mut().filter(|_| *controlling) else { bail!("touch control is off") };
        match (control.send(Owner::Code(finger), phase, screen_to_touch(position, view.size, view.rotation)), phase) {
            (true, _) => Ok(()),
            (false, Phase::Start) => bail!("every touch slot is taken"),
            (false, _) => bail!("finger {finger} is not down"),
        }
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
            // Only real event devices: a pipe or socket under the same name would lose what is read from it.
            let node = |meta: &fs::Metadata| meta.file_type().is_char_device() && meta.rdev() >> 8 & 0xfff == INPUT_MAJOR;
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

            let mut state = hub.lock();
            state.devices.insert(index, device);
            state.pick_screens();
            restore(&mut state, index, &file);
            state.sync_control(hub.started.elapsed());
            drop(state);

            let reader = hub.clone();
            let _ = std::thread::Builder::new().name(format!("input-{name}")).spawn(move || read(&reader, index, &file));
        }
        hub.lock().scanned = true;
        // Devices come and go rarely, and their directories tell when: no looking in between.
        let changed = watch.as_mut().is_some_and(Watch::wait);
        std::thread::sleep(if changed { ARRIVAL } else { RESCAN });
    }
}

fn open(path: &std::path::Path, display: Option<&str>) -> Option<(Arc<File>, Device)> {
    let file = Arc::new(File::open(path).ok()?);
    let capabilities = evdev::capabilities(&file).filter(|device| device.name != VIRTUAL_TOUCHSCREEN)?;
    let roles = classify(&capabilities, &load_idc(&capabilities), display);
    if roles.is_empty() {
        return None;
    }
    let parser = DeviceParser::new(&capabilities, &roles);
    let slots = capabilities.axis(ABS_MT_SLOT).map_or(0, |axis| axis.max as usize + 1).min(MAX_SLOTS);
    let axes = (0u16..).zip(&capabilities.axes).filter_map(|(code, axis)| axis.map(|_| code)).collect();
    let path = path.display().to_string();
    Some((file.clone(), Device { path, name: capabilities.name, roles, parser, slots, screen: false, file, axes, keys_down: 0, grabbed: false, refused: false, left: None }))
}

fn load_idc(device: &Capabilities) -> Idc {
    let names = Idc::file_names(device);
    IDC_DIRECTORIES
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
        parser.restore_slots(&evdev::axis_state(file, &device.axes, device.slots), evdev::key_down(file, BTN_TOUCH), &mut contacts);
    }
    samples.extend(contacts.into_iter().map(Sample::Contact));
    routing.dispatch(index, device, samples);
}

fn read(hub: &Hub, index: u32, mut file: &File) {
    let mut bytes = [0u8; (evdev::TIME_SIZE + 8) * 64];
    let mut dropped = false;
    loop {
        let length = match file.read(&mut bytes) {
            Ok(0) => break,
            Ok(length) => length,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let now = hub.started.elapsed();
        let mut state = hub.lock();
        let State { devices, samples, routing, .. } = &mut *state;
        let Some(device) = devices.get_mut(&index) else { return };
        let mut resync = false;
        for event in RawEvent::decode(&bytes[..length], evdev::TIME_SIZE) {
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

    let mut state = hub.lock();
    let State { devices, samples, routing, .. } = &mut *state;
    if let Some(mut device) = devices.remove(&index) {
        device.parser.cancel(samples);
        routing.dispatch(index, &mut device, samples);
    }
    state.pick_screens();
    state.sync_control(hub.started.elapsed());
    hub.wake.notify_one();
}
