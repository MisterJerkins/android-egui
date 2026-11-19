use std::ffi::CString;

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Logger;

static LOGGER: Logger = Logger;

/// Sends the crate's messages to stderr and to logcat. `ANDROID_EGUI_LOG` picks the level.
pub fn init() {
    let level = match std::env::var("ANDROID_EGUI_LOG").as_deref() {
        Ok("trace") => LevelFilter::Trace,
        Ok("debug") => LevelFilter::Debug,
        Ok("warn") => LevelFilter::Warn,
        Ok("error") => LevelFilter::Error,
        Ok("off") => LevelFilter::Off,
        _ => LevelFilter::Info,
    };
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let (tag, priority) = match record.level() {
            Level::Error => ('E', 6),
            Level::Warn => ('W', 5),
            Level::Info => ('I', 4),
            Level::Debug => ('D', 3),
            Level::Trace => ('T', 2),
        };
        let text = record.args().to_string();
        eprintln!("[{tag}] {text}");
        if let Ok(text) = CString::new(text) {
            logcat::write(priority, &text);
        }
    }

    fn flush(&self) {}
}

/// The system's own log, read with `logcat`.
#[allow(unsafe_code)]
mod logcat {
    use std::ffi::{CStr, c_char, c_int};

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(priority: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub fn write(priority: c_int, text: &CStr) {
        unsafe { __android_log_write(priority, c"android-egui".as_ptr(), text.as_ptr()) };
    }
}
