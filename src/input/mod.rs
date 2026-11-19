//! Input. Touchscreens, mice and keyboards are read straight from the kernel, sorted into what
//! is the interface's and what belongs to whatever lies under it, and handed to the frames.
//! [`Touch::init`] starts all of it; the [`Touch`] it returns is the handle to it afterwards.

mod device;
mod keyboard;
#[cfg(target_os = "android")]
mod reader;
mod router;
mod touch;

use egui::{Modifiers, MouseWheelSource, MouseWheelUnit, PointerButton, TouchDeviceId, TouchId, TouchPhase, pos2, vec2};

use device::MouseButton;
pub(crate) use keyboard::Clipboard;
pub use keyboard::{Keyboard, Language};
#[cfg(target_os = "android")]
pub(crate) use reader::Feed;
use router::Event;
pub use touch::Touch;
pub(crate) use touch::{Motion, Phase};

/// Tells egui a touch screen is present before the first finger lands. egui only arms
/// drag-to-scroll once it has seen a touch, so without this the first swipe would not scroll.
pub(crate) fn announce_touch_screen() -> egui::Event {
    egui::Event::Touch { device_id: TouchDeviceId(0), id: TouchId(u64::MAX), phase: TouchPhase::Cancel, pos: pos2(0.0, 0.0), force: None }
}

/// Converts a routed pointer event, given in display pixels, into the event egui expects in points.
/// Keys and touches on the on-screen keyboard go to [`crate::Keyboard`] instead.
pub(crate) fn to_egui(event: Event, points_per_pixel: f32, modifiers: Modifiers) -> Option<egui::Event> {
    let point = |[x, y]: [f32; 2]| pos2(x * points_per_pixel, y * points_per_pixel);
    Some(match event {
        Event::Touch { device, id, phase, position } => egui::Event::Touch {
            device_id: TouchDeviceId(u64::from(device)),
            id: TouchId(u64::from(id)),
            phase: match phase {
                Phase::Start => TouchPhase::Start,
                Phase::Move => TouchPhase::Move,
                Phase::End => TouchPhase::End,
                Phase::Cancel => TouchPhase::Cancel,
            },
            pos: point(position),
            force: None,
        },
        Event::PointerMoved(position) => egui::Event::PointerMoved(point(position)),
        Event::PointerButton { position, button, pressed } => egui::Event::PointerButton {
            pos: point(position),
            button: match button {
                MouseButton::Left => PointerButton::Primary,
                MouseButton::Right => PointerButton::Secondary,
                MouseButton::Middle => PointerButton::Middle,
            },
            pressed,
            modifiers,
        },
        Event::PointerGone => egui::Event::PointerGone,
        Event::Wheel { x, y } => egui::Event::MouseWheel { unit: MouseWheelUnit::Line, delta: vec2(-x, y), phase: TouchPhase::Move, source: MouseWheelSource::Wheel, modifiers },
        Event::Key { .. } | Event::KeyboardTouch { .. } => return None,
    })
}

/// The time egui is given for a frame, in seconds: when the frame's input happened, or now for
/// a frame without any, and never earlier than the frame before. egui tells a click from a hold
/// by the clock of its frames, so a tap has to stay as short as it was however long the frames
/// around it took to draw.
pub(crate) fn frame_time(previous: f64, happened: Option<f64>, now: f64) -> f64 {
    happened.map_or(now, |at| at.min(now)).max(previous)
}

#[cfg(test)]
mod tests {
    use egui::{Context, RawInput, Rect, ScrollArea, Window};

    use super::router::{Queue, Router, View};
    use super::touch::Contact;
    use super::*;

    struct Harness {
        context: Context,
        router: Router,
        view: View,
        time: f64,
        offset: f32,
        clicks: u32,
    }

    impl Harness {
        fn new() -> Self {
            let view = View { size: [1000.0, 1000.0], regions: vec![[0.0, 0.0, 1000.0, 1000.0]], ..Default::default() };
            let mut harness = Self { context: Context::default(), router: Router::default(), view, time: 0.0, offset: 0.0, clicks: 0 };
            harness.frame(&[]);
            harness.frame(&[]);
            harness
        }

        fn frame(&mut self, events: &[Event]) {
            self.frame_at(self.time + 1.0 / 60.0, events);
        }

