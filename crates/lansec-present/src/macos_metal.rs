//! Metal present from CVPixelBuffer (GPU CIContext, no CPU readback).

use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::PresentError;

unsafe extern "C" {
    fn lansec_metal_open(nsview: *mut std::ffi::c_void, w: u32, h: u32) -> *mut std::ffi::c_void;
    fn lansec_metal_present(ctx: *mut std::ffi::c_void, pixel_buffer: *mut std::ffi::c_void) -> i32;
    fn lansec_metal_resize(ctx: *mut std::ffi::c_void, w: u32, h: u32);
    fn lansec_metal_close(ctx: *mut std::ffi::c_void);
}

pub struct MetalLayer {
    ptr: *mut std::ffi::c_void,
}

unsafe impl Send for MetalLayer {}

impl MetalLayer {
    pub fn from_winit(window: &winit::window::Window, width: u32, height: u32) -> Result<Self, PresentError> {
        let view = match window
            .window_handle()
            .map_err(|e| PresentError::Message(e.to_string()))?
            .as_raw()
        {
            RawWindowHandle::AppKit(h) => h.ns_view.as_ptr(),
            _ => return Err(PresentError::Message("not an AppKit window".into())),
        };
        let ptr = unsafe { lansec_metal_open(view, width, height) };
        if ptr.is_null() {
            return Err(PresentError::Message("Metal layer failed".into()));
        }
        Ok(Self { ptr })
    }

    pub fn present_pixel_buffer(&self, pb: *mut std::ffi::c_void) -> Result<(), PresentError> {
        if unsafe { lansec_metal_present(self.ptr, pb) } == 0 {
            Err(PresentError::Message("metal present failed".into()))
        } else {
            Ok(())
        }
    }

    pub fn resize(&self, width: u32, height: u32) {
        unsafe { lansec_metal_resize(self.ptr, width, height) }
    }
}

impl Drop for MetalLayer {
    fn drop(&mut self) {
        unsafe { lansec_metal_close(self.ptr) }
    }
}
