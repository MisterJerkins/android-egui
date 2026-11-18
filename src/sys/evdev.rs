use std::ffi::c_void;
use std::fs::File;
use std::os::fd::AsRawFd;

use crate::input::evdev::{ABS_COUNT, ABS_MT_FIRST, ABS_MT_LAST, ABS_MT_SLOT, Axis, AxisState, Capabilities, EV_ABS, EV_KEY, EV_REL, KEY_COUNT};

pub const TIME_SIZE: usize = size_of::<libc::timeval>();

const fn read_request(number: u32, size: usize) -> u32 {
    2 << 30 | (size as u32) << 16 | (b'E' as u32) << 8 | number
}

fn query<T: ?Sized>(device: &File, number: u32, target: &mut T) -> bool {
    let request = read_request(number, size_of_val(target));
    unsafe { libc::ioctl(device.as_raw_fd(), request as _, std::ptr::from_mut(target).cast::<c_void>()) >= 0 }
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

    let length = name.iter().position(|&byte| byte == 0).unwrap_or(name.len());
    found.name = String::from_utf8_lossy(&name[..length]).into_owned();
    [found.bus, found.vendor, found.product, found.version] = identity;
    found.properties = u32::from_ne_bytes(properties);
    found.relative = u32::from_ne_bytes(relative);
    for code in (0..ABS_COUNT).filter(|code| axes[code / 8] & (1 << (code % 8)) != 0) {
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
    query(device, 0x18, &mut keys) && keys[usize::from(code) / 8] & (1 << (code % 8)) != 0
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
