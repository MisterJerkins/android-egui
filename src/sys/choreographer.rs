use std::ffi::{c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::dl::Library;

static PERIOD_NANOS: AtomicU64 = AtomicU64::new(0);

type Prepare = unsafe extern "C" fn(c_int) -> *mut c_void;
type PollOnce = unsafe extern "C" fn(c_int, *mut c_int, *mut c_int, *mut *mut c_void) -> c_int;
type GetInstance = unsafe extern "C" fn() -> *mut c_void;
type Callback = unsafe extern "C" fn(i64, *mut c_void);
type Register = unsafe extern "C" fn(*mut c_void, Callback, *mut c_void);

unsafe extern "C" fn on_refresh_rate(period_nanos: i64, _data: *mut c_void) {
    PERIOD_NANOS.store(period_nanos.max(0) as u64, Ordering::Relaxed);
}

/// The display's vsync period, tracked through the NDK choreographer (Android 11+).
pub fn vsync_period() -> Option<Duration> {
    static STARTED: OnceLock<bool> = OnceLock::new();
    if !*STARTED.get_or_init(start) {
        return None;
    }
    match PERIOD_NANOS.load(Ordering::Relaxed) {
        0 => None,
        nanos => Some(Duration::from_nanos(nanos)),
    }
}

fn start() -> bool {
    let Ok(android) = Library::open(c"libandroid.so") else {
        return false;
    };
    let functions = unsafe {
        (
            android.func::<Prepare>(c"ALooper_prepare"),
            android.func::<PollOnce>(c"ALooper_pollOnce"),
            android.func::<GetInstance>(c"AChoreographer_getInstance"),
            android.func::<Register>(c"AChoreographer_registerRefreshRateCallback"),
        )
    };
    let (Some(prepare), Some(poll_once), Some(instance), Some(register)) = functions else {
        return false;
    };
    std::thread::Builder::new()
        .name("vsync".into())
        .spawn(move || unsafe {
            prepare(0);
            let choreographer = instance();
            if choreographer.is_null() {
                return;
            }
            register(choreographer, on_refresh_rate, std::ptr::null_mut());
            loop {
                poll_once(-1, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            }
        })
        .is_ok()
}
