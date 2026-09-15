use lansec_capture::GpuFrame;
use lansec_protocol::{Chroma, EncodeBackend};

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("{0}")]
    Message(String),
    #[error("no hardware encoder")]
    Unavailable,
}

pub type Result<T> = std::result::Result<T, EncodeError>;

#[derive(Debug, Clone)]
pub struct EncodedAu {
    pub annexb: Vec<u8>,
    pub is_keyframe: bool,
    pub chroma: Chroma,
    pub backend: EncodeBackend,
}

pub trait HardwareEncoder: Send {
    fn encode(&mut self, frame: &GpuFrame, force_idr: bool) -> Result<Option<EncodedAu>>;
    fn set_bitrate(&mut self, bps: u32);
    fn backend(&self) -> EncodeBackend;
    fn chroma(&self) -> Chroma;
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub bitrate_bps: u32,
    pub prefer_444: bool,
}

pub fn open_encoder(cfg: EncoderConfig) -> Result<Box<dyn HardwareEncoder>> {
    #[cfg(windows)]
    {
        let gpu = lansec_capture::GpuContext::new().map_err(|e| EncodeError::Message(e.to_string()))?;
        return windows_impl::open_with_gpu(&gpu, cfg);
    }
    #[cfg(target_os = "macos")]
    {
        return macos::open(cfg);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = cfg;
        Err(EncodeError::Unavailable)
    }
}

#[cfg(windows)]
pub fn open_encoder_with_gpu(
    gpu: &lansec_capture::GpuContext,
    cfg: EncoderConfig,
) -> Result<Box<dyn HardwareEncoder>> {
    windows_impl::open_with_gpu(gpu, cfg)
}

pub fn probe_encode() -> Vec<lansec_protocol::CodecCap> {
    #[cfg(windows)]
    {
        return windows_impl::probe();
    }
    #[cfg(target_os = "macos")]
    {
        return macos::probe();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
mod qsv;
#[cfg(target_os = "macos")]
mod macos;
