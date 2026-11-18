use std::time::Duration;

use crate::input::classify::{Panel, Protocol};
use crate::input::evdev::*;

const MAX_CONTACTS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Start,
    Move,
    End,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Contact {
    pub id: u32,
    pub phase: Phase,
    pub u: f32,
    pub v: f32,
}

/// Timing rules for panels that report whole frames with neither tracking ids nor `BTN_TOUCH`
/// (emulators). Such panels drop a held contact for a moment and then resume it, so a release
/// is only believed once the panel has stayed empty: briefly for a fresh contact, to keep taps
/// quick, and longer once a contact has been held for more than a tap.
#[derive(Clone, Copy, Debug)]
pub struct Quirks {
    pub tap: Duration,
    pub bounce: Duration,
    pub glitch: Duration,
    pub silence: Duration,
}

impl Default for Quirks {
    fn default() -> Self {
        Self { tap: Duration::from_millis(200), bounce: Duration::from_millis(40), glitch: Duration::from_millis(250), silence: Duration::from_secs(1) }
    }
}

#[derive(Clone, Copy)]
struct Slot {
    tracking: i32,
    x: i32,
    y: i32,
    pressure: i32,
    palm: bool,
    live: Option<(u32, i32)>,
    moved: bool,
}

const EMPTY_SLOT: Slot = Slot { tracking: -1, x: 0, y: 0, pressure: 0, palm: false, live: None, moved: false };

#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
    tracking: Option<i32>,
}

#[derive(Clone, Copy)]
struct Live {
    id: u32,
    point: Point,
}

#[derive(Default)]
struct Partial {
    x: Option<i32>,
    y: Option<i32>,
    tracking: Option<i32>,
    pressure: Option<i32>,
}

pub struct TouchParser {
    setup: Panel,
    quirks: Quirks,
    next_id: u32,
    slots: Vec<Slot>,
    current: usize,
    partial: Partial,
    last: Point,
    frame: Vec<Point>,
    live: Vec<Live>,
    pressed: bool,
    pressure: i32,
    held_since: Option<Duration>,
    release_at: Option<Duration>,
    fading: bool,
}

impl TouchParser {
    pub fn new(setup: Panel) -> Self {
        Self {
            setup,
            quirks: Quirks::default(),
            next_id: 0,
            slots: Vec::new(),
            current: 0,
            partial: Partial::default(),
            last: Point::default(),
            frame: Vec::new(),
            live: Vec::new(),
            pressed: false,
            pressure: 0,
            held_since: None,
            release_at: None,
            fading: false,
        }
    }

    /// When [`TouchParser::tick`] has to run even if the device stays silent.
    pub fn deadline(&self) -> Option<Duration> {
        self.release_at
    }

    pub fn feed(&mut self, event: RawEvent, now: Duration, out: &mut Vec<Contact>) {
        match self.setup.protocol {
            Protocol::Slots => self.feed_slots(event, out),
            Protocol::Frames { tracked } => self.feed_frames(event, now, tracked, out),
            Protocol::Single => self.feed_single(event, out),
        }
    }

    pub fn tick(&mut self, now: Duration, out: &mut Vec<Contact>) {
        if self.release_at.is_some_and(|at| now >= at) {
            self.finish(Phase::End, out);
        }
    }

    /// Whether no finger is down.
    pub fn is_idle(&self) -> bool {
        self.live.is_empty() && self.slots.iter().all(|slot| slot.live.is_none())
    }

    /// Drops every contact, for a device that went away or lost events.
    pub fn cancel(&mut self, out: &mut Vec<Contact>) {
        self.finish(Phase::Cancel, out);
        self.slots.clear();
        self.frame.clear();
        self.partial = Partial::default();
        self.pressed = false;
    }

    /// Android's rule: a finger hovers while the touch button is up or it presses on nothing.
    fn hovers(&self, pressed: bool, pressure: i32, palm: bool) -> bool {
        (self.setup.button && !pressed) || (self.setup.pressure && pressure <= 0) || palm
    }

