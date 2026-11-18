use std::ffi::{CStr, c_void};
use std::ptr::NonNull;

use crate::{Result, bail};

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
