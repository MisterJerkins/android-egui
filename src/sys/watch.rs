use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};

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
