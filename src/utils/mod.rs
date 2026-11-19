//! What the rest of the crate shares: the error type, the small helpers that more than one
//! module needs, the few calls into the system's C library, and the logger.

#[cfg(target_os = "android")]
pub mod logger;

use std::path::Path;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

#[cfg(target_os = "android")]
pub use ffi::{Library, Watch, ioctl_get, ioctl_set};

#[derive(Debug)]
pub struct Error(String);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

macro_rules! err {
    ($($arg:tt)*) => {
        $crate::Error::new(format!($($arg)*))
    };
}

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::err!($($arg)*))
    };
}

pub(crate) use {bail, err};

/// Locks a mutex whether or not a thread has panicked while it held it: what the mutex guards
/// is still what everyone else works with.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts a thread under a name of its own, which is how a debugger and `ps -T` show it.
pub fn spawn<T: Send + 'static>(name: impl Into<String>, work: impl FnOnce() -> T + Send + 'static) -> std::io::Result<JoinHandle<T>> {
    std::thread::Builder::new().name(name.into()).spawn(work)
}

/// Reads a number, or anything else written as text, with the blanks around it left out.
pub fn parse<T: FromStr>(text: &str) -> Option<T> {
    text.trim().parse().ok()
}

/// The text in a buffer that C code has filled: what comes before the first NUL, with anything
/// that is not UTF-8 replaced.
pub fn text(bytes: &[u8]) -> String {
    let length = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..length]).into_owned()
}

/// Text as hexadecimal UTF-8, which keeps line breaks and blanks out of a line protocol.
pub fn hex(text: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    text.bytes().flat_map(|byte| [DIGITS[usize::from(byte >> 4)], DIGITS[usize::from(byte & 15)]]).map(char::from).collect()
}

/// The text that [`hex`] wrote.
pub fn unhex(hex: &str) -> Option<String> {
    let digit = |byte: u8| char::from(byte).to_digit(16);
    let byte = |&[high, low]: &[u8; 2]| u8::try_from(digit(high)? << 4 | digit(low)?).ok();
    String::from_utf8(hex.as_bytes().as_chunks().0.iter().map(byte).collect::<Option<Vec<u8>>>()?).ok()
}

/// Whether a bit is set in a bitmap the kernel filled: the lowest bit of the first byte is bit zero.
pub fn bit(bits: &[u8], index: usize) -> bool {
    bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

/// Where the hashes that tell a changed picture from an unchanged one start from.
pub const HASH_SEED: u64 = 0x9e37_79b9_7f4a_7c15;

/// Takes one more value into such a hash. It is quick, and good enough to notice a change.
pub fn mix(hash: u64, value: u64) -> u64 {
    (hash.rotate_left(5) ^ value).wrapping_mul(HASH_SEED)
}

/// Takes bytes into such a hash, eight at a time.
pub fn mix_bytes(hash: u64, bytes: &[u8]) -> u64 {
    let (words, rest) = bytes.as_chunks::<8>();
    let hash = words.iter().fold(hash, |hash, word| mix(hash, u64::from_le_bytes(*word)));
    rest.iter().fold(hash, |hash, &byte| mix(hash, byte.into()))
}

/// Reads a whole file; the error names it.
pub fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| err!("cannot read {}: {error}", path.display()))
}

/// A system property read as a number.
#[cfg(target_os = "android")]
fn property_as<T: FromStr>(name: &std::ffi::CStr) -> Option<T> {
    parse(&ffi::property(name)?)
}

#[cfg(target_os = "android")]
pub fn api_level() -> u32 {
    property_as(c"ro.build.version.sdk").unwrap_or(0)
}

/// How many pixels of the display make one of egui's points.
#[cfg(target_os = "android")]
pub fn density() -> f32 {
    let dpi: f32 = property_as(c"ro.sf.lcd_density").or_else(|| property_as(c"qemu.sf.lcd_density")).unwrap_or(160.0);
    (dpi / 160.0).clamp(0.5, 8.0)
}

