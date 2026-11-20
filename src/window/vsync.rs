//! The display's own rhythm, asked of the NDK's choreographer (Android 7 and later). To a
//! program without a Java machine the choreographer says nothing by itself, so it is asked to
//! call at a few refreshes in a row, a few times a second for as long as frames go on.

use std::ffi::{c_int, c_long, c_void};
use std::ptr::{null_mut, without_provenance_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use crate::utils::{self, Library};

/// The moment the display took a frame, and the time between two of its frames.
#[derive(Clone, Copy, Debug)]
pub struct Vsync {
    pub at: Instant,
    pub period: Duration,
}

impl Vsync {
    /// For a display that is known only by how often it takes a frame, not when: any moment
    /// will do.
    pub fn anytime(period: Duration) -> Self {
        Self { at: base().0, period }
    }
}

type Prepare = unsafe extern "C" fn(c_int) -> *mut c_void;
type PollOnce = unsafe extern "C" fn(c_int, *mut c_int, *mut c_int, *mut *mut c_void) -> c_int;
type GetInstance = unsafe extern "C" fn() -> *mut c_void;
type Callback = unsafe extern "C" fn(i64, *mut c_void);
type Post = unsafe extern "C" fn(*mut c_void, Callback, *mut c_void);

static POST: OnceLock<Option<Post>> = OnceLock::new();
static CHOREOGRAPHER: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// Nanoseconds on the clock that does not stop or jump. No period, nothing known yet.
static AT: AtomicI64 = AtomicI64::new(0);
static PERIOD: AtomicI64 = AtomicI64::new(0);
static ASKED: AtomicI64 = AtomicI64::new(0);
static ASKING: AtomicBool = AtomicBool::new(false);
/// What a run of calls keeps from one to the next; only the choreographer's thread has these.
static BEFORE: AtomicI64 = AtomicI64::new(0);
static SHORTEST: AtomicI64 = AtomicI64::new(0);

/// Now, in nanoseconds on the clock that does not stop or jump: the one the system times its
/// frames by.
pub fn nanos() -> i64 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    i64::from(time.tv_sec) * 1_000_000_000 + i64::from(time.tv_nsec)
}

/// One moment as both clocks tell it, which turns the system's count into an [`Instant`].
fn base() -> (Instant, i64) {
    static BASE: OnceLock<(Instant, i64)> = OnceLock::new();
    *BASE.get_or_init(|| (Instant::now(), nanos()))
}

/// The latest that is known of the display's rhythm. Asking keeps it fresh.
pub fn get() -> Option<Vsync> {
    let post = (*POST.get_or_init(listen))?;
    let choreographer = CHOREOGRAPHER.load(Ordering::Acquire);
    if choreographer.is_null() {
        return None;
    }
    let (base, base_nanos) = base();
    let now = nanos();
    if now - ASKED.load(Ordering::Relaxed) > 250_000_000 && !ASKING.swap(true, Ordering::AcqRel) {
        ASKED.store(now, Ordering::Relaxed);
        unsafe { post(choreographer, on_refresh, without_provenance_mut(2)) };
    }
    let period = PERIOD.load(Ordering::Relaxed);
    if period == 0 {
        return None;
    }
    let since = AT.load(Ordering::Relaxed) - base_nanos;
    let passed = Duration::from_nanos(since.unsigned_abs());
    let at = if since >= 0 { base.checked_add(passed)? } else { base.checked_sub(passed)? };
    Some(Vsync { at, period: Duration::from_nanos(period.unsigned_abs()) })
}

/// Starts the thread that listens to the choreographer. It lives for as long as the process
/// does: a looper cannot be asked to stop from outside.
fn listen() -> Option<Post> {
    let android = Library::open(c"libandroid.so").ok()?;
    let (prepare, poll_once, instance, post) = unsafe {
        (
            android.func::<Prepare>(c"ALooper_prepare")?,
            android.func::<PollOnce>(c"ALooper_pollOnce")?,
            android.func::<GetInstance>(c"AChoreographer_getInstance")?,
            // The older call counts the time in a `long`, which is as wide only on a 64-bit system.
            android
                .func::<Post>(c"AChoreographer_postFrameCallback64")
                .or_else(|| android.func::<Post>(c"AChoreographer_postFrameCallback").filter(|_| size_of::<c_long>() == size_of::<i64>()))?,
        )
    };
    let wait = move || unsafe {
        prepare(0);
        CHOREOGRAPHER.store(instance(), Ordering::Release);
        loop {
            poll_once(-1, null_mut(), null_mut(), null_mut());
        }
    };
    utils::spawn("vsync", wait).ok()?;
    Some(post)
}

/// Called by the choreographer on its thread at a refresh, with how many more refreshes to be
/// called at. The shortest time between two of the run is the period.
unsafe extern "C" fn on_refresh(nanos: i64, left: *mut c_void) {
    let more = left.addr();
    let shortest = if more == 2 { i64::MAX } else { SHORTEST.load(Ordering::Relaxed).min(nanos - BEFORE.load(Ordering::Relaxed)) };
    SHORTEST.store(shortest, Ordering::Relaxed);
    BEFORE.store(nanos, Ordering::Relaxed);
    if more > 0 {
        if let Some(Some(post)) = POST.get() {
            unsafe { post(CHOREOGRAPHER.load(Ordering::Acquire), on_refresh, without_provenance_mut(more - 1)) };
        }
        return;
    }
    // No display takes its frames less than two milliseconds or more than a tenth of a second apart.
    if (2_000_000..=100_000_000).contains(&shortest) {
        PERIOD.store(shortest, Ordering::Relaxed);
        AT.store(nanos, Ordering::Relaxed);
    }
    ASKING.store(false, Ordering::Release);
}