    /// Whether a device in this state, with its touch button `pressed` or not, has a finger on
    /// it. A slot that merely holds a tracking id does not: some drivers leave zero in slots
    /// that were never used.
    pub fn touching(&self, state: &AxisState, pressed: bool) -> bool {
        let column = |code: u16| state.values.iter().find(|(other, _)| *other == code).map(|(_, values)| values.as_slice()).unwrap_or_default();
        let (tracking, pressure, tool) = (column(ABS_MT_TRACKING_ID), column(ABS_MT_PRESSURE), column(ABS_MT_TOOL_TYPE));
        let value = |values: &[i32], slot: usize| values.get(slot).copied().unwrap_or(0);
        (0..tracking.len()).any(|slot| tracking[slot] >= 0 && !self.hovers(pressed, value(pressure, slot), value(tool, slot) == MT_TOOL_PALM))
    }

    /// Loads the slot state read straight from the device, and whether its touch button is down.
    pub fn restore_slots(&mut self, state: &AxisState, pressed: bool, out: &mut Vec<Contact>) {
        let column = |code: u16| state.values.iter().find(|(other, _)| *other == code).map(|(_, values)| values.as_slice()).unwrap_or_default();
        let (tracking, x, y, pressure, tool) =
            (column(ABS_MT_TRACKING_ID), column(ABS_MT_POSITION_X), column(ABS_MT_POSITION_Y), column(ABS_MT_PRESSURE), column(ABS_MT_TOOL_TYPE));
        let value = |values: &[i32], slot: usize| values.get(slot).copied().unwrap_or(0);
        self.slots = (0..tracking.len().min(MAX_CONTACTS))
            .map(|slot| Slot {
                tracking: tracking[slot],
                x: value(x, slot),
                y: value(y, slot),
                pressure: value(pressure, slot),
                palm: value(tool, slot) == MT_TOOL_PALM,
                ..EMPTY_SLOT
            })
            .collect();
        self.current = usize::try_from(state.slot).unwrap_or(0).min(MAX_CONTACTS - 1);
        self.pressed = pressed;
        self.flush_slots(out);
    }

    fn contact(&self, id: u32, phase: Phase, point: Point) -> Contact {
        Contact { id, phase, u: self.setup.x.normalize(point.x), v: self.setup.y.normalize(point.y) }
    }

    fn fresh_id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    fn finish(&mut self, phase: Phase, out: &mut Vec<Contact>) {
        for index in 0..self.slots.len() {
            if let Some((id, _)) = self.slots[index].live.take() {
                let slot = self.slots[index];
                out.push(self.contact(id, phase, Point { x: slot.x, y: slot.y, tracking: None }));
            }
        }
        for live in std::mem::take(&mut self.live) {
            out.push(self.contact(live.id, phase, live.point));
        }
        self.held_since = None;
        self.release_at = None;
        self.fading = false;
    }

    fn feed_slots(&mut self, event: RawEvent, out: &mut Vec<Contact>) {
        match (event.kind, event.code) {
            (EV_ABS, ABS_MT_SLOT) => self.current = event.value.clamp(0, MAX_CONTACTS as i32 - 1) as usize,
            (EV_ABS, ABS_MT_TRACKING_ID) => self.slot().tracking = event.value,
            (EV_ABS, ABS_MT_PRESSURE) => self.slot().pressure = event.value,
            (EV_ABS, ABS_MT_TOOL_TYPE) => self.slot().palm = event.value == MT_TOOL_PALM,
            (EV_KEY, BTN_TOUCH) => self.pressed = event.value != 0,
            (EV_ABS, ABS_MT_POSITION_X) => {
                let slot = self.slot();
                slot.x = event.value;
                slot.moved = true;
            }
            (EV_ABS, ABS_MT_POSITION_Y) => {
                let slot = self.slot();
                slot.y = event.value;
                slot.moved = true;
            }
            (EV_SYN, SYN_REPORT) => self.flush_slots(out),
            _ => {}
        }
    }

    fn slot(&mut self) -> &mut Slot {
        if self.slots.len() <= self.current {
            self.slots.resize(self.current + 1, EMPTY_SLOT);
        }
        &mut self.slots[self.current]
    }

