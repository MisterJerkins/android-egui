use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use super::parser::Phase;
use crate::Result;
use crate::input::evdev::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, BTN_TOUCH, EV_ABS, EV_KEY, EV_SYN, RawEvent, SYN_REPORT};

/// The name the virtual touchscreen goes by in `/dev/input`.
pub const VIRTUAL_TOUCHSCREEN: &str = "android-egui touch";
/// Android follows at most this many pointers at once.
pub const SLOTS: usize = 16;
/// Units along each axis of the virtual touchscreen.
pub const RANGE: i32 = 32768;

const GESTURE_FINGERS: u32 = 1 << 16;
const TAP: Duration = Duration::from_millis(60);
const STEP: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Panel { device: u32, contact: u32 },
    Code(u32),
}

/// The fingers on the virtual touchscreen, and the events that put them there.
#[derive(Default)]
pub struct Fingers {
    owners: [Option<Owner>; SLOTS],
    tracking: i32,
}

impl Fingers {
    pub fn is_empty(&self) -> bool {
        self.owners.iter().all(Option::is_none)
    }

    pub fn holds_panel(&self, wanted: u32) -> bool {
        self.owners.iter().flatten().any(|owner| matches!(owner, Owner::Panel { device, .. } if *device == wanted))
    }

    /// Writes the events for one change of a finger; `at` is a fraction of the display in its
    /// natural orientation. Returns `false` when there is nothing to change.
    pub fn apply(&mut self, owner: Owner, phase: Phase, at: (f32, f32), out: &mut Vec<RawEvent>) -> bool {
        let held = self.owners.iter().position(|slot| *slot == Some(owner));
        let lifting = matches!(phase, Phase::End | Phase::Cancel);
        let Some(slot) = held.or_else(|| self.owners.iter().position(Option::is_none).filter(|_| phase == Phase::Start)) else { return false };
        let absolute = |code, value| RawEvent { kind: EV_ABS, code, value };
        let button = |value| RawEvent { kind: EV_KEY, code: BTN_TOUCH, value };

        out.push(absolute(ABS_MT_SLOT, slot as i32));
        if lifting {
            self.owners[slot] = None;
            out.push(absolute(ABS_MT_TRACKING_ID, -1));
            out.extend(self.is_empty().then(|| button(0)));
        } else {
            if held.is_none() {
                out.extend(self.is_empty().then(|| button(1)));
                self.owners[slot] = Some(owner);
                self.tracking = (self.tracking + 1) & 0xffff;
                out.push(absolute(ABS_MT_TRACKING_ID, self.tracking));
            }
            let unit = |fraction: f32| ((fraction * RANGE as f32) as i32).clamp(0, RANGE - 1);
            out.push(absolute(ABS_MT_POSITION_X, unit(at.0)));
            out.push(absolute(ABS_MT_POSITION_Y, unit(at.1)));
        }
        out.push(RawEvent { kind: EV_SYN, code: SYN_REPORT, value: 0 });
        true
    }

    /// Lifts every finger `which` picks.
    pub fn lift(&mut self, which: impl Fn(Owner) -> bool, out: &mut Vec<RawEvent>) {
        for owner in self.owners.into_iter().flatten().filter(|&owner| which(owner)) {
            self.apply(owner, Phase::End, (0.0, 0.0), out);
        }
    }
}

pub(crate) trait Injector: Send + Sync {
    fn set_control(&self, enabled: bool) -> Result<()>;
    fn is_control(&self) -> bool;
    fn finger(&self, finger: u32, phase: Phase, position: [f32; 2]) -> Result<()>;
}

/// Touch control from code. Cheap to clone, and free to use from another thread.
///
/// Touches are only read until [`Touch::set_control`] turns control on. From then on a virtual
/// touchscreen stands in for the real one: fingers on the interface stay with the interface,
/// everything else is passed on, and [`Touch::down`], [`Touch::move_to`] and [`Touch::up`] add
/// fingers of their own. Positions are display pixels in the current orientation.
#[derive(Clone)]
pub struct Touch {
    injector: Arc<dyn Injector>,
}

impl Touch {
    pub(crate) fn new(injector: Arc<dyn Injector>) -> Self {
        Self { injector }
    }

    pub fn set_control(&self, enabled: bool) -> Result<()> {
        self.injector.set_control(enabled)
    }

    pub fn is_control(&self) -> bool {
        self.injector.is_control()
    }

    pub fn down(&self, finger: u32, position: [f32; 2]) -> Result<()> {
        self.injector.finger(finger, Phase::Start, position)
    }

    pub fn move_to(&self, finger: u32, position: [f32; 2]) -> Result<()> {
        self.injector.finger(finger, Phase::Move, position)
    }

    pub fn up(&self, finger: u32) -> Result<()> {
        self.injector.finger(finger, Phase::End, [0.0, 0.0])
    }

    /// Taps once. Blocks for the length of the tap.
    pub fn tap(&self, position: [f32; 2]) -> Result<()> {
        self.swipe(position, position, TAP)
    }

