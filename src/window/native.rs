use std::ffi::c_void;
use std::ptr::NonNull;

use super::Region;
use crate::{Result, bail};

const WINDOW_MAGIC: u32 = u32::from_be_bytes(*b"_wnd");
const FORMAT_RGBA_8888: i32 = 1;

#[repr(C)]
struct RawBuffer {
    width: i32,
    height: i32,
    stride: i32,
    format: i32,
    bits: *mut c_void,
    reserved: [u32; 6],
}

#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_acquire(window: *mut c_void);
    fn ANativeWindow_release(window: *mut c_void);
    fn ANativeWindow_getWidth(window: *mut c_void) -> i32;
    fn ANativeWindow_getHeight(window: *mut c_void) -> i32;
    fn ANativeWindow_setBuffersGeometry(window: *mut c_void, width: i32, height: i32, format: i32) -> i32;
    fn ANativeWindow_lock(window: *mut c_void, buffer: *mut RawBuffer, dirty: *mut Region) -> i32;
    fn ANativeWindow_unlockAndPost(window: *mut c_void) -> i32;
}

pub struct NativeWindow(NonNull<c_void>);

impl NativeWindow {
    /// # Safety
    /// `surface` must point to a live `android::Surface`.
    pub(crate) unsafe fn from_surface(surface: *mut c_void) -> Result<Self> {
        let window = unsafe { surface.byte_add(2 * size_of::<usize>()) };
        if unsafe { window.cast::<u32>().read() } != WINDOW_MAGIC {
            bail!("android::Surface has an unexpected layout on this system");
        }
        unsafe { ANativeWindow_acquire(window) };
        Ok(Self(unsafe { NonNull::new_unchecked(window) }))
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }

    pub fn size(&self) -> (u32, u32) {
        let (width, height) = unsafe { (ANativeWindow_getWidth(self.as_ptr()), ANativeWindow_getHeight(self.as_ptr())) };
        (width.max(0) as u32, height.max(0) as u32)
    }

    pub fn set_size(&self, width: u32, height: u32) -> Result<()> {
        let status = unsafe { ANativeWindow_setBuffersGeometry(self.as_ptr(), width as i32, height as i32, FORMAT_RGBA_8888) };
        if status != 0 {
            bail!("ANativeWindow_setBuffersGeometry failed (status {status})");
        }
        Ok(())
    }

    /// Locks the next buffer for CPU drawing. `dirty` is the area the caller intends to redraw;
    /// the returned frame reports the area that actually has to be redrawn.
    pub fn lock(&self, dirty: Option<Region>) -> Result<Frame<'_>> {
        let mut raw = RawBuffer { width: 0, height: 0, stride: 0, format: 0, bits: std::ptr::null_mut(), reserved: [0; 6] };
        let mut region = dirty.unwrap_or_default();
        let region_ptr = if dirty.is_some() { &raw mut region } else { std::ptr::null_mut() };
        let status = unsafe { ANativeWindow_lock(self.as_ptr(), &mut raw, region_ptr) };
        if status != 0 || raw.bits.is_null() {
            bail!("ANativeWindow_lock failed (status {status})");
        }
        if dirty.is_none() {
            region = Region { left: 0, top: 0, right: raw.width, bottom: raw.height };
        }
        Ok(Frame { window: self, pixels: raw.bits.cast(), width: raw.width.max(0) as usize, height: raw.height.max(0) as usize, stride: raw.stride.max(0) as usize, dirty: region })
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        unsafe { ANativeWindow_release(self.as_ptr()) };
    }
}

pub struct Frame<'a> {
    window: &'a NativeWindow,
    pixels: *mut [u8; 4],
    width: usize,
    height: usize,
    stride: usize,
    dirty: Region,
}

impl Frame<'_> {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn dirty(&self) -> Region {
        self.dirty
    }

    /// Every row of premultiplied RGBA pixels, and how many pixels apart their starts are.
    pub fn pixels(&mut self) -> (&mut [[u8; 4]], usize) {
        assert!(self.width <= self.stride);
        let length = if self.height == 0 { 0 } else { (self.height - 1) * self.stride + self.width };
        (unsafe { std::slice::from_raw_parts_mut(self.pixels, length) }, self.stride)
    }

    /// One row of premultiplied RGBA pixels.
    pub fn row(&mut self, y: usize) -> &mut [[u8; 4]] {
        assert!(y < self.height && self.width <= self.stride);
        unsafe { std::slice::from_raw_parts_mut(self.pixels.add(y * self.stride), self.width) }
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        unsafe { ANativeWindow_unlockAndPost(self.window.as_ptr()) };
    }
}