    fn flush_slots(&mut self, out: &mut Vec<Contact>) {
        for index in 0..self.slots.len() {
            let slot = self.slots[index];
            let point = Point { x: slot.x, y: slot.y, tracking: None };
            let wanted = (slot.tracking >= 0 && !self.hovers(self.pressed, slot.pressure, slot.palm)).then_some(slot.tracking);
            let mut live = slot.live;
            match (live, wanted) {
                (Some((id, old)), Some(new)) if old == new => {
                    if slot.moved {
                        out.push(self.contact(id, Phase::Move, point));
                    }
                }
                (previous, wanted) => {
                    if let Some((id, _)) = previous {
                        out.push(self.contact(id, Phase::End, point));
                    }
                    live = wanted.map(|tracking| (self.fresh_id(), tracking));
                    if let Some((id, _)) = live {
                        out.push(self.contact(id, Phase::Start, point));
                    }
                }
            }
            self.slots[index].live = live;
            self.slots[index].moved = false;
        }
    }

    fn feed_frames(&mut self, event: RawEvent, now: Duration, tracked: bool, out: &mut Vec<Contact>) {
        match (event.kind, event.code) {
            (EV_ABS, ABS_MT_POSITION_X) => self.partial.x = Some(event.value),
            (EV_ABS, ABS_MT_POSITION_Y) => self.partial.y = Some(event.value),
            (EV_ABS, ABS_MT_TRACKING_ID) => self.partial.tracking = Some(event.value),
            (EV_ABS, ABS_MT_PRESSURE) => self.partial.pressure = Some(event.value),
            (EV_KEY, BTN_TOUCH) => self.pressed = event.value != 0,
            (EV_SYN, SYN_MT_REPORT) => self.push_partial(),
            (EV_SYN, SYN_REPORT) => {
                self.push_partial();
                self.tick(now, out);
                self.commit_frame(now, tracked, out);
            }
            _ => {}
        }
    }

    fn push_partial(&mut self) {
        let partial = std::mem::take(&mut self.partial);
        if partial.x.is_none() && partial.y.is_none() {
            return;
        }
        self.last = Point { x: partial.x.unwrap_or(self.last.x), y: partial.y.unwrap_or(self.last.y), tracking: partial.tracking };
        let hovering = self.setup.pressure && partial.pressure.is_some_and(|pressure| pressure <= 0);
        if !hovering && self.frame.len() < MAX_CONTACTS {
            self.frame.push(self.last);
        }
    }

    fn commit_frame(&mut self, now: Duration, tracked: bool, out: &mut Vec<Contact>) {
        let points = std::mem::take(&mut self.frame);
        let button = self.setup.button;
        let unreliable = !tracked && !button;

        if points.is_empty() || (button && !self.pressed) {
            if !unreliable || self.live.is_empty() {
                self.finish(Phase::End, out);
            } else if !std::mem::replace(&mut self.fading, true) {
                let long_hold = self.held_since.is_some_and(|since| now.saturating_sub(since) >= self.quirks.tap);
                self.release_at = Some(now + if long_hold { self.quirks.glitch } else { self.quirks.bounce });
            }
            self.frame = points;
            self.frame.clear();
            return;
        }

        let previous = std::mem::take(&mut self.live);
        let mut taken = [false; MAX_CONTACTS];
        let mut owner = [None; MAX_CONTACTS];
        if tracked {
            for (index, point) in points.iter().enumerate() {
                owner[index] = previous.iter().position(|live| live.point.tracking == point.tracking);
            }
        } else {
            for _ in 0..previous.len().min(points.len()) {
                let mut best: Option<(i64, usize, usize)> = None;
                for (live_index, live) in previous.iter().enumerate().filter(|(index, _)| !taken[*index]) {
                    for (point_index, point) in points.iter().enumerate().filter(|(index, _)| owner[*index].is_none()) {
                        let (dx, dy) = (i64::from(point.x - live.point.x), i64::from(point.y - live.point.y));
                        let distance = dx * dx + dy * dy;
                        if best.is_none_or(|(shortest, _, _)| distance < shortest) {
                            best = Some((distance, live_index, point_index));
                        }
                    }
                }
                let Some((_, live_index, point_index)) = best else { break };
                taken[live_index] = true;
                owner[point_index] = Some(live_index);
            }
        }
        for (index, _) in points.iter().enumerate() {
            if let Some(live_index) = owner[index] {
                taken[live_index] = true;
            }
        }

        for (index, live) in previous.iter().enumerate() {
            if !taken[index] {
                out.push(self.contact(live.id, Phase::End, live.point));
            }
        }
        for (index, &point) in points.iter().enumerate() {
            let id = match owner[index] {
                Some(live_index) => {
                    let live = previous[live_index];
                    if (live.point.x, live.point.y) != (point.x, point.y) {
                        out.push(self.contact(live.id, Phase::Move, point));
                    }
                    live.id
                }
                None => {
                    let id = self.fresh_id();
                    out.push(self.contact(id, Phase::Start, point));
                    id
                }
            };
            self.live.push(Live { id, point });
        }

        self.fading = false;
        self.held_since = self.held_since.filter(|_| !previous.is_empty()).or(Some(now));
        self.release_at = unreliable.then_some(now + self.quirks.silence);
        self.frame = points;
        self.frame.clear();
    }

