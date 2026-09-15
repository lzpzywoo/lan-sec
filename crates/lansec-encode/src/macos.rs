use std::ffi::c_void;
use std::ptr::NonNull;

use lansec_protocol::{Chroma, CodecCap, EncodeBackend};
use tracing::{info, warn};

use crate::{EncodeError, EncodedAu, EncoderConfig, HardwareEncoder, Result};
use lansec_capture::GpuFrame;

pub fn probe() -> Vec<CodecCap> {
    let mut caps = vec![CodecCap::encode(
        EncodeBackend::VideoToolbox,
        Chroma::Yuv420,
        3840,
        2160,
    )];
    if probe_hevc_main444() {
        info!("VideoToolbox HEVC Main 4:4:4 hardware encoder available");
        caps.insert(
            0,
            CodecCap::encode(EncodeBackend::VideoToolbox, Chroma::Yuv444, 3840, 2160),
        );
    } else {
        warn!("VideoToolbox hardware 4:4:4 encode not available; host will negotiate 4:2:0");
    }
    caps
}

fn probe_hevc_main444() -> bool {
    // Undocumented kVTProfileLevel_HEVC_Main444_AutoLevel. Require hardware; never silently
    // fall back to software encode.
    unsafe { vt_probe_444() }
}

pub fn open(cfg: EncoderConfig) -> Result<Box<dyn HardwareEncoder>> {
    let chroma = if cfg.prefer_444 && probe_hevc_main444() {
        Chroma::Yuv444
    } else {
        if cfg.prefer_444 {
            warn!("Mac host 4:4:4 hardware encode unavailable; using HEVC 4:2:0");
        }
        Chroma::Yuv420
    };
    let session = unsafe { vt_open(cfg.width, cfg.height, cfg.bitrate_bps, chroma == Chroma::Yuv444) }
        .ok_or(EncodeError::Unavailable)?;
    Ok(Box::new(VtEncoder {
        session,
        chroma,
        width: cfg.width,
        height: cfg.height,
    }))
}

struct VtEncoder {
    session: VtSession,
    chroma: Chroma,
    width: u32,
    height: u32,
}

unsafe impl Send for VtEncoder {}

impl HardwareEncoder for VtEncoder {
    fn encode(&mut self, frame: &GpuFrame, force_idr: bool) -> Result<Option<EncodedAu>> {
        let pb = frame
            .cv_pixel_buffer()
            .ok_or_else(|| EncodeError::Message("expected CVPixelBuffer".into()))?;
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        let mut len = 0i32;
        let mut key = 0i32;
        let ok = unsafe {
            vt_encode(
                self.session.0.as_ptr(),
                pb,
                force_idr as i32,
                buf.as_mut_ptr(),
                buf.len() as i32,
                &mut len,
                &mut key,
            )
        };
        if ok == 0 || len <= 0 {
            return Ok(None);
        }
        buf.truncate(len as usize);
        Ok(Some(EncodedAu {
            annexb: buf,
            is_keyframe: key != 0 || force_idr,
            chroma: self.chroma,
            backend: EncodeBackend::VideoToolbox,
        }))
    }

    fn set_bitrate(&mut self, bps: u32) {
        unsafe { vt_set_bitrate(self.session.0.as_ptr(), bps) };
    }

    fn backend(&self) -> EncodeBackend {
        EncodeBackend::VideoToolbox
    }

    fn chroma(&self) -> Chroma {
        self.chroma
    }
}

struct VtSession(NonNull<c_void>);

impl Drop for VtSession {
    fn drop(&mut self) {
        unsafe { vt_close(self.0.as_ptr()) }
    }
}

extern "C" {
    fn lansec_vt_probe_444() -> i32;
    fn lansec_vt_open(width: u32, height: u32, bitrate: u32, yuv444: i32) -> *mut c_void;
    fn lansec_vt_close(s: *mut c_void);
    fn lansec_vt_set_bitrate(s: *mut c_void, bitrate: u32);
    fn lansec_vt_encode(
        s: *mut c_void,
        pixel_buffer: *mut c_void,
        force_idr: i32,
        out: *mut u8,
        cap: i32,
        len: *mut i32,
        key: *mut i32,
    ) -> i32;
}

unsafe fn vt_probe_444() -> bool {
    lansec_vt_probe_444() != 0
}

unsafe fn vt_open(width: u32, height: u32, bitrate: u32, yuv444: bool) -> Option<VtSession> {
    let p = lansec_vt_open(width, height, bitrate, i32::from(yuv444));
    NonNull::new(p).map(VtSession)
}

unsafe fn vt_close(p: *mut c_void) {
    lansec_vt_close(p)
}

unsafe fn vt_set_bitrate(p: *mut c_void, bps: u32) {
    lansec_vt_set_bitrate(p, bps)
}

unsafe fn vt_encode(
    p: *mut c_void,
    pb: *mut c_void,
    force_idr: i32,
    out: *mut u8,
    cap: i32,
    len: *mut i32,
    key: *mut i32,
) -> i32 {
    lansec_vt_encode(p, pb, force_idr, out, cap, len, key)
}
