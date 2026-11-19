//! Input devices as the kernel shows them: the events they send, what they are able to report,
//! what that makes of them, and the samples their events are turned into.

use std::time::Duration;

use super::touch::{Contact, TouchParser};
use crate::utils;
use crate::window::Rotation;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;

pub const SYN_REPORT: u16 = 0;
pub const SYN_MT_REPORT: u16 = 2;
pub const SYN_DROPPED: u16 = 3;

pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const REL_HWHEEL_HI_RES: u16 = 0x0c;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_PRESSURE: u16 = 0x18;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_FIRST: u16 = 0x30;
pub const ABS_MT_LAST: u16 = 0x3d;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TOOL_TYPE: u16 = 0x37;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_PRESSURE: u16 = 0x3a;
pub const MT_TOOL_PALM: i32 = 2;
pub const ABS_COUNT: usize = 0x40;

pub const KEY_ENTER: u16 = 28;
pub const KEY_A: u16 = 30;
pub const BTN_MISC: u16 = 0x100;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_JOYSTICK: u16 = 0x120;
pub const BTN_DIGI: u16 = 0x140;
pub const BTN_TOUCH: u16 = 0x14a;
pub const KEY_COUNT: usize = 0x300;

pub const PROP_POINTER: u32 = 0;
pub const PROP_DIRECT: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl RawEvent {
    /// Decodes kernel `input_event` records, whose timestamp takes `time_size` bytes.
    pub fn decode(bytes: &[u8], time_size: usize) -> impl Iterator<Item = RawEvent> + '_ {
        bytes.chunks_exact(time_size + 8).map(move |record| {
            let field = &record[time_size..];
            RawEvent {
                kind: u16::from_ne_bytes([field[0], field[1]]),
                code: u16::from_ne_bytes([field[2], field[3]]),
                value: i32::from_ne_bytes([field[4], field[5], field[6], field[7]]),
            }
        })
    }

    /// Appends the record the kernel expects on a write; it fills in the timestamp itself.
    pub fn encode(&self, time_size: usize, bytes: &mut Vec<u8>) {
        bytes.resize(bytes.len() + time_size, 0);
        bytes.extend_from_slice(&self.kind.to_ne_bytes());
        bytes.extend_from_slice(&self.code.to_ne_bytes());
        bytes.extend_from_slice(&self.value.to_ne_bytes());
    }
}

/// What a device's absolute axes hold: the slot it addresses and every axis's value, one per
/// slot for the multi-touch axes of a slotted device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AxisState {
    pub slot: i32,
    pub values: Vec<(u16, Vec<i32>)>,
}

impl AxisState {
    /// What an axis holds, a value for each slot; nothing for an axis that is not there.
    pub fn column(&self, code: u16) -> &[i32] {
        self.values.iter().find(|(other, _)| *other == code).map(|(_, values)| values.as_slice()).unwrap_or_default()
    }

