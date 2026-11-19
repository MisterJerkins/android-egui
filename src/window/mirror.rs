//! Copies of the overlay for the displays that screen recordings read from.
//!
//! From Android 14 a recording takes its picture from the window manager's own layers and so
//! misses a layer made beside them. Each such display has a layer stack of its own; a mirror of
//! the overlay is put on every one of them, scaled the way the system scales the screen there.

use crate::utils;

/// A display as `dumpsys display` lists it: its layer stack and the size of the space it shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shown {
    pub stack: u32,
    pub size: [f32; 2],
}

/// Reads the display devices out of `dumpsys display`. Displays that are off have no layer stack.
pub fn displays(dump: &str) -> Vec<Shown> {
    let mut shown = Vec::new();
    for device in dump.split("DisplayDeviceInfo{").skip(1) {
        let field = |name: &str| device.lines().find_map(|line| line.trim().strip_prefix(name));
        let stack = field("mCurrentLayerStack=").and_then(utils::parse::<u32>);
        let size = field("mCurrentLayerStackRect=Rect(").and_then(|rect| {
            let numbers: Vec<f32> = rect.split(|c: char| !c.is_ascii_digit() && c != '-').filter(|part| !part.is_empty() && *part != "-").filter_map(utils::parse).collect();
            let [left, top, right, bottom] = numbers[..] else { return None };
            Some([right - left, bottom - top]).filter(|size| size[0] > 0.0 && size[1] > 0.0)
        });
        shown.extend(stack.zip(size).map(|(stack, size)| Shown { stack, size }));
    }
    shown
}

/// How a `source`-sized picture sits on a `target`-sized display: its scale and its offset.
pub fn fit(source: [f32; 2], target: [f32; 2]) -> (f32, [f32; 2]) {
    let scale = (target[0] / source[0]).min(target[1] / source[1]);
    (scale, [(target[0] - source[0] * scale) / 2.0, (target[1] - source[1] * scale) / 2.0])
}

#[cfg(target_os = "android")]
pub use worker::Mirrors;

#[cfg(target_os = "android")]
mod worker {
    use std::collections::HashMap;
    use std::collections::hash_map::Entry;
    use std::process::Command;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use super::super::libgui::{Handle, LibGui};
    use super::{displays, fit};
    use crate::utils;

    const MAIN_STACK: u32 = 0;

    #[derive(Clone)]
    struct Source {
        layer: Handle,
        size: [u32; 2],
        version: u64,
    }

    #[derive(Default)]
    struct State {
        source: Option<Source>,
        stop: bool,
    }

    #[derive(Default)]
    struct Shared {
        state: Mutex<State>,
        wake: Condvar,
    }

    struct Copy {
        root: Handle,
        _copy: Handle,
        placed: Option<(f32, [f32; 2])>,
    }

    /// Keeps the overlay mirrored on every other display, from a thread of its own: asking the
    /// system which displays exist takes milliseconds, which a frame cannot spare. The system
    /// is asked once a second, and only while there is a layer to mirror.
    pub struct Mirrors {
        shared: Arc<Shared>,
        worker: Option<JoinHandle<()>>,
        version: u64,
    }

    impl Mirrors {
        /// Returns `None` where recordings see the overlay without help, or mirroring is missing.
        pub fn start(gui: &'static LibGui, client: Handle) -> Option<Self> {
            // Recordings miss the overlay from Android 14 (API 34) on.
            if gui.api() < 34 || !gui.can_mirror() {
                return None;
            }
            let shared = Arc::new(Shared::default());
            let seen = shared.clone();
            let worker = utils::spawn("mirror", move || work(gui, &client, &seen)).ok()?;
            Some(Self { shared, worker: Some(worker), version: 0 })
        }

        /// Sets the layer to mirror and its size, or `None` to mirror nothing.
        pub fn follow(&mut self, layer: Option<(Handle, [u32; 2])>) {
            self.version += 1;
            let source = layer.map(|(layer, size)| Source { layer, size, version: self.version });
            utils::lock(&self.shared.state).source = source;
            self.shared.wake.notify_one();
        }
    }