/// The calls into the system's C library that the standard library does not make for us.
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod ffi {
    use std::ffi::{CStr, CString, c_void};
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::ptr::NonNull;

    use crate::Result;

    /// A library of the system, opened by name to look its functions up in.
    pub struct Library {
        handle: NonNull<c_void>,
    }

    unsafe impl Send for Library {}
    unsafe impl Sync for Library {}

    impl Library {
        pub fn open(name: &CStr) -> Result<Self> {
            let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW) };
            match NonNull::new(handle) {
                Some(handle) => Ok(Self { handle }),
                None => bail!("dlopen {name:?} failed: {}", last_error()),
            }
        }

        pub fn find(&self, symbol: &CStr) -> Option<NonNull<c_void>> {
            NonNull::new(unsafe { libc::dlsym(self.handle.as_ptr(), symbol.as_ptr()) })
        }

        /// # Safety
        /// `F` must be the function pointer type matching the symbol's real ABI.
        pub unsafe fn func<F: Copy>(&self, symbol: &CStr) -> Option<F> {
            assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
            self.find(symbol).map(|address| unsafe { std::mem::transmute_copy(&address.as_ptr()) })
        }
    }

    fn last_error() -> String {
        let message = unsafe { libc::dlerror() };
        if message.is_null() {
            return "unknown error".into();
        }
        unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned()
    }

    const PROPERTY_VALUE_MAX: usize = 92;

    /// A system property, when it is set to anything.
    pub fn property(name: &CStr) -> Option<String> {
        let mut buffer = [0u8; PROPERTY_VALUE_MAX];
        let length = unsafe { libc::__system_property_get(name.as_ptr(), buffer.as_mut_ptr().cast()) };
        let length = usize::try_from(length).ok().filter(|&length| length > 0)?;
        Some(super::text(&buffer[..length.min(PROPERTY_VALUE_MAX)]))
    }

    /// Tells when entries appear in directories or leave them, without looking at them in between.
    pub struct Watch {
        events: File,
    }

    impl Watch {
        /// Watches those of the directories that can be watched; `None` when none of them can.
        pub fn new(directories: &[&str]) -> Option<Self> {
            let descriptor = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
            if descriptor < 0 {
                return None;
            }
            let events = unsafe { File::from_raw_fd(descriptor) };
            let changes = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_TO;
            let watch = |directory: &str| CString::new(directory).is_ok_and(|path| unsafe { libc::inotify_add_watch(events.as_raw_fd(), path.as_ptr(), changes) } >= 0);
            // Every directory is tried: one that cannot be watched does not stop the others.
            let watched = directories.iter().filter(|directory| watch(directory)).count();
            (watched > 0).then_some(Self { events })
        }

        /// Sleeps until something appeared or left. `false` when the watch itself is gone.
        pub fn wait(&mut self) -> bool {
            let mut events = [0u8; 4096];
            self.events.read(&mut events).is_ok_and(|read| read > 0)
        }
    }

    /// The number of a device request: which way its argument travels, how large that is, the
    /// driver's letter and the request's own number.
    const fn request(direction: u32, size: usize, group: u8, number: u32) -> u32 {
        direction << 30 | (size as u32) << 16 | (group as u32) << 8 | number
    }

    const NOTHING: u32 = 0;
    const WRITES: u32 = 1;
    const READS: u32 = 2;

    /// Asks a device driver for what it holds under a request and has it written into `target`.
    /// The request is made for the size of `target`, so the driver writes no more than fits.
    /// Returns whether the driver answered.
    pub fn ioctl_get<T: ?Sized>(device: &File, group: u8, number: u32, target: &mut T) -> bool {
        let request = request(READS, size_of_val(target), group, number);
        unsafe { libc::ioctl(device.as_raw_fd(), request as _, std::ptr::from_mut(target).cast::<c_void>()) >= 0 }
    }

    /// Passes a device driver a number, or with `None` makes a request that takes nothing.
    pub fn ioctl_set(device: &File, group: u8, number: u32, value: Option<u32>) -> io::Result<()> {
        let request = if value.is_some() { request(WRITES, size_of::<u32>(), group, number) } else { request(NOTHING, 0, group, number) };
        let status = unsafe { libc::ioctl(device.as_raw_fd(), request as _, value.unwrap_or(0) as libc::c_ulong) };
        if status < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_travels_as_hexadecimal_and_comes_back() {
        assert_eq!(hex("Привет\n ok"), "d09fd180d0b8d0b2d0b5d1820a206f6b");
        assert_eq!(unhex(&hex("Привет\n ok")).as_deref(), Some("Привет\n ok"));
        assert_eq!((unhex(""), unhex("zz"), unhex("ff")), (Some(String::new()), None, None));
    }

    #[test]
    fn numbers_bits_and_buffers_are_read_the_way_the_system_writes_them() {
        assert_eq!((parse::<u32>(" 34\n"), parse::<f32>("2.625"), parse::<u32>("x")), (Some(34), Some(2.625), None));
        assert_eq!((text(b"qwerty2\0\0left over"), text(b"no end")), ("qwerty2".to_owned(), "no end".to_owned()));
        let bits = [0b0000_0001, 0b1000_0000];
        assert!(bit(&bits, 0) && bit(&bits, 15) && !bit(&bits, 1) && !bit(&bits, 16));
    }

    #[test]
    fn bytes_are_hashed_eight_at_a_time_and_the_rest_one_by_one() {
        let bytes: Vec<u8> = (1..=19).collect();
        let values = [u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]), u64::from_le_bytes([9, 10, 11, 12, 13, 14, 15, 16]), 17, 18, 19];
        assert_eq!(mix_bytes(HASH_SEED, &bytes), values.into_iter().fold(HASH_SEED, mix));
        assert_ne!(mix_bytes(HASH_SEED, &bytes[..18]), mix_bytes(HASH_SEED, &bytes));
    }
}