    /// The events that take a device from this state back to an `earlier` one. The kernel only
    /// reports changes, so a reader that missed everything in between would otherwise go on
    /// with stale values.
    pub fn rewind(&self, earlier: &Self, out: &mut Vec<RawEvent>) {
        let slotted = |code: u16| (ABS_MT_FIRST..=ABS_MT_LAST).contains(&code);
        // Each value is approached from afar, which gets it past the kernel's jitter filter intact.
        let restore = |code: u16, value: i32| [value + (1 << 12), value].map(|value| RawEvent { kind: EV_ABS, code, value });
        let address = |slot: i32| RawEvent { kind: EV_ABS, code: ABS_MT_SLOT, value: slot };
        let start = out.len();

        let positions = self.values.iter().filter(|(code, _)| slotted(*code) && *code != ABS_MT_TRACKING_ID);
        let slots = positions.clone().map(|(_, values)| values.len()).max().unwrap_or(0);
        for slot in 0..slots {
            let mut addressed = false;
            for (code, values) in positions.clone() {
                let (Some(&now), Some(&then)) = (values.get(slot), earlier.column(*code).get(slot)) else { continue };
                if now != then {
                    out.extend((!std::mem::replace(&mut addressed, true)).then(|| address(slot as i32)));
                    out.extend(restore(*code, then));
                }
            }
        }
        // A slot is announced only together with a change in it, and the reader expects the old one.
        let last = usize::try_from(earlier.slot).ok().and_then(|slot| positions.clone().find_map(|(code, _)| Some((*code, *earlier.column(*code).get(slot)?))));
        if let Some((code, value)) = last.filter(|_| out.len() > start || self.slot != earlier.slot) {
            out.push(address(earlier.slot));
            out.extend(restore(code, value));
        }
        for (code, values) in self.values.iter().filter(|(code, _)| !slotted(*code)) {
            if let (&[now], &[then]) = (values.as_slice(), earlier.column(*code))
                && now != then
            {
                out.extend(restore(*code, then));
            }
        }
        if out.len() > start {
            out.push(RawEvent { kind: EV_SYN, code: SYN_REPORT, value: 0 });
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Axis {
    pub min: i32,
    pub max: i32,
}

impl Axis {
    pub fn span(&self) -> i32 {
        self.max.saturating_sub(self.min)
    }

    /// Position inside the axis as a fraction in `0.0..1.0`, the way Android's input reader scales it.
    pub fn normalize(&self, value: i32) -> f32 {
        let size = (i64::from(self.max) - i64::from(self.min) + 1).max(1) as f32;
        ((i64::from(value) - i64::from(self.min)) as f32 / size).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    pub name: String,
    pub bus: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub properties: u32,
    pub relative: u32,
    pub keys: Vec<u8>,
    pub axes: Vec<Option<Axis>>,
}

impl Capabilities {
    pub fn has_property(&self, property: u32) -> bool {
        self.properties & (1 << property) != 0
    }

    pub fn has_relative(&self, code: u16) -> bool {
        code < 32 && self.relative & (1 << code) != 0
    }

    pub fn has_key(&self, code: u16) -> bool {
        utils::bit(&self.keys, code.into())
    }

    pub fn has_key_in(&self, range: std::ops::Range<u16>) -> bool {
        range.into_iter().any(|code| self.has_key(code))
    }

    pub fn has_axis(&self, code: u16) -> bool {
        self.axis(code).is_some()
    }

    pub fn axis(&self, code: u16) -> Option<Axis> {
        self.axes.get(usize::from(code)).copied().flatten().filter(|axis| axis.max > axis.min)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Slots,
    Frames { tracked: bool },
    Single,
}

/// How sure we are that a touch surface is the screen itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Guess(i32),
    Certain,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Panel {
    pub protocol: Protocol,
    pub x: Axis,
    pub y: Axis,
    pub orientation: Rotation,
    pub follows_display: bool,
    pub confidence: Confidence,
    /// The device has `BTN_TOUCH`: while it is up, whatever it reports hovers.
    pub button: bool,
    /// The device reports pressure: a contact without any hovers.
    pub pressure: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pointer {
    pub x: Axis,
    pub y: Axis,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Roles {
    pub touch: Option<Panel>,
    pub pointer: Option<Pointer>,
    pub keyboard: bool,
    pub wheel: bool,
}

impl Roles {
    pub fn is_empty(&self) -> bool {
        self.touch.is_none() && self.pointer.is_none() && !self.keyboard && !self.wheel
    }
}

/// The subset of an Android input device configuration file that affects touch mapping.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Idc {
    pub device_type: Option<String>,
    pub orientation: Rotation,
    pub orientation_aware: Option<bool>,
    pub display: Option<String>,
}

impl Idc {
    pub fn parse(text: &str) -> Self {
        let mut idc = Self::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "touch.deviceType" => idc.device_type = Some(value.to_owned()),
                "touch.orientationAware" => idc.orientation_aware = Some(value != "0"),
                "touch.displayId" => idc.display = Some(value.to_owned()),
                "touch.orientation" => {
                    idc.orientation = match value {
                        "ORIENTATION_90" => Rotation::Deg90,
                        "ORIENTATION_180" => Rotation::Deg180,
                        "ORIENTATION_270" => Rotation::Deg270,
                        _ => Rotation::Deg0,
                    }
                }
                _ => {}
            }
        }
        idc
    }

    /// File names Android looks up for a device, most specific first.
    pub fn file_names(device: &Capabilities) -> Vec<String> {
        let mut names = Vec::new();
        if device.vendor != 0 && device.product != 0 {
            if device.version != 0 {
                names.push(format!("Vendor_{:04x}_Product_{:04x}_Version_{:04x}.idc", device.vendor, device.product, device.version));
            }
            names.push(format!("Vendor_{:04x}_Product_{:04x}.idc", device.vendor, device.product));
        }
        let canonical: String = device.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        names.push(format!("{canonical}.idc"));
        names
    }
}

/// Works out what a device is from what it can report, the way Android's own event hub does:
/// names differ between emulators and even between their versions, capabilities do not.
/// `display` is the unique id of the display the overlay is on, when it is known.
pub fn classify(device: &Capabilities, idc: &Idc, display: Option<&str>) -> Roles {
    let mut roles =
        Roles { keyboard: device.has_key(KEY_ENTER) || device.has_key(KEY_A), wheel: device.has_relative(REL_WHEEL) || device.has_relative(REL_HWHEEL), ..Default::default() };

    let gamepad = device.has_key_in(BTN_JOYSTICK..BTN_DIGI);
    let button = device.has_key(BTN_TOUCH);
    let mouse = device.has_key(BTN_LEFT) && !gamepad;
    let slotted = device.axis(ABS_MT_SLOT).is_some_and(|slot| slot.min == 0) && device.has_axis(ABS_MT_TRACKING_ID);
    let multi = device.axis(ABS_MT_POSITION_X).zip(device.axis(ABS_MT_POSITION_Y)).filter(|_| button || !gamepad);
    let single = device.axis(ABS_X).zip(device.axis(ABS_Y));

    let surface = match multi {
        Some((x, y)) if slotted => Some((Protocol::Slots, x, y, ABS_MT_PRESSURE)),
        Some((x, y)) => Some((Protocol::Frames { tracked: device.has_axis(ABS_MT_TRACKING_ID) }, x, y, ABS_MT_PRESSURE)),
        // An absolute device with mouse buttons is a mouse, even where it also lists `BTN_TOUCH`.
        None => single.filter(|_| button && !mouse).map(|(x, y)| (Protocol::Single, x, y, ABS_PRESSURE)),
    };
    match surface {
        Some((protocol, x, y, pressure)) => {
            roles.touch = screen_confidence(device, idc, display, x, y).map(|confidence| Panel {
                protocol,
                x,
                y,
                orientation: idc.orientation,
                follows_display: idc.orientation_aware.unwrap_or(true),
                confidence,
                button,
                pressure: device.has_axis(pressure),
            });
        }
        None => roles.pointer = single.filter(|_| mouse).map(|(x, y)| Pointer { x, y }),
    }
    roles
}

fn screen_confidence(device: &Capabilities, idc: &Idc, display: Option<&str>, x: Axis, y: Axis) -> Option<Confidence> {
    if idc.display.as_deref().zip(display).is_some_and(|(bound, own)| bound != own) {
        return None;
    }
    match idc.device_type.as_deref() {
        Some("touchScreen") => return Some(Confidence::Certain),
        Some("default") | None => {}
        Some(_) => return None,
    }
    if device.has_property(PROP_DIRECT) {
        return Some(Confidence::Certain);
    }
    if device.has_property(PROP_POINTER) {
        return None;
    }
    let name = device.name.to_ascii_lowercase();
    // Names that rule a surface out when nothing better than a guess is available.
    let not_a_screen = [
        "fingerprint",
        "finger_print",
        "fpc",
        "goodix_fp",
        "fp_",
        "gesture",
        "hall",
        "sensor",
        "accel",
        "gyro",
        "proximity",
        "keyboard",
        "mouse",
        "gpio-keys",
        "headset",
        "ir_",
        "hbtp",
    ];
    if not_a_screen.iter().any(|word| name.contains(word)) {
        return None;
    }
    // Neither is a surface a screen whose shorter side has fewer than 200 units.
    let span = x.span().min(y.span());
    (span >= 200).then_some(Confidence::Guess(span))
}

const WHEEL_DETENT: f32 = 120.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// One input change in device space: positions are fractions of the device's own axes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sample {
    Contact(Contact),
    Hover { u: f32, v: f32 },
    Button { button: MouseButton, pressed: bool },
    Wheel { x: f32, y: f32 },
    Key { code: u16, pressed: bool, repeat: bool },
}

pub struct DeviceParser {
    touch: Option<TouchParser>,
    pointer: Option<Pointer>,
    keyboard: bool,
    wheel: bool,
    precise_wheel: bool,
    position: (i32, i32),
    moved: bool,
    buttons: Vec<(MouseButton, bool)>,
    scroll: (f32, f32),
    contacts: Vec<Contact>,
}

impl DeviceParser {
    pub fn new(device: &Capabilities, roles: &Roles) -> Self {
        Self {
            touch: roles.touch.map(TouchParser::new),
            pointer: roles.pointer,
            keyboard: roles.keyboard,
            wheel: roles.wheel,
            precise_wheel: device.has_relative(REL_WHEEL_HI_RES) || device.has_relative(REL_HWHEEL_HI_RES),
            position: (0, 0),
            moved: false,
            buttons: Vec::new(),
            scroll: (0.0, 0.0),
            contacts: Vec::new(),
        }
    }

    pub fn touch_mut(&mut self) -> Option<&mut TouchParser> {
        self.touch.as_mut()
    }

    pub fn deadline(&self) -> Option<Duration> {
        self.touch.as_ref().and_then(TouchParser::deadline)
    }

    pub fn is_idle(&self) -> bool {
        self.touch.as_ref().is_none_or(TouchParser::is_idle)
    }

    /// Whether a device in this state has a finger on its touch surface.
    pub fn touching(&self, state: &AxisState, pressed: bool) -> bool {
        self.touch.as_ref().is_some_and(|touch| touch.touching(state, pressed))
    }

    pub fn feed(&mut self, event: RawEvent, now: Duration, out: &mut Vec<Sample>) {
        if let Some(touch) = &mut self.touch {
            touch.feed(event, now, &mut self.contacts);
            out.extend(self.contacts.drain(..).map(Sample::Contact));
        }

        let value = event.value;
        match (event.kind, event.code) {
            (EV_ABS, ABS_X) if self.pointer.is_some() => {
                self.position.0 = value;
                self.moved = true;
            }
            (EV_ABS, ABS_Y) if self.pointer.is_some() => {
                self.position.1 = value;
                self.moved = true;
            }
            (EV_KEY, BTN_LEFT | BTN_TOUCH) if self.pointer.is_some() => self.buttons.push((MouseButton::Left, value != 0)),
            (EV_KEY, BTN_RIGHT) if self.pointer.is_some() => self.buttons.push((MouseButton::Right, value != 0)),
            (EV_KEY, BTN_MIDDLE) if self.pointer.is_some() => self.buttons.push((MouseButton::Middle, value != 0)),
            // The codes from BTN_MISC up to 0x160 are buttons of mice, pads and pens, not keys.
            (EV_KEY, code) if self.keyboard && !(BTN_MISC..0x160).contains(&code) => {
                out.push(Sample::Key { code, pressed: value != 0, repeat: value == 2 });
            }
            (EV_REL, REL_WHEEL) if self.wheel && !self.precise_wheel => self.scroll.1 += value as f32,
            (EV_REL, REL_HWHEEL) if self.wheel && !self.precise_wheel => self.scroll.0 += value as f32,
            (EV_REL, REL_WHEEL_HI_RES) if self.wheel => self.scroll.1 += value as f32 / WHEEL_DETENT,
            (EV_REL, REL_HWHEEL_HI_RES) if self.wheel => self.scroll.0 += value as f32 / WHEEL_DETENT,
            (EV_SYN, SYN_REPORT) => self.flush(out),
            _ => {}
        }
    }

    pub fn tick(&mut self, now: Duration, out: &mut Vec<Sample>) {
        if let Some(touch) = &mut self.touch {
            touch.tick(now, &mut self.contacts);
            out.extend(self.contacts.drain(..).map(Sample::Contact));
        }
    }

    pub fn cancel(&mut self, out: &mut Vec<Sample>) {
        if let Some(touch) = &mut self.touch {
            touch.cancel(&mut self.contacts);
            out.extend(self.contacts.drain(..).map(Sample::Contact));
        }
        self.buttons.clear();
        self.scroll = (0.0, 0.0);
    }

    fn flush(&mut self, out: &mut Vec<Sample>) {
        if let Some(pointer) = self.pointer.filter(|_| std::mem::take(&mut self.moved)) {
            out.push(Sample::Hover { u: pointer.x.normalize(self.position.0), v: pointer.y.normalize(self.position.1) });
        }
        out.extend(self.buttons.drain(..).map(|(button, pressed)| Sample::Button { button, pressed }));
        let (x, y) = std::mem::take(&mut self.scroll);
        if x != 0.0 || y != 0.0 {
            out.push(Sample::Wheel { x, y });
        }
    }
}

/// What the kernel itself tells about an event device, and does to it, when asked.
#[cfg(target_os = "android")]
pub mod kernel {
    use std::fs::File;

    use super::{ABS_COUNT, ABS_MT_FIRST, ABS_MT_LAST, ABS_MT_SLOT, Axis, AxisState, Capabilities, EV_ABS, EV_KEY, EV_REL, KEY_COUNT};
    use crate::utils::{self, bit};

    /// How many bytes of an event record its timestamp takes.
    pub const TIME_SIZE: usize = size_of::<libc::timeval>();
    /// The letter that requests to an event device go under.
    const EVENTS: u8 = b'E';

    fn query<T: ?Sized>(device: &File, number: u32, target: &mut T) -> bool {
        utils::ioctl_get(device, EVENTS, number, target)
    }

    /// Reads what a device can report. Every request is a getter: nothing is written to the device.
    pub fn capabilities(device: &File) -> Option<Capabilities> {
        let mut name = [0u8; 128];
        let mut identity = [0u16; 4];
        let mut properties = [0u8; 4];
        let mut relative = [0u8; 4];
        let mut axes = [0u8; ABS_COUNT / 8];
        let mut found = Capabilities { keys: vec![0; KEY_COUNT / 8], axes: vec![None; ABS_COUNT], ..Default::default() };

        if !query(device, 0x02, &mut identity) {
            return None;
        }
        query(device, 0x06, &mut name[..127]);
        query(device, 0x09, &mut properties);
        query(device, 0x20 + u32::from(EV_REL), &mut relative);
        query(device, 0x20 + u32::from(EV_ABS), &mut axes);
        query(device, 0x20 + u32::from(EV_KEY), found.keys.as_mut_slice());

        found.name = utils::text(&name);
        [found.bus, found.vendor, found.product, found.version] = identity;
        found.properties = u32::from_ne_bytes(properties);
        found.relative = u32::from_ne_bytes(relative);
        for code in (0..ABS_COUNT).filter(|&code| bit(&axes, code)) {
            let mut info = [0i32; 6];
            if query(device, 0x40 + code as u32, &mut info) {
                found.axes[code] = Some(Axis { min: info[1], max: info[2] });
            }
        }
        Some(found)
    }

    /// Whether a key or button is held down right now.
    pub fn key_down(device: &File, code: u16) -> bool {
        let mut keys = [0u8; KEY_COUNT / 8];
        query(device, 0x18, &mut keys) && bit(&keys, code.into())
    }

    /// What the given axes hold right now, with one value per slot for multi-touch axes.
    pub fn axis_state(device: &File, axes: &[u16], slots: usize) -> AxisState {
        let mut state = AxisState::default();
        for &code in axes {
            let mut info = [0i32; 6];
            if (ABS_MT_FIRST..=ABS_MT_LAST).contains(&code) {
                let mut values = vec![0i32; slots + 1];
                values[0] = i32::from(code);
                if slots > 0 && query(device, 0x0a, values.as_mut_slice()) {
                    state.values.push((code, values.split_off(1)));
                }
            } else if query(device, 0x40 + u32::from(code), &mut info) {
                match code {
                    ABS_MT_SLOT => state.slot = info[0],
                    _ => state.values.push((code, vec![info[0]])),
                }
            }
        }
        state
    }

    /// Takes a device away from every other reader, or gives it back.
    pub fn grab(device: &File, grab: bool) -> std::io::Result<()> {
        utils::ioctl_set(device, EVENTS, 0x90, Some(grab.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's side of a slotted device: what it holds, and the slot it last announced.
    struct Kernel {
        state: AxisState,
        addressing: i32,
    }

    impl Kernel {
        fn write(&mut self, events: &[RawEvent]) {
            for event in events.iter().filter(|event| event.kind == EV_ABS) {
                if event.code == ABS_MT_SLOT {
                    self.addressing = event.value;
                    continue;
                }
                let slotted = (ABS_MT_FIRST..=ABS_MT_LAST).contains(&event.code);
                let values = &mut self.state.values.iter_mut().find(|(code, _)| *code == event.code).unwrap().1;
                let held = &mut values[if slotted { self.addressing as usize } else { 0 }];
                if *held != event.value {
                    *held = event.value;
                    if slotted {
                        self.state.slot = self.addressing;
                    }
                }
            }
        }
    }

    fn state(slot: i32, x: [i32; 3], y: [i32; 3], single: i32) -> AxisState {
        AxisState { slot, values: vec![(ABS_X, vec![single]), (ABS_MT_POSITION_X, x.to_vec()), (ABS_MT_POSITION_Y, y.to_vec()), (ABS_MT_TRACKING_ID, vec![-1; 3])] }
    }

    #[test]
    fn events_survive_encoding() {
        let events = [RawEvent { kind: EV_ABS, code: ABS_MT_POSITION_X, value: -7 }, RawEvent { kind: EV_SYN, code: SYN_REPORT, value: 0 }];
        for time_size in [8, 16] {
            let mut bytes = Vec::new();
            events.iter().for_each(|event| event.encode(time_size, &mut bytes));
            assert_eq!(RawEvent::decode(&bytes, time_size).collect::<Vec<_>>(), events);
        }
    }

    #[test]
    fn rewinding_puts_every_axis_and_the_slot_back() {
        let earlier = state(1, [100, 200, 300], [10, 20, 30], 5);
        for now in
            [state(1, [100, 200, 300], [10, 20, 30], 5), state(0, [100, 200, 300], [10, 20, 30], 5), state(2, [100, 999, 300], [10, 20, 31], 9), state(1, [7, 8, 9], [1, 2, 3], 5)]
        {
            let mut events = Vec::new();
            now.rewind(&earlier, &mut events);
            let untouched = now == earlier;
            assert_eq!(events.is_empty(), untouched);
            assert!(untouched || events.last() == Some(&RawEvent { kind: EV_SYN, code: SYN_REPORT, value: 0 }));
            assert!(!events.iter().any(|event| event.code == ABS_MT_TRACKING_ID));

            let mut kernel = Kernel { addressing: now.slot, state: now };
            kernel.write(&events);
            assert_eq!(kernel.state, earlier);
        }
    }

    const DIRECT: u32 = 1 << PROP_DIRECT;
    const SCREEN: &str = "touch.deviceType = touchScreen\ntouch.orientationAware = 1\n";
    const KEYBOARD: [u16; 4] = [1, KEY_ENTER, KEY_A, 57];
    const MOUSE: [u16; 3] = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE];
    const BTN_TOOL_FINGER: u16 = 0x145;

    fn device(name: &str, properties: u32, keys: &[u16], axes: &[(u16, i32, i32)], relative: &[u16]) -> Capabilities {
        let mut device = Capabilities { name: name.into(), properties, keys: vec![0; KEY_COUNT / 8], axes: vec![None; ABS_COUNT], ..Default::default() };
        for &key in keys {
            device.keys[usize::from(key) / 8] |= 1 << (key % 8);
        }
        for &(code, min, max) in axes {
            device.axes[usize::from(code)] = Some(Axis { min, max });
        }
        for &code in relative {
            device.relative |= 1 << code;
        }
        device
    }

    fn slots(count: i32, width: i32, height: i32) -> Vec<(u16, i32, i32)> {
        vec![(ABS_MT_SLOT, 0, count - 1), (ABS_MT_POSITION_X, 0, width), (ABS_MT_POSITION_Y, 0, height), (ABS_MT_TRACKING_ID, 0, 65535)]
    }

    fn screen(roles: Roles) -> Panel {
        let panel = roles.touch.expect("a touchscreen");
        assert!(roles.pointer.is_none() && panel.confidence == Confidence::Certain, "{roles:?}");
        panel
    }

    #[test]
    fn tablet_panel_is_a_slot_touchscreen() {
        let mut axes = slots(10, 15039, 23999);
        axes.extend([(ABS_X, 0, 15039), (ABS_Y, 0, 23999)]);
        let panel = screen(classify(&device("synaptics_tcm_touch", DIRECT, &[BTN_TOUCH, BTN_TOOL_FINGER, 143], &axes, &[]), &Idc::default(), None));
        assert_eq!((panel.protocol, panel.x.max, panel.y.max, panel.button, panel.pressure), (Protocol::Slots, 15039, 23999, true, false));
    }

    #[test]
    fn bluestacks_panels_need_their_idc_file() {
        let frames = device("BlueStacks Virtual Touch", 0, &[], &[(ABS_MT_POSITION_X, 0, 32767), (ABS_MT_POSITION_Y, 0, 32767)], &[]);
        assert_eq!(Idc::file_names(&frames), ["BlueStacks_Virtual_Touch.idc"]);
        let idc = Idc::parse("# Basic Parameters\ntouch.deviceType = touchScreen\n#touch.orientationAware = 1\ndevice.internal = 1\n");
        let panel = screen(classify(&frames, &idc, None));
        assert_eq!((panel.protocol, panel.button), (Protocol::Frames { tracked: false }, false));
        assert_eq!(classify(&frames, &Idc::default(), None).touch.map(|panel| panel.confidence), Some(Confidence::Guess(32767)));

        let newer = device("BlueStacks Virtual Touch", 0, &[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_TOUCH], &slots(16, 32767, 32767), &[]);
        assert_eq!(screen(classify(&newer, &idc, None)).protocol, Protocol::Slots);
    }

    #[test]
    fn android_emulator_nodes_belong_to_one_display_each() {
        let mut axes = slots(11, 32767, 32767);
        axes.extend([(ABS_X, 0, 32767), (ABS_Y, 0, 32767), (ABS_MT_PRESSURE, 0, 1024), (ABS_MT_TOOL_TYPE, 0, 15)]);
        let node = device("virtio_input_multi_touch_1", 0, &[0x141, 0x14b], &axes, &[]);
        let idc = Idc::parse(&format!("{SCREEN}touch.displayId = local:4619827259155550721\n"));

        let panel = screen(classify(&node, &idc, Some("local:4619827259155550721")));
        assert_eq!((panel.protocol, panel.button, panel.pressure), (Protocol::Slots, false, true));
        assert!(classify(&node, &idc, Some("local:4619827551948147201")).touch.is_none());
        assert!(classify(&node, &idc, None).touch.is_some());
    }

    #[test]
    fn emulator_panels_are_told_apart_by_what_they_report() {
        let genymotion = device(
            "Genymotion Virtual Input",
            0,
            &[&KEYBOARD[..], &[BTN_TOUCH, BTN_LEFT, BTN_TOOL_FINGER]].concat(),
            &[(ABS_MT_POSITION_X, 0, 1080), (ABS_MT_POSITION_Y, 0, 1920), (ABS_MT_PRESSURE, 0, 0)],
            &[REL_WHEEL, REL_HWHEEL],
        );
        let roles = classify(&genymotion, &Idc::parse(SCREEN), None);
        assert_eq!((screen(roles).protocol, screen(roles).button, screen(roles).pressure), (Protocol::Frames { tracked: false }, true, false));
        assert!(roles.keyboard && roles.wheel);

        let mut axes = slots(16, 1279, 719);
        axes.push((ABS_MT_PRESSURE, 0, 2));
        let ldplayer = device("input", DIRECT, &[&KEYBOARD[..], &[BTN_TOUCH, BTN_TOOL_FINGER]].concat(), &axes, &[REL_WHEEL, REL_HWHEEL]);
        let roles = classify(&ldplayer, &Idc::default(), None);
        assert_eq!((screen(roles).protocol, screen(roles).pressure), (Protocol::Slots, true));
        assert!(roles.keyboard && roles.wheel);

        let nox = device("Android_Input", 0, &[], &[(ABS_MT_POSITION_X, 0, 1280), (ABS_MT_POSITION_Y, 0, 720), (ABS_MT_TRACKING_ID, 0, 1)], &[]);
        assert_eq!(screen(classify(&nox, &Idc::parse(SCREEN), None)).protocol, Protocol::Frames { tracked: true });

        let mut axes = slots(32, 1080, 1920);
        axes.push((ABS_MT_PRESSURE, 0, 0));
        let mumu = device("Xiaomi Input", DIRECT, &[&KEYBOARD[..], &MOUSE[..], &[BTN_TOUCH, BTN_TOOL_FINGER]].concat(), &axes, &[0, 1, REL_WHEEL, REL_HWHEEL]);
        let roles = classify(&mumu, &Idc::default(), None);
        assert_eq!((screen(roles).protocol, screen(roles).pressure), (Protocol::Slots, false));
        assert!(roles.keyboard && roles.wheel);

        let memu = device(
            "User Input",
            DIRECT,
            &[BTN_TOUCH],
            &[(ABS_MT_SLOT, 0, 9), (ABS_MT_POSITION_X, 0, 1280), (ABS_MT_POSITION_Y, 0, 720), (ABS_MT_TRACKING_ID, 0, 65535), (0x30, 0, 255)],
            &[],
        );
        assert_eq!(screen(classify(&memu, &Idc::default(), None)).protocol, Protocol::Slots);

        let one_slot =
            device("panel", DIRECT, &[BTN_TOUCH], &[(ABS_MT_SLOT, 0, 0), (ABS_MT_POSITION_X, 0, 1280), (ABS_MT_POSITION_Y, 0, 720), (ABS_MT_TRACKING_ID, 0, 65535)], &[]);
        assert_eq!(screen(classify(&one_slot, &Idc::default(), None)).protocol, Protocol::Frames { tracked: true });
    }

    #[test]
    fn absolute_mice_are_pointers_even_when_they_list_a_touch_button() {
        let tablets = [
            device("QEMU QEMU USB Tablet", 0, &MOUSE, &[(ABS_X, 0, 32767), (ABS_Y, 0, 32767)], &[REL_WHEEL]),
            device("QEMU Virtio Tablet", 0, &[&MOUSE[..], &[BTN_TOUCH]].concat(), &[(ABS_X, 0, 32767), (ABS_Y, 0, 32767)], &[REL_WHEEL]),
            device("VirtualBox mouse integration", 0, &MOUSE, &[(ABS_X, 0, 65535), (ABS_Y, 0, 65535)], &[REL_WHEEL, REL_HWHEEL]),
        ];
        for tablet in tablets {
            let roles = classify(&tablet, &Idc::default(), None);
            assert!(roles.touch.is_none() && roles.pointer.is_some() && roles.wheel && !roles.keyboard, "{}", tablet.name);
        }

        let resistive = device("ADS7846 Touchscreen", DIRECT, &[BTN_TOUCH], &[(ABS_X, 0, 4095), (ABS_Y, 0, 4095), (ABS_PRESSURE, 0, 255)], &[]);
        let panel = screen(classify(&resistive, &Idc::default(), None));
        assert_eq!((panel.protocol, panel.button, panel.pressure), (Protocol::Single, true, true));
    }

    #[test]
    fn keyboards_sensors_touchpads_and_gamepads_are_not_screens() {
        let keyboard = device("AT Translated Set 2 keyboard", 0, &KEYBOARD, &[], &[]);
        let roles = classify(&keyboard, &Idc::default(), None);
        assert!(roles.keyboard && roles.touch.is_none() && roles.pointer.is_none());

        let fingerprint = device("goodix_fp", 0, &[102, 103], &[(ABS_MT_POSITION_X, 0, 1000), (ABS_MT_POSITION_Y, 0, 1000)], &[]);
        assert!(classify(&fingerprint, &Idc::default(), None).touch.is_none());

        let touchpad = device("Synaptics TouchPad", 1 << PROP_POINTER, &[BTN_TOUCH], &slots(5, 4000, 3000), &[]);
        assert!(classify(&touchpad, &Idc::default(), None).touch.is_none());

        let gamepad = device("Xiaomi Joystick", 0, &[BTN_JOYSTICK, 0x130, BTN_LEFT], &[(ABS_X, -1_000_000, 1_000_000), (ABS_Y, -1_000_000, 1_000_000)], &[0, 1]);
        assert!(classify(&gamepad, &Idc::default(), None).is_empty());

        let flat = device("broken", DIRECT, &[BTN_TOUCH], &[(ABS_MT_POSITION_X, 0, 0), (ABS_MT_POSITION_Y, 0, 0)], &[]);
        assert!(classify(&flat, &Idc::default(), None).touch.is_none());
    }

    #[test]
    fn idc_orientation_and_names() {
        let idc = Idc::parse("touch.deviceType = touchScreen\ntouch.orientation = ORIENTATION_270\ntouch.orientationAware = 0\n");
        assert_eq!(idc.orientation, Rotation::Deg270);
        assert_eq!(idc.orientation_aware, Some(false));

        let usb = Capabilities { name: "Some Panel v2".into(), vendor: 0x04f3, product: 0x261a, version: 0x0100, ..Default::default() };
        assert_eq!(Idc::file_names(&usb), ["Vendor_04f3_Product_261a_Version_0100.idc", "Vendor_04f3_Product_261a.idc", "Some_Panel_v2.idc"]);
    }

    fn mouse() -> DeviceParser {
        let device = device("", 0, &MOUSE, &[(ABS_X, 0, 65535), (ABS_Y, 0, 65535)], &[REL_WHEEL, REL_HWHEEL]);
        DeviceParser::new(&device, &classify(&device, &Idc::default(), None))
    }

    fn feed(parser: &mut DeviceParser, events: &[(u16, u16, i32)]) -> Vec<Sample> {
        let mut out = Vec::new();
        for &(kind, code, value) in events {
            parser.feed(RawEvent { kind, code, value }, Duration::ZERO, &mut out);
        }
        out
    }

    #[test]
    fn mouse_reports_position_before_the_click() {
        let mut parser = mouse();
        let samples = feed(&mut parser, &[(EV_KEY, BTN_LEFT, 1), (EV_ABS, ABS_X, 32768), (EV_ABS, ABS_Y, 16384), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(samples, [Sample::Hover { u: 0.5, v: 0.25 }, Sample::Button { button: MouseButton::Left, pressed: true }]);
    }

    #[test]
    fn wheel_steps_accumulate_per_frame() {
        let mut parser = mouse();
        let samples = feed(&mut parser, &[(EV_REL, REL_WHEEL, 1), (EV_REL, REL_WHEEL, 1), (EV_REL, REL_HWHEEL, -1), (EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(samples, [Sample::Wheel { x: -1.0, y: 2.0 }]);
    }

    #[test]
    fn keyboard_ignores_mouse_buttons() {
        let device = device("", 0, &[KEY_A], &[], &[]);
        let mut parser = DeviceParser::new(&device, &classify(&device, &Idc::default(), None));
        let samples = feed(&mut parser, &[(EV_KEY, 30, 1), (EV_KEY, BTN_LEFT, 1), (EV_KEY, 30, 2), (EV_KEY, 30, 0)]);
        assert_eq!(
            samples,
            [
                Sample::Key { code: 30, pressed: true, repeat: false },
                Sample::Key { code: 30, pressed: true, repeat: true },
                Sample::Key { code: 30, pressed: false, repeat: false }
            ]
        );
    }
}
