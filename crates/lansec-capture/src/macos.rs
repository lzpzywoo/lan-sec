use std::ffi::c_void;

use crate::{CaptureError, FrameInfo, GpuFrame, GpuFrameInner, Result};

#[link(name = "lansec_vt")]
unsafe extern "C" {
    fn lansec_sck_start(width: *mut u32, height: *mut u32) -> *mut c_void;
    fn lansec_sck_stop(cap: *mut c_void);
    fn lansec_sck_next(cap: *mut c_void, capture_us: *mut u64) -> *mut c_void;
    fn lansec_sck_next_audio(cap: *mut c_void, out: *mut f32, cap_samples: i32) -> i32;
    fn lansec_cf_release(obj: *mut c_void);
}

pub struct IoSurfaceFrame {
    pub width: u32,
    pub height: u32,
    pub pixel_buffer: *mut c_void,
}

impl Drop for IoSurfaceFrame {
    fn drop(&mut self) {
        if !self.pixel_buffer.is_null() {
            unsafe { lansec_cf_release(self.pixel_buffer) }
        }
    }
}

pub struct SckCapture {
    ptr: *mut c_void,
    width: u32,
    height: u32,
}

unsafe impl Send for SckCapture {}

impl SckCapture {
    pub fn new() -> Result<Self> {
        let mut width = 0u32;
        let mut height = 0u32;
        let ptr = unsafe { lansec_sck_start(&mut width, &mut height) };
        if ptr.is_null() {
            return Err(CaptureError::Message(
                "ScreenCaptureKit failed (grant Screen Recording permission)".into(),
            ));
        }
        tracing::info!(width, height, "ScreenCaptureKit started");
        Ok(Self { ptr, width, height })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn next_frame(&mut self) -> Result<Option<GpuFrame>> {
        let mut ts = 0u64;
        let pb = unsafe { lansec_sck_next(self.ptr, &mut ts) };
        if pb.is_null() {
            return Ok(None);
        }
        Ok(Some(GpuFrame {
            info: FrameInfo {
                width: self.width,
                height: self.height,
                capture_us: ts,
            },
            inner: GpuFrameInner::IoSurface(IoSurfaceFrame {
                width: self.width,
                height: self.height,
                pixel_buffer: pb,
            }),
        }))
    }

    pub fn next_audio(&mut self) -> Vec<f32> {
        let mut buf = vec![0f32; 480 * 2 * 8];
        let n = unsafe { lansec_sck_next_audio(self.ptr, buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 {
            Vec::new()
        } else {
            buf.truncate(n as usize);
            buf
        }
    }
}

impl Drop for SckCapture {
    fn drop(&mut self) {
        unsafe { lansec_sck_stop(self.ptr) }
    }
}