        fn frame_at(&mut self, time: f64, events: &[Event]) {
            let mut events: Vec<egui::Event> = events.iter().filter_map(|&event| to_egui(event, 1.0, Modifiers::default())).collect();
            if self.time == 0.0 {
                events.push(announce_touch_screen());
            }
            self.time = time;
            let input = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 1000.0))), time: Some(self.time), events, ..Default::default() };
            let (mut offset, mut clicks) = (self.offset, self.clicks);
            let mut output = self.context.run_ui(input, |ui| {
                Window::new("list").fixed_pos([100.0, 100.0]).show(ui.ctx(), |ui| {
                    if ui.add_sized([300.0, 60.0], egui::Button::new("tap")).clicked() {
                        clicks += 1;
                    }
                    let output = ScrollArea::vertical().max_height(300.0).auto_shrink(false).show(ui, |ui| {
                        for row in 0..100 {
                            ui.label(format!("row {row}"));
                        }
                    });
                    offset = output.state.clamped_offset().y;
                });
            });
            output.textures_delta.clear();
            (self.offset, self.clicks) = (offset, clicks);
        }

        fn touch(&mut self, phase: Phase, position: [f32; 2]) {
            let events = self.route(phase, position);
            self.frame(&events);
        }

        fn route(&mut self, phase: Phase, position: [f32; 2]) -> Vec<Event> {
            let mut events = Vec::new();
            self.router.touch(0, Contact { id: 1, phase, u: 0.0, v: 0.0 }, position, &self.view, &mut events);
            events
        }

        /// A finger that goes down at `pressed` and up at `released`, seen by frames that take
        /// `frame` seconds each to draw: the release waits its turn behind the press's frame.
        fn slow_tap(&mut self, pressed: f64, released: f64, frame: f64) {
            let (down, up) = (self.route(Phase::Start, [200.0, 160.0]), self.route(Phase::End, [200.0, 160.0]));
            let start = self.time.max(pressed);
            self.frame_at(frame_time(self.time, Some(pressed), start), &down);
            let turn = (start + frame).max(released);
            self.frame_at(frame_time(self.time, Some(released), turn), &up);
            self.frame_at(frame_time(self.time, None, turn + frame), &[]);
        }
    }

    /// A whole drag queued before a single frame ran, as after a stall: the finger leaves the
    /// title bar at once and lifts 200 points to the left.
    #[test]
    fn a_drag_that_outruns_the_frames_still_moves_the_window() {
        let context = Context::default();
        let view = View { size: [1000.0, 1000.0], regions: vec![[0.0, 0.0, 1000.0, 1000.0]], ..Default::default() };
        let (mut router, mut queue, mut time, mut left) = (Router::default(), Queue::default(), 0.0, 0.0);
        let mut frame = |queue: &mut Queue| {
            time += 1.0 / 60.0;
            let mut taken = Vec::new();
            queue.take_frame(&mut taken);
            let mut events: Vec<egui::Event> = taken.into_iter().filter_map(|event| to_egui(event, 1.0, Modifiers::default())).collect();
            events.extend((time < 0.02).then(announce_touch_screen));
            let input = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 1000.0))), time: Some(time), events, ..Default::default() };
            let mut output = context.run_ui(input, |ui| {
                let window = Window::new("drag me").default_pos([400.0, 400.0]).show(ui.ctx(), |ui| ui.label("content"));
                left = window.map_or(f32::NAN, |window| window.response.rect.left());
            });
            output.textures_delta.clear();
            left
        };
        frame(&mut queue);
        let start = frame(&mut queue);

        let mut events = Vec::new();
        for (phase, position) in [(Phase::Start, [440.0, 412.0]), (Phase::Move, [300.0, 200.0]), (Phase::Move, [240.0, 200.0]), (Phase::End, [240.0, 200.0])] {
            router.touch(0, Contact { id: 1, phase, u: 0.0, v: 0.0 }, position, &view, &mut events);
        }
        let now = std::time::Instant::now();
        events.drain(..).for_each(|event| queue.push(event, now));
        while !queue.is_empty() {
            frame(&mut queue);
        }
        let end = frame(&mut queue);
        assert!((start - end - 200.0).abs() < 1.0, "window went from {start} to {end}");
    }

    #[test]
    fn a_tap_clicks_the_button() {
        let mut harness = Harness::new();
        harness.touch(Phase::Start, [200.0, 160.0]);
        harness.touch(Phase::End, [200.0, 160.0]);
        harness.frame(&[]);
        assert_eq!(harness.clicks, 1);
    }

    /// Frames that take over a second each, as with software drawing on a loaded emulator:
    /// a tap of fifty milliseconds is still a click, and a finger held for two seconds is not.
    #[test]
    fn a_tap_clicks_however_slow_the_frames_are() {
        let mut harness = Harness::new();
        harness.slow_tap(1.0, 1.05, 1.2);
        assert_eq!(harness.clicks, 1);
        harness.slow_tap(5.0, 7.0, 1.2);
        assert_eq!(harness.clicks, 1);
    }

    #[test]
    fn frame_time_follows_the_input_and_never_runs_backwards() {
        assert_eq!(frame_time(1.0, None, 3.0), 3.0);
        assert_eq!(frame_time(1.0, Some(2.0), 3.0), 2.0);
        assert_eq!(frame_time(2.5, Some(2.0), 3.0), 2.5);
        assert_eq!(frame_time(1.0, Some(4.0), 3.0), 3.0);
    }

    #[test]
    fn a_finger_drag_scrolls_the_list_and_keeps_gliding() {
        let mut harness = Harness::new();
        harness.touch(Phase::Start, [200.0, 420.0]);
        for step in 1..=10 {
            harness.touch(Phase::Move, [200.0, 420.0 - step as f32 * 15.0]);
        }
        let dragged = harness.offset;
        assert!(dragged > 100.0, "dragging by 150 px scrolled only {dragged}");

        harness.touch(Phase::End, [200.0, 270.0]);
        for _ in 0..20 {
            harness.frame(&[]);
        }
        assert!(harness.offset > dragged + 10.0, "no momentum after release: {} -> {}", dragged, harness.offset);
    }
}
