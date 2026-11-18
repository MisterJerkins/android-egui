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
    /// The events that take a device from this state back to an `earlier` one. The kernel only
    /// reports changes, so a reader that missed everything in between would otherwise go on
    /// with stale values.
    pub fn rewind(&self, earlier: &Self, out: &mut Vec<RawEvent>) {
        // Each value is approached from afar, which gets it past the kernel's jitter filter intact.
        const AFAR: i32 = 1 << 12;
        let slotted = |code: u16| (ABS_MT_FIRST..=ABS_MT_LAST).contains(&code);
        let restore = |code: u16, value: i32| [value + AFAR, value].map(|value| RawEvent { kind: EV_ABS, code, value });
        let address = |slot: i32| RawEvent { kind: EV_ABS, code: ABS_MT_SLOT, value: slot };
        let before = |code: u16| earlier.values.iter().find(|(other, _)| *other == code).map(|(_, values)| values.as_slice());
        let start = out.len();

        let positions = self.values.iter().filter(|(code, _)| slotted(*code) && *code != ABS_MT_TRACKING_ID);
        let slots = positions.clone().map(|(_, values)| values.len()).max().unwrap_or(0);
        for slot in 0..slots {
            let mut addressed = false;
            for (code, values) in positions.clone() {
                let (Some(&now), Some(&then)) = (values.get(slot), before(*code).and_then(|values| values.get(slot))) else { continue };
                if now != then {
                    out.extend((!std::mem::replace(&mut addressed, true)).then(|| address(slot as i32)));
                    out.extend(restore(*code, then));
                }
            }
        }
        // A slot is announced only together with a change in it, and the reader expects the old one.
        let last = usize::try_from(earlier.slot).ok().and_then(|slot| positions.clone().find_map(|(code, _)| Some((*code, *before(*code)?.get(slot)?))));
        if let Some((code, value)) = last.filter(|_| out.len() > start || self.slot != earlier.slot) {
            out.push(address(earlier.slot));
            out.extend(restore(code, value));
        }
        for (code, values) in self.values.iter().filter(|(code, _)| !slotted(*code)) {
            if let (&[now], Some(&[then])) = (values.as_slice(), before(*code))
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
        self.keys.get(usize::from(code) / 8).is_some_and(|byte| byte & (1 << (code % 8)) != 0)
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
}
