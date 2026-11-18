use std::collections::HashMap;
use std::time::Instant;

use super::device::MouseButton;
use super::touch::{Contact, Phase};
use crate::window::Rotation;

/// A rectangle in display pixels: `[left, top, right, bottom]`.
pub type Area = [f32; 4];

fn contains(area: Area, [x, y]: [f32; 2]) -> bool {
    x >= area[0] && y >= area[1] && x < area[2] && y < area[3]
}

/// What the interface looked like after the last frame, in display pixels.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub size: [f32; 2],
    pub rotation: Rotation,
    pub regions: Vec<Area>,
    pub unused: Option<Area>,
    pub keyboard: Option<Area>,
    pub dismissable: bool,
    pub wants_keys: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Interface,
    Keyboard,
    Outside,
}

impl View {
    pub fn target(&self, position: [f32; 2]) -> Target {
        if self.keyboard.is_some_and(|area| contains(area, position)) {
            Target::Keyboard
        } else if self.regions.iter().any(|&area| contains(area, position)) || self.unused.is_some_and(|area| !contains(area, position)) {
            Target::Interface
        } else {
            Target::Outside
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    Touch { device: u32, id: u32, phase: Phase, position: [f32; 2] },
    PointerMoved([f32; 2]),
    PointerButton { position: [f32; 2], button: MouseButton, pressed: bool },
    PointerGone,
    Wheel { x: f32, y: f32 },
    Key { code: u16, pressed: bool, repeat: bool },
    KeyboardTouch { id: u64, phase: Phase, position: [f32; 2] },
}

const MOUSE_KEYBOARD_ID: u64 = u64::MAX;

/// Decides which touches belong to the interface. A contact is judged once, where it lands:
/// fingers resting on whatever is underneath never reach egui, however long they stay down.
#[derive(Default)]
pub struct Router {
    routes: HashMap<(u32, u32), Target>,
    owner: Option<(u32, u32)>,
    mouse: [f32; 2],
    hovering: bool,
    held: [bool; 3],
    typing: bool,
}

impl Router {
    /// Returns where the contact went: the same answer for its whole life.
    pub fn touch(&mut self, device: u32, contact: Contact, position: [f32; 2], view: &View, out: &mut Vec<Event>) -> Target {
        let key = (device, contact.id);
        let keyboard_id = u64::from(device) << 32 | u64::from(contact.id);
        let target = match contact.phase {
            Phase::Start => view.target(position),
            Phase::Move => self.routes.get(&key).copied().unwrap_or(Target::Outside),
            Phase::End | Phase::Cancel => self.routes.get(&key).copied().unwrap_or(Target::Outside),
        };
        match contact.phase {
            Phase::Start => {
                self.routes.insert(key, target);
                match target {
                    Target::Interface => {
                        out.push(Event::Touch { device, id: contact.id, phase: Phase::Start, position });
                        if self.owner.is_none() && !self.dragging() {
                            self.owner = Some(key);
                            self.mouse = position;
                            out.push(Event::PointerMoved(position));
                            out.push(Event::PointerButton { position, button: MouseButton::Left, pressed: true });
                        }
                    }
                    Target::Keyboard => out.push(Event::KeyboardTouch { id: keyboard_id, phase: Phase::Start, position }),
                    Target::Outside => self.dismiss(position, view, out),
                }
            }
            Phase::Move => match self.routes.get(&key) {
                Some(Target::Interface) => {
                    out.push(Event::Touch { device, id: contact.id, phase: Phase::Move, position });
                    if self.owner == Some(key) {
                        self.mouse = position;
                        out.push(Event::PointerMoved(position));
                    }
                }
                Some(Target::Keyboard) => out.push(Event::KeyboardTouch { id: keyboard_id, phase: Phase::Move, position }),
                _ => {}
            },
            Phase::End | Phase::Cancel => match self.routes.remove(&key) {
                Some(Target::Interface) => {
                    out.push(Event::Touch { device, id: contact.id, phase: contact.phase, position });
                    if self.owner == Some(key) {
                        self.owner = None;
                        self.hovering = false;
                        if contact.phase == Phase::End {
                            out.push(Event::PointerButton { position, button: MouseButton::Left, pressed: false });
                        }
                        out.push(Event::PointerGone);
                    }
                }
                Some(Target::Keyboard) => out.push(Event::KeyboardTouch { id: keyboard_id, phase: contact.phase, position }),
                _ => {}
            },
        }
        target
    }

    pub fn hover(&mut self, position: [f32; 2], view: &View, out: &mut Vec<Event>) {
        self.mouse = position;
        if self.owner.is_some() {
            return;
        }
        if self.typing {
            out.push(Event::KeyboardTouch { id: MOUSE_KEYBOARD_ID, phase: Phase::Move, position });
        } else if self.dragging() || view.target(position) == Target::Interface {
            self.hovering = true;
            out.push(Event::PointerMoved(position));
        } else if std::mem::take(&mut self.hovering) {
            out.push(Event::PointerGone);
        }
    }

    pub fn button(&mut self, button: MouseButton, pressed: bool, view: &View, out: &mut Vec<Event>) {
        let position = self.mouse;
        let index = button as usize;
        if !pressed {
            if std::mem::take(&mut self.held[index]) {
                out.push(Event::PointerButton { position, button, pressed: false });
            } else if button == MouseButton::Left && std::mem::take(&mut self.typing) {
                out.push(Event::KeyboardTouch { id: MOUSE_KEYBOARD_ID, phase: Phase::End, position });
            }
            return;
        }
        if self.owner.is_some() {
            return;
        }
        match view.target(position) {
            Target::Interface => {
                self.held[index] = true;
                self.hovering = true;
                out.push(Event::PointerMoved(position));
                out.push(Event::PointerButton { position, button, pressed: true });
            }
            Target::Keyboard if button == MouseButton::Left => {
                self.typing = true;
                out.push(Event::KeyboardTouch { id: MOUSE_KEYBOARD_ID, phase: Phase::Start, position });
            }
            Target::Outside if button == MouseButton::Left => self.dismiss(position, view, out),
            _ => {}
        }
    }

    /// Scrolls whatever the pointer is over. Emulators that turn the mouse into touches send
    /// the wheel without a pointer; it then applies where the last touch was.
    pub fn wheel(&mut self, x: f32, y: f32, view: &View, out: &mut Vec<Event>) {
        if view.target(self.mouse) == Target::Interface {
            if self.owner.is_none() && !std::mem::replace(&mut self.hovering, true) {
                out.push(Event::PointerMoved(self.mouse));
            }
            out.push(Event::Wheel { x, y });
        }
    }

    pub fn key(&mut self, code: u16, pressed: bool, repeat: bool, view: &View, out: &mut Vec<Event>) {
        if view.wants_keys || !pressed {
            out.push(Event::Key { code, pressed, repeat });
        }
    }

    fn dragging(&self) -> bool {
        self.held.iter().any(|&held| held)
    }

    /// A press outside the interface still has to close popups and take focus off text fields.
    fn dismiss(&mut self, position: [f32; 2], view: &View, out: &mut Vec<Event>) {
        if view.dismissable && self.owner.is_none() && !self.dragging() {
            out.push(Event::PointerMoved(position));
            out.push(Event::PointerButton { position, button: MouseButton::Left, pressed: true });
            out.push(Event::PointerButton { position, button: MouseButton::Left, pressed: false });
            out.push(Event::PointerGone);
            self.hovering = false;
        }
    }
}

/// Events waiting for the next frame, each with the moment it happened. Runs of movement
/// collapse to their latest position, so a stalled frame or a 1000 Hz panel cannot flood it;
/// presses and releases are never lost.
#[derive(Default)]
pub struct Queue {
    events: Vec<Event>,
    times: Vec<Instant>,
}

impl Queue {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Takes the events for one frame, cut so that egui sees a drag whole. It decides what a
    /// press landed on from where the pointer is once the frame's input is in, and it drops
    /// the movement of the frame a drag ends in; so a press ends its frame, and a release
    /// waits for the next one when movement came before it. Returns when the last of the
    /// frame's events happened.
    pub fn take_frame(&mut self, into: &mut Vec<Event>) -> Option<Instant> {
        let button = |pressed: bool| move |event: &Event| matches!(event, Event::PointerButton { pressed: held, .. } if *held == pressed);
        let mut end = self.events.iter().position(button(true)).map_or(self.events.len(), |press| press + 1);
        if let Some(mut release) = self.events[..end].iter().position(button(false)) {
            release -= usize::from(release > 0 && matches!(self.events[release - 1], Event::Touch { phase: Phase::End | Phase::Cancel, .. }));
            if self.events[..release].iter().any(|event| matches!(event, Event::PointerMoved(_))) {
                end = release;
            }
        }
        into.extend(self.events.drain(..end));
        self.times.drain(..end).max()
    }

    pub fn push(&mut self, event: Event, at: Instant) {
        let same_motion = |queued: &Event| match (queued, &event) {
            (Event::Touch { device: a, id: b, phase: Phase::Move, .. }, Event::Touch { device: c, id: d, phase: Phase::Move, .. }) => a == c && b == d,
            (Event::KeyboardTouch { id: a, phase: Phase::Move, .. }, Event::KeyboardTouch { id: b, phase: Phase::Move, .. }) => a == b,
            (Event::PointerMoved(_), Event::PointerMoved(_)) => true,
            _ => false,
        };
        let is_motion = |queued: &Event| matches!(queued, Event::Touch { phase: Phase::Move, .. } | Event::KeyboardTouch { phase: Phase::Move, .. } | Event::PointerMoved(_));

        if is_motion(&event) {
            for (queued, time) in self.events.iter_mut().zip(&mut self.times).rev().take_while(|(queued, _)| is_motion(queued)) {
                if same_motion(queued) {
                    (*queued, *time) = (event, at);
                    return;
                }
            }
        }
        self.events.push(event);
        self.times.push(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View { size: [2400.0, 1504.0], regions: vec![[100.0, 100.0, 500.0, 400.0]], keyboard: Some([0.0, 1000.0, 2400.0, 1504.0]), ..Default::default() }
    }

    fn contact(id: u32, phase: Phase) -> Contact {
        Contact { id, phase, u: 0.0, v: 0.0 }
    }

    fn route(router: &mut Router, id: u32, phase: Phase, position: [f32; 2], view: &View) -> Vec<Event> {
        let mut out = Vec::new();
        router.touch(0, contact(id, phase), position, view, &mut out);
        out
    }

    #[test]
    fn finger_resting_outside_never_reaches_the_interface() {
        let (view, mut router) = (view(), Router::default());
        assert!(route(&mut router, 1, Phase::Start, [1500.0, 700.0], &view).is_empty());
        assert!(route(&mut router, 1, Phase::Move, [300.0, 200.0], &view).is_empty());

        let tap = route(&mut router, 2, Phase::Start, [300.0, 200.0], &view);
        assert_eq!(
            tap,
            [
                Event::Touch { device: 0, id: 2, phase: Phase::Start, position: [300.0, 200.0] },
                Event::PointerMoved([300.0, 200.0]),
                Event::PointerButton { position: [300.0, 200.0], button: MouseButton::Left, pressed: true },
            ]
        );
        let lift = route(&mut router, 2, Phase::End, [310.0, 200.0], &view);
        assert_eq!(lift.len(), 3);
        assert_eq!(lift[1], Event::PointerButton { position: [310.0, 200.0], button: MouseButton::Left, pressed: false });
        assert_eq!(lift[2], Event::PointerGone);
        assert!(route(&mut router, 1, Phase::End, [300.0, 200.0], &view).is_empty());
    }

    #[test]
    fn a_drag_keeps_its_finger_outside_the_window() {
        let (view, mut router) = (view(), Router::default());
        route(&mut router, 1, Phase::Start, [300.0, 200.0], &view);
        let moved = route(&mut router, 1, Phase::Move, [2000.0, 900.0], &view);
        assert_eq!(moved[1], Event::PointerMoved([2000.0, 900.0]));

        let second = route(&mut router, 2, Phase::Start, [120.0, 120.0], &view);
        assert_eq!(second, [Event::Touch { device: 0, id: 2, phase: Phase::Start, position: [120.0, 120.0] }]);
        let cancelled = route(&mut router, 1, Phase::Cancel, [2000.0, 900.0], &view);
        assert_eq!(cancelled.last(), Some(&Event::PointerGone));
        assert!(!cancelled.iter().any(|event| matches!(event, Event::PointerButton { .. })));
    }

    #[test]
    fn keyboard_touches_stay_away_from_the_interface() {
        let (view, mut router) = (view(), Router::default());
        let press = route(&mut router, 7, Phase::Start, [50.0, 1200.0], &view);
        assert_eq!(press, [Event::KeyboardTouch { id: 7, phase: Phase::Start, position: [50.0, 1200.0] }]);
        let release = route(&mut router, 7, Phase::End, [50.0, 1200.0], &view);
        assert_eq!(release, [Event::KeyboardTouch { id: 7, phase: Phase::End, position: [50.0, 1200.0] }]);
    }

    #[test]
    fn outside_press_dismisses_only_when_asked() {
        let (mut view, mut router) = (view(), Router::default());
        assert!(route(&mut router, 1, Phase::Start, [1500.0, 700.0], &view).is_empty());
        view.dismissable = true;
        let dismissed = route(&mut router, 2, Phase::Start, [1500.0, 700.0], &view);
        assert_eq!(dismissed.len(), 4);
        assert_eq!(dismissed[3], Event::PointerGone);
    }

    #[test]
    fn background_counts_once_the_root_is_used() {
        let mut view = View { size: [1000.0, 1000.0], ..Default::default() };
        assert_eq!(view.target([10.0, 10.0]), Target::Outside);
        view.unused = Some([0.0, 100.0, 1000.0, 1000.0]);
        assert_eq!(view.target([10.0, 10.0]), Target::Interface);
        assert_eq!(view.target([10.0, 500.0]), Target::Outside);
    }

    #[test]
    fn the_wheel_scrolls_where_the_last_touch_was() {
        let (view, mut router) = (view(), Router::default());
        let mut out = Vec::new();
        for phase in [Phase::Start, Phase::End] {
            router.touch(0, Contact { id: 1, phase, u: 0.0, v: 0.0 }, [300.0, 200.0], &view, &mut out);
        }
        out.clear();
        router.wheel(0.0, -1.0, &view, &mut out);
        router.wheel(0.0, -1.0, &view, &mut out);
        assert_eq!(out, [Event::PointerMoved([300.0, 200.0]), Event::Wheel { x: 0.0, y: -1.0 }, Event::Wheel { x: 0.0, y: -1.0 }]);
    }

    #[test]
    fn mouse_hovers_only_over_the_interface() {
        let (view, mut router) = (view(), Router::default());
        let mut out = Vec::new();
        router.hover([1500.0, 700.0], &view, &mut out);
        router.wheel(0.0, 1.0, &view, &mut out);
        assert!(out.is_empty());

        router.hover([300.0, 200.0], &view, &mut out);
        router.wheel(0.0, 1.0, &view, &mut out);
        router.button(MouseButton::Left, true, &view, &mut out);
        router.hover([1500.0, 700.0], &view, &mut out);
        router.button(MouseButton::Left, false, &view, &mut out);
        router.hover([1600.0, 700.0], &view, &mut out);
        assert_eq!(
            out,
            [
                Event::PointerMoved([300.0, 200.0]),
                Event::Wheel { x: 0.0, y: 1.0 },
                Event::PointerMoved([300.0, 200.0]),
                Event::PointerButton { position: [300.0, 200.0], button: MouseButton::Left, pressed: true },
                Event::PointerMoved([1500.0, 700.0]),
                Event::PointerButton { position: [1500.0, 700.0], button: MouseButton::Left, pressed: false },
                Event::PointerGone,
            ]
        );
    }

    fn frame(queue: &mut Queue) -> Vec<Event> {
        let mut events = Vec::new();
        queue.take_frame(&mut events);
        events
    }

    #[test]
    fn queue_keeps_the_latest_motion_and_every_press() {
        let (mut queue, now) = (Queue::default(), Instant::now());
        let touch = |id, phase, x| Event::Touch { device: 0, id, phase, position: [x, 0.0] };
        queue.push(touch(1, Phase::Start, 0.0), now);
        for step in 1..=100 {
            queue.push(touch(1, Phase::Move, step as f32), now);
            queue.push(Event::PointerMoved([step as f32, 0.0]), now);
            queue.push(touch(2, Phase::Move, -(step as f32)), now);
        }
        queue.push(touch(1, Phase::End, 100.0), now);
        queue.push(touch(1, Phase::Move, 5.0), now);

        let events = frame(&mut queue);
        assert_eq!(
            events,
            [
                touch(1, Phase::Start, 0.0),
                touch(1, Phase::Move, 100.0),
                Event::PointerMoved([100.0, 0.0]),
                touch(2, Phase::Move, -100.0),
                touch(1, Phase::End, 100.0),
                touch(1, Phase::Move, 5.0)
            ]
        );
    }

    #[test]
    fn frames_are_cut_after_a_press_and_before_a_release_that_follows_movement() {
        let (mut queue, now) = (Queue::default(), Instant::now());
        let press = |pressed| Event::PointerButton { position: [10.0, 10.0], button: MouseButton::Left, pressed };
        let touch = |phase| Event::Touch { device: 0, id: 1, phase, position: [10.0, 10.0] };
        let drag = [
            touch(Phase::Start),
            Event::PointerMoved([10.0, 10.0]),
            press(true),
            touch(Phase::Move),
            Event::PointerMoved([90.0, 90.0]),
            touch(Phase::End),
            press(false),
            Event::PointerGone,
        ];
        drag.into_iter().for_each(|event| queue.push(event, now));
        assert_eq!(frame(&mut queue), drag[..3]);
        assert_eq!(frame(&mut queue), drag[3..5]);
        assert_eq!(frame(&mut queue), drag[5..]);

        let tap = [touch(Phase::Start), Event::PointerMoved([10.0, 10.0]), press(true), touch(Phase::End), press(false), Event::PointerGone];
        tap.into_iter().for_each(|event| queue.push(event, now));
        assert_eq!(frame(&mut queue), tap[..3]);
        assert_eq!(frame(&mut queue), tap[3..]);
        assert!(queue.is_empty());
    }

    #[test]
    fn a_frame_is_as_old_as_its_latest_event() {
        let (mut queue, start) = (Queue::default(), Instant::now());
        let later = |millis| start + std::time::Duration::from_millis(millis);
        let press = |pressed| Event::PointerButton { position: [10.0, 10.0], button: MouseButton::Left, pressed };
        queue.push(Event::PointerMoved([10.0, 10.0]), later(0));
        queue.push(press(true), later(5));
        queue.push(Event::PointerMoved([20.0, 10.0]), later(30));
        queue.push(Event::PointerMoved([30.0, 10.0]), later(60));
        queue.push(press(false), later(70));

        let mut events = Vec::new();
        assert_eq!(queue.take_frame(&mut events), Some(later(5)));
        assert_eq!(queue.take_frame(&mut events), Some(later(60)));
        assert_eq!(queue.take_frame(&mut events), Some(later(70)));
        assert_eq!(queue.take_frame(&mut events), None);
    }
}
