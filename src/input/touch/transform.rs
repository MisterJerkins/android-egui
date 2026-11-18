use crate::input::classify::Panel;
use crate::window::Rotation;

/// Turns a position given as fractions of an unrotated surface into fractions of the same
/// surface after `rotation`, matching Android's input reader.
pub fn rotate(u: f32, v: f32, rotation: Rotation) -> (f32, f32) {
    match rotation {
        Rotation::Deg0 => (u, v),
        Rotation::Deg90 => (v, 1.0 - u),
        Rotation::Deg180 => (1.0 - u, 1.0 - v),
        Rotation::Deg270 => (1.0 - v, u),
    }
}

/// Maps a touch panel position to pixels of the display in its current orientation.
pub fn touch_to_screen(u: f32, v: f32, panel: &Panel, size: [f32; 2], rotation: Rotation) -> [f32; 2] {
    let (u, v) = rotate(u, v, panel.orientation);
    let (u, v) = if panel.follows_display { rotate(u, v, rotation) } else { (u, v) };
    [u * size[0], v * size[1]]
}

/// The reverse of [`touch_to_screen`] for a panel that lies the way the display naturally does.
pub fn screen_to_touch(position: [f32; 2], size: [f32; 2], rotation: Rotation) -> (f32, f32) {
    let back = match rotation {
        Rotation::Deg90 => Rotation::Deg270,
        Rotation::Deg270 => Rotation::Deg90,
        straight => straight,
    };
    rotate(position[0] / size[0], position[1] / size[1], back)
}

/// Absolute pointers (emulator mice) already report in the display's current orientation.
pub fn pointer_to_screen(u: f32, v: f32, size: [f32; 2]) -> [f32; 2] {
    [u * size[0], v * size[1]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::classify::{Confidence, Protocol};
    use crate::input::evdev::Axis;

    fn panel(orientation: Rotation, follows_display: bool) -> Panel {
        Panel {
            protocol: Protocol::Slots,
            x: Axis { min: 0, max: 15039 },
            y: Axis { min: 0, max: 23999 },
            orientation,
            follows_display,
            confidence: Confidence::Certain,
            button: false,
            pressure: false,
        }
    }

    fn close(actual: [f32; 2], expected: [f32; 2]) {
        assert!((actual[0] - expected[0]).abs() < 0.01 && (actual[1] - expected[1]).abs() < 0.01, "{actual:?} != {expected:?}");
    }

    #[test]
    fn follows_every_display_rotation() {
        let touch = panel(Rotation::Deg0, true);
        let (u, v) = (0.25, 0.1);
        close(touch_to_screen(u, v, &touch, [1504.0, 2400.0], Rotation::Deg0), [376.0, 240.0]);
        close(touch_to_screen(u, v, &touch, [2400.0, 1504.0], Rotation::Deg90), [240.0, 1128.0]);
        close(touch_to_screen(u, v, &touch, [1504.0, 2400.0], Rotation::Deg180), [1128.0, 2160.0]);
        close(touch_to_screen(u, v, &touch, [2400.0, 1504.0], Rotation::Deg270), [2160.0, 376.0]);
    }

    #[test]
    fn screen_positions_map_back_to_the_panel() {
        let touch = panel(Rotation::Deg0, true);
        for (rotation, size) in
            [(Rotation::Deg0, [1504.0, 2400.0]), (Rotation::Deg90, [2400.0, 1504.0]), (Rotation::Deg180, [1504.0, 2400.0]), (Rotation::Deg270, [2400.0, 1504.0])]
        {
            let (u, v) = screen_to_touch(touch_to_screen(0.25, 0.1, &touch, size, rotation), size, rotation);
            assert!((u - 0.25).abs() < 1e-5 && (v - 0.1).abs() < 1e-5, "{rotation:?}: {u} {v}");
        }
    }

    #[test]
    fn four_quarter_turns_are_the_identity() {
        let (mut u, mut v) = (0.3, 0.8);
        for _ in 0..4 {
            (u, v) = rotate(u, v, Rotation::Deg90);
        }
        assert!((u - 0.3).abs() < 1e-6 && (v - 0.8).abs() < 1e-6);
        let (a, b) = rotate(0.3, 0.8, Rotation::Deg90);
        let (a, b) = rotate(a, b, Rotation::Deg270);
        assert!((a - 0.3).abs() < 1e-6 && (b - 0.8).abs() < 1e-6);
    }

    #[test]
    fn panel_orientation_and_fixed_panels() {
        let turned = panel(Rotation::Deg90, true);
        close(touch_to_screen(0.25, 0.1, &turned, [1000.0, 500.0], Rotation::Deg0), [100.0, 375.0]);

        let fixed = panel(Rotation::Deg0, false);
        close(touch_to_screen(0.25, 0.1, &fixed, [1000.0, 500.0], Rotation::Deg270), [250.0, 50.0]);
        close(pointer_to_screen(0.5, 0.5, [2560.0, 1440.0]), [1280.0, 720.0]);
    }
}
