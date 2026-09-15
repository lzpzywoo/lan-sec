use lansec_protocol::{Chroma, CodecCap, DecodeBackend};

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("{0}")]
    Message(String),
    #[error("no hardware decoder")]
    Unavailable,
}

pub type Result<T> = std::result::Result<T, DecodeError>;

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub decode_done_us: u64,
    inner: DecodedInner,
}

enum DecodedInner {
    #[cfg(windows)]
    #[allow(dead_code)]
    D3d11(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D),
    #[cfg(target_os = "macos")]
    PixelBuffer(*mut std::ffi::c_void),
    #[cfg(not(any(windows, target_os = "macos")))]
    None,
}

impl DecodedFrame {
    #[cfg(windows)]
    pub fn d3d11_texture(&self) -> Option<&windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
        match &self.inner {
            DecodedInner::D3d11(t) => Some(t),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn cv_pixel_buffer(&self) -> Option<*mut std::ffi::c_void> {
        match &self.inner {
            DecodedInner::PixelBuffer(p) => Some(*p),
        }
    }
}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let DecodedInner::PixelBuffer(p) = self.inner {
            if !p.is_null() {
                unsafe { macos::release_cf(p) }
            }
        }
    }
}

pub trait HardwareDecoder: Send {
    fn decode(&mut self, annexb: &[u8], is_keyframe: bool) -> Result<Option<DecodedFrame>>;
    fn backend(&self) -> DecodeBackend;
}

pub fn probe_decode() -> Vec<CodecCap> {
    #[cfg(windows)]
    {
        return windows_impl::probe();
    }
    #[cfg(target_os = "macos")]
    {
        return macos::probe();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    Vec::new()
}

pub fn open_decoder(chroma: Chroma, width: u32, height: u32) -> Result<Box<dyn HardwareDecoder>> {
    #[cfg(windows)]
    {
        return windows_impl::open(chroma, width, height);
    }
    #[cfg(target_os = "macos")]
    {
        return macos::open(chroma, width, height);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (chroma, width, height);
        Err(DecodeError::Unavailable)
    }
}

#[cfg(windows)]
pub fn open_decoder_with_gpu(
    gpu: &lansec_capture::GpuContext,
    chroma: Chroma,
    width: u32,
    height: u32,
) -> Result<Box<dyn HardwareDecoder>> {
    windows_impl::open_with_gpu(gpu, chroma, width, height)
}

#[cfg(windows)]
mod windows_impl;
#[cfg(target_os = "macos")]
mod macos;
