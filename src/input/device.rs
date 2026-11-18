use std::time::Duration;

use super::classify::{Pointer, Roles};
use super::evdev::*;
use super::touch::{Contact, TouchParser};

const WHEEL_DETENT: f32 = 120.0;
const FIRST_NON_BUTTON_KEY: u16 = 0x160;

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
            (EV_KEY, code) if self.keyboard && !(BTN_MISC..FIRST_NON_BUTTON_KEY).contains(&code) => {
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

#[cfg(test)]
mod tests {
    use super::super::classify::{Idc, classify};
    use super::*;

    fn mouse() -> DeviceParser {
        let mut device = Capabilities { keys: vec![0; KEY_COUNT / 8], axes: vec![None; ABS_COUNT], ..Default::default() };
        for key in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
            device.keys[usize::from(key) / 8] |= 1 << (key % 8);
        }
        device.axes[usize::from(ABS_X)] = Some(Axis { min: 0, max: 65535 });
        device.axes[usize::from(ABS_Y)] = Some(Axis { min: 0, max: 65535 });
        device.relative = 1 << REL_WHEEL | 1 << REL_HWHEEL;
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
        let mut device = Capabilities { keys: vec![0; KEY_COUNT / 8], axes: vec![None; ABS_COUNT], ..Default::default() };
        device.keys[30 / 8] |= 1 << (30 % 8);
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
