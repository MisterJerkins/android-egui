use super::evdev::*;
use crate::window::Rotation;

pub const IDC_DIRECTORIES: [&str; 6] = ["/product/usr/idc", "/system_ext/usr/idc", "/odm/usr/idc", "/vendor/usr/idc", "/system/usr/idc", "/data/system/devices/idc"];

/// Names that rule a surface out when nothing better than a guess is available.
const NOT_A_SCREEN: [&str; 17] = [
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
const SMALLEST_SCREEN_SPAN: i32 = 200;

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
    if NOT_A_SCREEN.iter().any(|word| name.contains(word)) {
        return None;
    }
    let span = x.span().min(y.span());
    (span >= SMALLEST_SCREEN_SPAN).then_some(Confidence::Guess(span))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    const DIRECT: u32 = 1 << PROP_DIRECT;
    const SCREEN: &str = "touch.deviceType = touchScreen\ntouch.orientationAware = 1\n";
    const KEYBOARD: [u16; 4] = [1, KEY_ENTER, KEY_A, 57];
    const MOUSE: [u16; 3] = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE];
    const BTN_TOOL_FINGER: u16 = 0x145;

    pub fn device(name: &str, properties: u32, keys: &[u16], axes: &[(u16, i32, i32)], relative: &[u16]) -> Capabilities {
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
}