    impl Drop for Mirrors {
        fn drop(&mut self) {
            utils::lock(&self.shared.state).stop = true;
            self.shared.wake.notify_one();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn work(gui: &'static LibGui, client: &Handle, shared: &Shared) {
        let Ok(transaction) = gui.new_transaction() else { return };
        let mut copies: HashMap<u32, Copy> = HashMap::new();
        let mut version = 0;
        loop {
            let (source, stop) = {
                let state = utils::lock(&shared.state);
                (state.source.clone(), state.stop)
            };
            if stop {
                return;
            }
            let Some(source) = source else {
                // Nothing to mirror, so nothing to watch for either, until there is.
                copies.clear();
                drop(shared.wake.wait_while(utils::lock(&shared.state), |state| state.source.is_none() && !state.stop));
                continue;
            };
            if source.version != version {
                copies.clear();
                version = source.version;
            }
            let shown = Command::new("dumpsys").arg("display").output().map(|output| displays(&String::from_utf8_lossy(&output.stdout))).unwrap_or_default();
            copies.retain(|stack, _| shown.iter().any(|display| display.stack == *stack));

            let main = shown.iter().find(|display| display.stack == MAIN_STACK).map(|display| display.size);
            for display in shown.iter().filter(|display| display.stack != MAIN_STACK) {
                let Some(main) = main else { break };
                if let Entry::Vacant(free) = copies.entry(display.stack) {
                    match gui.mirror(client.as_ptr(), transaction, source.layer.as_ptr(), source.size, display.stack) {
                        Ok([copy, root]) => {
                            log::info!("mirroring the overlay to display layer stack {}", display.stack);
                            free.insert(Copy { root, _copy: copy, placed: None });
                        }
                        Err(error) => log::warn!("cannot mirror to layer stack {}: {error}", display.stack),
                    }
                }
                let placement = fit(main, display.size);
                if let Some(copy) = copies.get_mut(&display.stack).filter(|copy| copy.placed != Some(placement)) {
                    match gui.place(transaction, copy.root.as_ptr(), placement.0, placement.1) {
                        Ok(()) => copy.placed = Some(placement),
                        Err(error) => log::warn!("cannot place the mirror on layer stack {}: {error}", display.stack),
                    }
                }
            }

            // The next look is a second away, or as soon as the layer to mirror changes.
            let same = |state: &mut State| !state.stop && state.source.as_ref().is_some_and(|source| source.version == version);
            drop(shared.wake.wait_timeout_while(utils::lock(&shared.state), Duration::from_secs(1), same));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_are_read_from_the_dump() {
        let dump = r#"
Display Devices: size=3
  DisplayDeviceInfo{"Built-in Screen": uniqueId="local:4630946218744042899", 1504 x 2400, modeId 1, supportedModes [{id=1, width=1504, height=2400, fps=60.0}], state ON}
    mAdapter=LocalDisplayAdapter
    mUniqueId=local:4630946218744042899
    mCurrentLayerStack=0
    mCurrentFlags=1
    mCurrentOrientation=1
    mCurrentLayerStackRect=Rect(0, 0 - 2400, 1504)
    mCurrentDisplayRect=Rect(0, 0 - 2400, 1504)
  DisplayDeviceInfo{"ScreenRecorder": uniqueId="virtual:com.android.systemui,10182,ScreenRecorder,0", 1280 x 720, modeId 2, state ON}
    mUniqueId=virtual:com.android.systemui,10182,ScreenRecorder,0
    mCurrentLayerStack=7
    mCurrentOrientation=0
    mCurrentLayerStackRect=Rect(0, 0 - 1280, 720)
    mCurrentDisplayRect=Rect(0, 0 - 1280, 720)
  DisplayDeviceInfo{"Sleeping": uniqueId="local:2", 800 x 600, state OFF}
    mUniqueId=local:2
    mCurrentLayerStack=-1
    mCurrentLayerStackRect=Rect(0, 0 - 800, 600)
Logical Displays: size=1
"#;
        assert_eq!(displays(dump), [Shown { stack: 0, size: [2400.0, 1504.0] }, Shown { stack: 7, size: [1280.0, 720.0] }]);
        assert!(displays("nothing of the kind").is_empty());
    }

    #[test]
    fn a_picture_is_scaled_to_fit_and_centred() {
        assert_eq!(fit([2400.0, 1504.0], [1200.0, 752.0]), (0.5, [0.0, 0.0]));
        let (scale, offset) = fit([2400.0, 1504.0], [1280.0, 720.0]);
        assert!((scale - 720.0 / 1504.0).abs() < 1e-6 && (offset[0] - (1280.0 - 2400.0 * scale) / 2.0).abs() < 1e-3 && offset[1].abs() < 1e-3);
        let (scale, offset) = fit([1504.0, 2400.0], [1280.0, 720.0]);
        assert!((scale - 0.3).abs() < 1e-6 && offset[0] > 400.0 && offset[1].abs() < 1e-3);
    }
}