    fn feed_single(&mut self, event: RawEvent, out: &mut Vec<Contact>) {
        match (event.kind, event.code) {
            (EV_ABS, ABS_X) => self.partial.x = Some(event.value),
            (EV_ABS, ABS_Y) => self.partial.y = Some(event.value),
            (EV_ABS, ABS_PRESSURE) => self.pressure = event.value,
            (EV_KEY, BTN_TOUCH) => self.pressed = event.value != 0,
            (EV_SYN, SYN_REPORT) => {
                let partial = std::mem::take(&mut self.partial);
                let moved = partial.x.is_some() || partial.y.is_some();
                self.last = Point { x: partial.x.unwrap_or(self.last.x), y: partial.y.unwrap_or(self.last.y), tracking: None };
                let touching = self.pressed && !(self.setup.pressure && self.pressure <= 0);
                match (self.live.first().copied(), touching) {
                    (None, true) => {
                        let id = self.fresh_id();
                        self.live.push(Live { id, point: self.last });
                        out.push(self.contact(id, Phase::Start, self.last));
                    }
                    (Some(live), true) if moved => {
                        self.live[0].point = self.last;
                        out.push(self.contact(live.id, Phase::Move, self.last));
                    }
                    (Some(_), false) => self.finish(Phase::End, out),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::classify::Confidence;
    use crate::window::Rotation;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn panel(protocol: Protocol, max_x: i32, max_y: i32) -> Panel {
        Panel {
            protocol,
            x: Axis { min: 0, max: max_x },
            y: Axis { min: 0, max: max_y },
            orientation: Rotation::Deg0,
            follows_display: true,
            confidence: Confidence::Certain,
            button: false,
            pressure: false,
        }
    }

    fn parser(protocol: Protocol, max_x: i32, max_y: i32) -> TouchParser {
        TouchParser::new(panel(protocol, max_x, max_y))
    }

    fn feed(parser: &mut TouchParser, now: Duration, events: &[(u16, u16, i32)]) -> Vec<Contact> {
        let mut out = Vec::new();
        for &(kind, code, value) in events {
            parser.feed(RawEvent { kind, code, value }, now, &mut out);
        }
        out
    }

    fn phases(contacts: &[Contact]) -> Vec<(u32, Phase)> {
        contacts.iter().map(|contact| (contact.id, contact.phase)).collect()
    }

    const SYN: (u16, u16, i32) = (EV_SYN, SYN_REPORT, 0);
    const MT: (u16, u16, i32) = (EV_SYN, SYN_MT_REPORT, 0);

    #[test]
    fn slots_follow_one_finger() {
        let mut parser = parser(Protocol::Slots, 15039, 23999);
        let down = feed(
            &mut parser,
            MS(0),
            &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 77), (EV_ABS, ABS_MT_POSITION_X, 7520), (EV_ABS, ABS_MT_POSITION_Y, 12000), (EV_KEY, BTN_TOUCH, 1), SYN],
        );
        assert_eq!(phases(&down), [(1, Phase::Start)]);
        assert!((down[0].u - 0.5).abs() < 1e-4 && (down[0].v - 0.5).abs() < 1e-4);

        let moved = feed(&mut parser, MS(8), &[(EV_ABS, ABS_MT_POSITION_X, 15039), SYN]);
        assert_eq!(phases(&moved), [(1, Phase::Move)]);
        assert!(moved[0].u > 0.9999 && moved[0].u < 1.0);

        assert!(feed(&mut parser, MS(16), &[SYN]).is_empty());
        let up = feed(&mut parser, MS(24), &[(EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_KEY, BTN_TOUCH, 0), SYN]);
        assert_eq!(phases(&up), [(1, Phase::End)]);
    }

    #[test]
    fn slots_track_fingers_independently() {
        let mut parser = parser(Protocol::Slots, 1000, 1000);
        feed(&mut parser, MS(0), &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 1), (EV_ABS, ABS_MT_POSITION_X, 100), (EV_ABS, ABS_MT_POSITION_Y, 100), SYN]);
        let second =
            feed(&mut parser, MS(5), &[(EV_ABS, ABS_MT_SLOT, 1), (EV_ABS, ABS_MT_TRACKING_ID, 2), (EV_ABS, ABS_MT_POSITION_X, 800), (EV_ABS, ABS_MT_POSITION_Y, 800), SYN]);
        assert_eq!(phases(&second), [(2, Phase::Start)]);

        let lifted = feed(&mut parser, MS(10), &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1), SYN]);
        assert_eq!(phases(&lifted), [(1, Phase::End)]);

        let replaced = feed(&mut parser, MS(15), &[(EV_ABS, ABS_MT_SLOT, 1), (EV_ABS, ABS_MT_TRACKING_ID, 9), SYN]);
        assert_eq!(phases(&replaced), [(2, Phase::End), (3, Phase::Start)]);
    }

    #[test]
    fn restored_slots_report_fingers_already_down() {
        let mut parser = parser(Protocol::Slots, 1000, 1000);
        let mut out = Vec::new();
        let held = AxisState {
            slot: 1,
            values: vec![(ABS_MT_TRACKING_ID, vec![-1, 5]), (ABS_MT_POSITION_X, vec![0, 500]), (ABS_MT_POSITION_Y, vec![0, 250]), (ABS_MT_PRESSURE, vec![0, 30])],
        };
        parser.restore_slots(&held, false, &mut out);
        assert_eq!(phases(&out), [(1, Phase::Start)]);
        assert!((out[0].u - 0.4995).abs() < 1e-3);

        let mut pressing = TouchParser::new(Panel { button: true, pressure: true, ..panel(Protocol::Slots, 1000, 1000) });
        let mut restored = Vec::new();
        pressing.restore_slots(&held, false, &mut restored);
        assert!(restored.is_empty() && pressing.is_idle());
        pressing.restore_slots(&held, true, &mut restored);
        assert_eq!(phases(&restored), [(1, Phase::Start)]);

        out.clear();
        parser.cancel(&mut out);
        assert_eq!(phases(&out), [(1, Phase::Cancel)]);
    }

    #[test]
    fn frames_release_a_tap_after_a_moment() {
        let mut parser = parser(Protocol::Frames { tracked: false }, 32767, 32767);
        let down = feed(&mut parser, MS(0), &[(EV_ABS, ABS_MT_POSITION_X, 16384), (EV_ABS, ABS_MT_POSITION_Y, 8192), MT, SYN]);
        assert_eq!(phases(&down), [(1, Phase::Start)]);
        assert!(feed(&mut parser, MS(1), &[(EV_ABS, ABS_MT_POSITION_X, 16384), (EV_ABS, ABS_MT_POSITION_Y, 8192), MT, SYN]).is_empty());

        assert!(feed(&mut parser, MS(66), &[MT, SYN]).is_empty());
        assert!(feed(&mut parser, MS(80), &[MT, SYN]).is_empty());
        assert_eq!(parser.deadline(), Some(MS(106)));
        let mut out = Vec::new();
        parser.tick(MS(106), &mut out);
        assert_eq!(phases(&out), [(1, Phase::End)]);
        assert_eq!(parser.deadline(), None);
    }

    #[test]
    fn frames_keep_a_fresh_contact_through_stray_empty_frames() {
        let mut parser = parser(Protocol::Frames { tracked: false }, 32767, 32767);
        let mut seen = Vec::new();
        for step in 0..20u64 {
            let x = 1000 + step as i32 * 500;
            seen.extend(feed(&mut parser, MS(step * 12), &[(EV_ABS, ABS_MT_POSITION_X, x), (EV_ABS, ABS_MT_POSITION_Y, 1000), MT, SYN]));
            for noise in [1, 4, 7, 10] {
                seen.extend(feed(&mut parser, MS(step * 12 + noise), &[MT, SYN]));
            }
        }
        assert_eq!(phases(&seen)[0], (1, Phase::Start));
        assert!(phases(&seen)[1..].iter().all(|&phase| phase == (1, Phase::Move)), "{:?}", phases(&seen));
    }

    #[test]
    fn frames_start_afresh_once_a_release_is_overdue() {
        let mut parser = parser(Protocol::Frames { tracked: false }, 32767, 32767);
        feed(&mut parser, MS(0), &[(EV_ABS, ABS_MT_POSITION_X, 1000), (EV_ABS, ABS_MT_POSITION_Y, 1000), MT, SYN]);
        feed(&mut parser, MS(50), &[MT, SYN]);
        let next = feed(&mut parser, MS(150), &[(EV_ABS, ABS_MT_POSITION_X, 1010), (EV_ABS, ABS_MT_POSITION_Y, 1000), MT, SYN]);
        assert_eq!(phases(&next), [(1, Phase::End), (2, Phase::Start)]);
    }

    #[test]
    fn frames_ride_out_a_dropped_contact() {
        let mut parser = parser(Protocol::Frames { tracked: false }, 32767, 32767);
        feed(&mut parser, MS(0), &[(EV_ABS, ABS_MT_POSITION_X, 1000), (EV_ABS, ABS_MT_POSITION_Y, 1000), MT, SYN]);
        feed(&mut parser, MS(300), &[(EV_ABS, ABS_MT_POSITION_X, 2000), (EV_ABS, ABS_MT_POSITION_Y, 1000), MT, SYN]);

        assert!(feed(&mut parser, MS(310), &[MT, SYN]).is_empty());
        assert_eq!(parser.deadline(), Some(MS(560)));

        let mut out = Vec::new();
        parser.tick(MS(400), &mut out);
        assert!(out.is_empty());

        let resumed = feed(&mut parser, MS(450), &[(EV_ABS, ABS_MT_POSITION_X, 2500), (EV_ABS, ABS_MT_POSITION_Y, 1000), MT, SYN]);
        assert_eq!(phases(&resumed), [(1, Phase::Move)]);

        assert!(feed(&mut parser, MS(460), &[MT, SYN]).is_empty());
        parser.tick(MS(709), &mut out);
        assert!(out.is_empty());
        parser.tick(MS(710), &mut out);
        assert_eq!(phases(&out), [(1, Phase::End)]);
    }

    #[test]
    fn frames_release_when_the_panel_goes_silent() {
        let mut parser = parser(Protocol::Frames { tracked: false }, 32767, 32767);
        feed(&mut parser, MS(0), &[(EV_ABS, ABS_MT_POSITION_X, 1000), (EV_ABS, ABS_MT_POSITION_Y, 1000), MT, SYN]);
        assert_eq!(parser.deadline(), Some(MS(1000)));
        let mut out = Vec::new();
        parser.tick(MS(1000), &mut out);
        assert_eq!(phases(&out), [(1, Phase::End)]);
    }

    #[test]
    fn frames_match_untracked_contacts_by_distance() {
        let mut parser = TouchParser::new(Panel { button: true, ..panel(Protocol::Frames { tracked: false }, 1000, 1000) });
        let down = feed(
            &mut parser,
            MS(0),
            &[
                (EV_KEY, BTN_TOUCH, 1),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 100),
                MT,
                (EV_ABS, ABS_MT_POSITION_X, 900),
                (EV_ABS, ABS_MT_POSITION_Y, 900),
                MT,
                SYN,
            ],
        );
        assert_eq!(phases(&down), [(1, Phase::Start), (2, Phase::Start)]);

        let swapped = feed(
            &mut parser,
            MS(10),
            &[(EV_ABS, ABS_MT_POSITION_X, 880), (EV_ABS, ABS_MT_POSITION_Y, 900), MT, (EV_ABS, ABS_MT_POSITION_X, 120), (EV_ABS, ABS_MT_POSITION_Y, 100), MT, SYN],
        );
        assert_eq!(phases(&swapped), [(2, Phase::Move), (1, Phase::Move)]);

        let one_left = feed(&mut parser, MS(20), &[(EV_ABS, ABS_MT_POSITION_X, 880), (EV_ABS, ABS_MT_POSITION_Y, 900), MT, SYN]);
        assert_eq!(phases(&one_left), [(1, Phase::End)]);

        let up = feed(&mut parser, MS(30), &[(EV_KEY, BTN_TOUCH, 0), MT, SYN]);
        assert_eq!(phases(&up), [(2, Phase::End)]);
    }

    #[test]
    fn a_finger_hovers_until_the_touch_button_goes_down() {
        let mut parser = TouchParser::new(Panel { button: true, ..panel(Protocol::Slots, 1000, 1000) });
        let near = feed(&mut parser, MS(0), &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 3), (EV_ABS, ABS_MT_POSITION_X, 400), (EV_ABS, ABS_MT_POSITION_Y, 400), SYN]);
        assert!(near.is_empty() && parser.is_idle());
        assert_eq!(phases(&feed(&mut parser, MS(10), &[(EV_KEY, BTN_TOUCH, 1), SYN])), [(1, Phase::Start)]);
        assert_eq!(phases(&feed(&mut parser, MS(20), &[(EV_ABS, ABS_MT_POSITION_X, 500), SYN])), [(1, Phase::Move)]);
        assert_eq!(phases(&feed(&mut parser, MS(30), &[(EV_KEY, BTN_TOUCH, 0), SYN])), [(1, Phase::End)]);
        assert!(feed(&mut parser, MS(40), &[(EV_ABS, ABS_MT_POSITION_X, 600), SYN]).is_empty());
        assert_eq!(phases(&feed(&mut parser, MS(50), &[(EV_KEY, BTN_TOUCH, 1), SYN])), [(2, Phase::Start)]);
    }

    #[test]
    fn slots_a_driver_left_at_zero_are_not_fingers() {
        let by_pressure = TouchParser::new(Panel { pressure: true, ..panel(Protocol::Slots, 1000, 1000) });
        let mut state = AxisState { slot: 0, values: vec![(ABS_MT_TRACKING_ID, vec![0; 3]), (ABS_MT_PRESSURE, vec![0; 3]), (ABS_MT_TOOL_TYPE, vec![0; 3])] };
        assert!(!by_pressure.touching(&state, false));
        state.values[1].1[2] = 400;
        assert!(by_pressure.touching(&state, false));
        state.values[2].1[2] = MT_TOOL_PALM;
        assert!(!by_pressure.touching(&state, false));

        let by_button = TouchParser::new(Panel { button: true, ..panel(Protocol::Slots, 1000, 1000) });
        let held = AxisState { slot: 0, values: vec![(ABS_MT_TRACKING_ID, vec![-1, 7, -1])] };
        assert!(!by_button.touching(&held, false) && by_button.touching(&held, true));
        assert!(!by_button.touching(&AxisState { slot: 0, values: Vec::new() }, true));
    }

    #[test]
    fn a_finger_without_pressure_or_marked_as_a_palm_is_not_a_touch() {
        let mut parser = TouchParser::new(Panel { pressure: true, ..panel(Protocol::Slots, 1000, 1000) });
        let light = feed(&mut parser, MS(0), &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 3), (EV_ABS, ABS_MT_POSITION_X, 400), (EV_ABS, ABS_MT_POSITION_Y, 400), SYN]);
        assert!(light.is_empty());
        assert_eq!(phases(&feed(&mut parser, MS(10), &[(EV_ABS, ABS_MT_PRESSURE, 50), SYN])), [(1, Phase::Start)]);
        assert_eq!(phases(&feed(&mut parser, MS(20), &[(EV_ABS, ABS_MT_TOOL_TYPE, MT_TOOL_PALM), SYN])), [(1, Phase::End)]);
        assert_eq!(phases(&feed(&mut parser, MS(30), &[(EV_ABS, ABS_MT_TOOL_TYPE, 0), SYN])), [(2, Phase::Start)]);
        assert_eq!(phases(&feed(&mut parser, MS(40), &[(EV_ABS, ABS_MT_PRESSURE, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1), SYN])), [(2, Phase::End)]);
    }

    #[test]
    fn frames_with_a_touch_button_release_at_once_and_ignore_stray_positions() {
        let mut parser = TouchParser::new(Panel { button: true, ..panel(Protocol::Frames { tracked: false }, 1080, 1920) });
        let down = feed(&mut parser, MS(0), &[(EV_KEY, BTN_TOUCH, 1), (EV_ABS, ABS_MT_PRESSURE, 1), (EV_ABS, ABS_MT_POSITION_X, 540), (EV_ABS, ABS_MT_POSITION_Y, 960), MT, SYN]);
        assert_eq!(phases(&down), [(1, Phase::Start)]);
        let up = feed(&mut parser, MS(30), &[(EV_KEY, BTN_TOUCH, 0), (EV_ABS, ABS_MT_PRESSURE, 0), (EV_ABS, ABS_MT_POSITION_X, 540), (EV_ABS, ABS_MT_POSITION_Y, 960), MT, SYN]);
        assert_eq!(phases(&up), [(1, Phase::End)]);
        assert_eq!(parser.deadline(), None);
        assert!(feed(&mut parser, MS(60), &[(EV_ABS, ABS_MT_POSITION_X, 100), (EV_ABS, ABS_MT_POSITION_Y, 100), (EV_REL, REL_WHEEL, 1), SYN]).is_empty());
    }

    #[test]
    fn every_slot_of_a_wide_panel_is_its_own_finger() {
        let mut parser = parser(Protocol::Slots, 1080, 1920);
        let mut seen = Vec::new();
        for slot in [0, 15, 16, 31] {
            seen.extend(feed(
                &mut parser,
                MS(0),
                &[(EV_ABS, ABS_MT_SLOT, slot), (EV_ABS, ABS_MT_TRACKING_ID, slot + 100), (EV_ABS, ABS_MT_POSITION_X, slot * 10), (EV_ABS, ABS_MT_POSITION_Y, 500), SYN],
            ));
        }
        assert_eq!(phases(&seen), [(1, Phase::Start), (2, Phase::Start), (3, Phase::Start), (4, Phase::Start)]);
    }

    #[test]
    fn single_touch_uses_the_touch_button() {
        let mut parser = parser(Protocol::Single, 4095, 4095);
        let down = feed(&mut parser, MS(0), &[(EV_ABS, ABS_X, 2048), (EV_ABS, ABS_Y, 1024), (EV_KEY, BTN_TOUCH, 1), SYN]);
        assert_eq!(phases(&down), [(1, Phase::Start)]);
        assert!((down[0].u - 0.5).abs() < 1e-3 && (down[0].v - 0.25).abs() < 1e-3);
        assert_eq!(phases(&feed(&mut parser, MS(5), &[(EV_ABS, ABS_X, 3000), SYN])), [(1, Phase::Move)]);
        assert_eq!(phases(&feed(&mut parser, MS(9), &[(EV_KEY, BTN_TOUCH, 0), SYN])), [(1, Phase::End)]);
    }
}
