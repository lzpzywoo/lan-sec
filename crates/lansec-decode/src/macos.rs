use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Instant;

use lansec_protocol::{Chroma, CodecCap, DecodeBackend};

use crate::{DecodeError, DecodedFrame, DecodedInner, HardwareDecoder, Result};

unsafe extern "C" {
    fn lansec_vt_probe_decode_444() -> i32;
    fn lansec_vt_dec_open(yuv444: i32) -> *mut c_void;
    fn lansec_vt_dec_close(s: *mut c_void);
    fn lansec_vt_dec_decode(s: *mut c_void, data: *const u8, len: i32, pb: *mut *mut c_void) -> i32;
    fn lansec_cf_release(obj: *mut c_void);
}

pub(crate) fn release_cf(p: *mut c_void) {
    unsafe { lansec_cf_release(p) }
}

pub fn probe() -> Vec<CodecCap> {
    let mut v = vec![CodecCap::decode(
        DecodeBackend::VideoToolbox,
        Chroma::Yuv420,
        3840,
        2160,
    )];
    if unsafe { lansec_vt_probe_decode_444() } != 0 {
        v.insert(
            0,
            CodecCap::decode(DecodeBackend::VideoToolbox, Chroma::Yuv444, 3840, 2160),
        );
    }
    v
}

pub fn open(chroma: Chroma, _width: u32, _height: u32) -> Result<Box<dyn HardwareDecoder>> {
    let p = unsafe { lansec_vt_dec_open(i32::from(chroma == Chroma::Yuv444)) };
    let p = NonNull::new(p).ok_or(DecodeError::Unavailable)?;
    Ok(Box::new(VtDecoder {
        ptr: p,
        origin: Instant::now(),
    }))
}

struct VtDecoder {
    ptr: NonNull<c_void>,
    origin: Instant,
}

unsafe impl Send for VtDecoder {}

impl HardwareDecoder for VtDecoder {
    fn backend(&self) -> DecodeBackend {
        DecodeBackend::VideoToolbox
    }

    fn decode(&mut self, annexb: &[u8], _is_keyframe: bool) -> Result<Option<DecodedFrame>> {
        let mut pb: *mut c_void = std::ptr::null_mut();
        let ok = unsafe {
            lansec_vt_dec_decode(self.ptr.as_ptr(), annexb.as_ptr(), annexb.len() as i32, &mut pb)
        };
        if ok == 0 || pb.is_null() {
            return Ok(None);
        }
        Ok(Some(DecodedFrame {
            width: 0,
            height: 0,
            decode_done_us: self.origin.elapsed().as_micros() as u64,
            inner: DecodedInner::PixelBuffer(pb),
        }))
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        unsafe { lansec_vt_dec_close(self.ptr.as_ptr()) }
    }
}