    /// Drags one finger in a straight line. Blocks until it is lifted.
    pub fn swipe(&self, from: [f32; 2], to: [f32; 2], duration: Duration) -> Result<()> {
        static NEXT: AtomicU32 = AtomicU32::new(GESTURE_FINGERS);
        let finger = NEXT.fetch_add(1, Ordering::Relaxed) | GESTURE_FINGERS;
        let started = Instant::now();
        self.down(finger, from)?;
        let dragged = loop {
            std::thread::sleep(STEP.min(duration.saturating_sub(started.elapsed())));
            let progress = if duration.is_zero() { 1.0 } else { (started.elapsed().as_secs_f32() / duration.as_secs_f32()).min(1.0) };
            let moved = self.move_to(finger, [from[0] + (to[0] - from[0]) * progress, from[1] + (to[1] - from[1]) * progress]);
            if moved.is_err() || progress >= 1.0 {
                break moved;
            }
        };
        self.up(finger).and(dragged)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const PANEL: Owner = Owner::Panel { device: 1, contact: 7 };

    fn codes(events: &[RawEvent]) -> Vec<(u16, i32)> {
        events.iter().map(|event| (event.code, event.value)).collect()
    }

    #[test]
    fn a_finger_goes_down_moves_and_lifts() {
        let (mut fingers, mut events) = (Fingers::default(), Vec::new());
        assert!(fingers.apply(PANEL, Phase::Start, (0.5, 0.25), &mut events));
        assert_eq!(codes(&events), [(ABS_MT_SLOT, 0), (BTN_TOUCH, 1), (ABS_MT_TRACKING_ID, 1), (ABS_MT_POSITION_X, 16384), (ABS_MT_POSITION_Y, 8192), (SYN_REPORT, 0)]);

        events.clear();
        assert!(fingers.apply(PANEL, Phase::Move, (1.0, -0.1), &mut events));
        assert_eq!(codes(&events), [(ABS_MT_SLOT, 0), (ABS_MT_POSITION_X, RANGE - 1), (ABS_MT_POSITION_Y, 0), (SYN_REPORT, 0)]);

        events.clear();
        assert!(fingers.apply(PANEL, Phase::End, (0.0, 0.0), &mut events));
        assert_eq!(codes(&events), [(ABS_MT_SLOT, 0), (ABS_MT_TRACKING_ID, -1), (BTN_TOUCH, 0), (SYN_REPORT, 0)]);
        assert!(fingers.is_empty() && !fingers.apply(PANEL, Phase::Move, (0.0, 0.0), &mut events));
    }

    #[test]
    fn fingers_share_the_screen_without_colliding() {
        let (mut fingers, mut events) = (Fingers::default(), Vec::new());
        fingers.apply(PANEL, Phase::Start, (0.1, 0.1), &mut events);
        events.clear();
        fingers.apply(Owner::Code(0), Phase::Start, (0.2, 0.2), &mut events);
        assert_eq!(codes(&events)[..2], [(ABS_MT_SLOT, 1), (ABS_MT_TRACKING_ID, 2)]);
        assert!(fingers.holds_panel(1) && !fingers.holds_panel(2));

        events.clear();
        fingers.apply(PANEL, Phase::Cancel, (0.0, 0.0), &mut events);
        assert_eq!(codes(&events), [(ABS_MT_SLOT, 0), (ABS_MT_TRACKING_ID, -1), (SYN_REPORT, 0)]);
        fingers.apply(Owner::Code(5), Phase::Start, (0.3, 0.3), &mut events);
        events.clear();
        fingers.lift(|owner| matches!(owner, Owner::Code(_)), &mut events);
        assert!(fingers.is_empty() && codes(&events).contains(&(BTN_TOUCH, 0)));
    }

    #[test]
    fn the_seventeenth_finger_is_refused() {
        let (mut fingers, mut events) = (Fingers::default(), Vec::new());
        for finger in 0..SLOTS as u32 {
            assert!(fingers.apply(Owner::Code(finger), Phase::Start, (0.5, 0.5), &mut events));
        }
        assert!(!fingers.apply(Owner::Code(99), Phase::Start, (0.5, 0.5), &mut events));
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(u32, Phase, [f32; 2])>>);

    impl Injector for Recorder {
        fn set_control(&self, _enabled: bool) -> Result<()> {
            Ok(())
        }

        fn is_control(&self) -> bool {
            true
        }

        fn finger(&self, finger: u32, phase: Phase, position: [f32; 2]) -> Result<()> {
            self.0.lock().unwrap().push((finger, phase, position));
            Ok(())
        }
    }

    #[test]
    fn a_swipe_runs_from_end_to_end_with_one_finger() {
        let recorder = Arc::new(Recorder::default());
        Touch::new(recorder.clone()).swipe([100.0, 200.0], [300.0, 600.0], Duration::from_millis(40)).unwrap();
        let steps = recorder.0.lock().unwrap();
        let (first, last) = (steps[0], steps[steps.len() - 1]);
        assert_eq!((first.1, first.2, last.1), (Phase::Start, [100.0, 200.0], Phase::End));
        assert_eq!((steps[steps.len() - 2].1, steps[steps.len() - 2].2), (Phase::Move, [300.0, 600.0]));
        assert!(steps.len() >= 4 && steps.iter().all(|step| step.0 == first.0));
        assert!(steps[1..steps.len() - 1].windows(2).all(|pair| pair[0].2[0] <= pair[1].2[0]));
    }
}
