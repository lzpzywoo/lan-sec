use lansec_capture::{GpuContext, GpuFrame};
use lansec_protocol::{Chroma, CodecCap, EncodeBackend};
use tracing::{info, warn};
use windows::core::Interface;

use crate::{EncodeError, EncodedAu, EncoderConfig, HardwareEncoder, Result};

#[link(name = "lansec_nvenc")]
unsafe extern "C" {
    fn lansec_nvenc_available() -> i32;
    fn lansec_nvenc_probe_yuv444(device: *mut std::ffi::c_void) -> i32;
    fn lansec_nvenc_open(
        device: *mut std::ffi::c_void,
        ctx: *mut std::ffi::c_void,
        width: u32,
        height: u32,
        bitrate: u32,
        yuv444: i32,
    ) -> *mut std::ffi::c_void;
    fn lansec_nvenc_close(enc: *mut std::ffi::c_void);
    fn lansec_nvenc_set_bitrate(enc: *mut std::ffi::c_void, bitrate: u32) -> i32;
    fn lansec_nvenc_encode(
        enc: *mut std::ffi::c_void,
        src: *mut std::ffi::c_void,
        force_idr: i32,
        out: *mut u8,
        out_cap: i32,
        out_len: *mut i32,
        is_key: *mut i32,
    ) -> i32;
}

pub fn probe() -> Vec<CodecCap> {
    let mut out = Vec::new();
    let gpu = GpuContext::new().ok();
    let nvenc = unsafe { lansec_nvenc_available() } != 0;
    if nvenc {
        let yuv444 = gpu
            .as_ref()
            .map(|g| unsafe { lansec_nvenc_probe_yuv444(g.as_device_ptr()) } != 0)
            .unwrap_or(false);
        info!(yuv444, "NVENC present");
        if yuv444 {
            out.push(CodecCap::encode(EncodeBackend::Nvenc, Chroma::Yuv444, 3840, 2160));
        }
        out.push(CodecCap::encode(EncodeBackend::Nvenc, Chroma::Yuv420, 3840, 2160));
    }
    if let Some(gpu) = gpu.as_ref() {
        if matches!(gpu.vendor, lansec_capture::GpuVendor::Intel) {
            let intel = crate::qsv::probe();
            if intel.hevc {
                if intel.yuv444 {
                    info!("Intel GPU: hardware HEVC MFT (4:4:4 + 4:2:0)");
                    out.push(CodecCap::encode(EncodeBackend::Qsv, Chroma::Yuv444, 3840, 2160));
                } else {
                    info!("Intel GPU: hardware HEVC MFT (4:2:0; 4:4:4 not advertised)");
                }
                out.push(CodecCap::encode(EncodeBackend::Qsv, Chroma::Yuv420, 3840, 2160));
            } else {
                warn!("Intel GPU present but no hardware HEVC encoder MFT");
            }
        }
    }
    out
}

pub fn open_with_gpu(gpu: &GpuContext, cfg: EncoderConfig) -> Result<Box<dyn HardwareEncoder>> {
    if unsafe { lansec_nvenc_available() } != 0 {
        let want444 = cfg.prefer_444 && unsafe { lansec_nvenc_probe_yuv444(gpu.as_device_ptr()) } != 0;
        let ptr = unsafe {
            lansec_nvenc_open(
                gpu.as_device_ptr(),
                gpu.as_context_ptr(),
                cfg.width,
                cfg.height,
                cfg.bitrate_bps,
                i32::from(want444),
            )
        };
        if !ptr.is_null() {
            info!(yuv444 = want444, "NVENC HEVC encoder opened");
            return Ok(Box::new(NvencEncoder {
                ptr,
                chroma: if want444 { Chroma::Yuv444 } else { Chroma::Yuv420 },
                scratch: vec![0u8; 8 * 1024 * 1024],
            }));
        }
        warn!("NVENC open failed, trying next backend");
    }
    if matches!(gpu.vendor, lansec_capture::GpuVendor::Intel) {
        return crate::qsv::open(gpu, cfg);
    }
    Err(EncodeError::Unavailable)
}

struct NvencEncoder {
    ptr: *mut std::ffi::c_void,
    chroma: Chroma,
    scratch: Vec<u8>,
}

unsafe impl Send for NvencEncoder {}

impl HardwareEncoder for NvencEncoder {
    fn backend(&self) -> EncodeBackend {
        EncodeBackend::Nvenc
    }

    fn chroma(&self) -> Chroma {
        self.chroma
    }

    fn set_bitrate(&mut self, bps: u32) {
        unsafe {
            lansec_nvenc_set_bitrate(self.ptr, bps);
        }
    }

    fn encode(&mut self, frame: &GpuFrame, force_idr: bool) -> Result<Option<EncodedAu>> {
        let tex = frame
            .d3d11_texture()
            .ok_or_else(|| EncodeError::Message("expected D3D11 texture".into()))?;
        let mut len = 0i32;
        let mut key = 0i32;
        let ok = unsafe {
            lansec_nvenc_encode(
                self.ptr,
                Interface::as_raw(tex),
                i32::from(force_idr),
                self.scratch.as_mut_ptr(),
                self.scratch.len() as i32,
                &mut len,
                &mut key,
            )
        };
        if ok == 0 || len <= 0 {
            return Ok(None);
        }
        Ok(Some(EncodedAu {
            annexb: self.scratch[..len as usize].to_vec(),
            is_keyframe: key != 0 || force_idr,
            chroma: self.chroma,
            backend: EncodeBackend::Nvenc,
        }))
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe { lansec_nvenc_close(self.ptr) }
    }
}
