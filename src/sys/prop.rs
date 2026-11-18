use std::ffi::CStr;
use std::str::FromStr;

const VALUE_MAX: usize = 92;

pub fn get(name: &CStr) -> Option<String> {
    let mut buffer = [0u8; VALUE_MAX];
    let length = unsafe { libc::__system_property_get(name.as_ptr(), buffer.as_mut_ptr().cast()) };
    let length = usize::try_from(length).ok().filter(|&length| length > 0)?;
    Some(String::from_utf8_lossy(&buffer[..length.min(VALUE_MAX)]).into_owned())
}

pub fn parse<T: FromStr>(name: &CStr) -> Option<T> {
    get(name)?.trim().parse().ok()
}

pub fn api_level() -> u32 {
    parse(c"ro.build.version.sdk").unwrap_or(0)
}

pub fn density() -> f32 {
    let dpi: f32 = parse(c"ro.sf.lcd_density").or_else(|| parse(c"qemu.sf.lcd_density")).unwrap_or(160.0);
    (dpi / 160.0).clamp(0.5, 8.0)
}
