use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;

use crate::input::evdev::{ABS_COUNT, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, BTN_TOUCH, EV_ABS, EV_KEY, EV_SYN, PROP_DIRECT};

const PATHS: [&str; 3] = ["/dev/uinput", "/dev/input/uinput", "/dev/misc/uinput"];
const NAME_SIZE: usize = 80;
const BUS_VIRTUAL: u16 = 0x06;

const fn request(writes: bool, group: u8, number: u32) -> u32 {
    if writes { 1 << 30 | 4 << 16 | (group as u32) << 8 | number } else { (group as u32) << 8 | number }
}

const DEVICE_CREATE: u32 = request(false, b'U', 1);
const SET_EVENT: u32 = request(true, b'U', 100);
const SET_KEY: u32 = request(true, b'U', 101);
const SET_ABSOLUTE: u32 = request(true, b'U', 103);
const SET_PROPERTY: u32 = request(true, b'U', 110);
const GRAB: u32 = request(true, b'E', 0x90);

fn control(device: &File, request: u32, value: u32) -> io::Result<()> {
    let status = unsafe { libc::ioctl(device.as_raw_fd(), request as _, value as libc::c_ulong) };
    if status < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Creates a multi-touch screen whose axes run from zero to `range - 1`. It lives as long as
/// the returned file stays open.
pub fn touchscreen(name: &str, range: i32, slots: usize) -> io::Result<File> {
    let mut failure = io::Error::from(io::ErrorKind::NotFound);
    let device = PATHS.iter().find_map(|path| OpenOptions::new().write(true).open(path).map_err(|error| failure = error).ok()).ok_or(failure)?;
    let axes = [(ABS_MT_SLOT, slots as i32 - 1), (ABS_MT_TRACKING_ID, 0xffff), (ABS_MT_POSITION_X, range - 1), (ABS_MT_POSITION_Y, range - 1)];

    for kind in [EV_SYN, EV_KEY, EV_ABS] {
        control(&device, SET_EVENT, kind.into())?;
    }
    control(&device, SET_KEY, BTN_TOUCH.into())?;
    control(&device, SET_PROPERTY, PROP_DIRECT)?;
    for (axis, _) in axes {
        control(&device, SET_ABSOLUTE, axis.into())?;
    }

    // struct uinput_user_dev: name, input_id, ff_effects_max, then absmax, absmin, absfuzz and absflat.
    let mut setup = vec![0u8; NAME_SIZE + 12 + 4 * 4 * ABS_COUNT];
    let length = name.len().min(NAME_SIZE - 1);
    setup[..length].copy_from_slice(&name.as_bytes()[..length]);
    setup[NAME_SIZE..NAME_SIZE + 2].copy_from_slice(&BUS_VIRTUAL.to_ne_bytes());
    for (axis, maximum) in axes {
        let offset = NAME_SIZE + 12 + 4 * usize::from(axis);
        setup[offset..offset + 4].copy_from_slice(&maximum.to_ne_bytes());
    }
    (&device).write_all(&setup)?;
    control(&device, DEVICE_CREATE, 0)?;
    Ok(device)
}

/// Takes a device away from every other reader, or gives it back.
pub fn grab(device: &File, grab: bool) -> io::Result<()> {
    control(device, GRAB, grab.into())
}
