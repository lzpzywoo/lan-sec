//! GPU-resident desktop frames. CPU never sees raw pixels.

#[cfg(not(any(windows, target_os = "macos")))]
use lansec_protocol::Platform;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("{0}")]
    Message(String),
    #[error("timeout")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, CaptureError>;

#[derive(Debug, Clone, Copy)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub capture_us: u64,
}

pub struct GpuFrame {
    pub info: FrameInfo,
    inner: GpuFrameInner,
}

enum GpuFrameInner {
    #[cfg(windows)]
    D3d11(windows_impl::D3d11Frame),
    #[cfg(target_os = "macos")]
    IoSurface(macos::IoSurfaceFrame),
    #[cfg(not(any(windows, target_os = "macos")))]
    Unsupported,
}

impl GpuFrame {
    #[cfg(windows)]
    pub fn d3d11_texture(&self) -> Option<&windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
        match &self.inner {
            GpuFrameInner::D3d11(f) => Some(&f.texture),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn cv_pixel_buffer(&self) -> Option<*mut std::ffi::c_void> {
        match &self.inner {
            GpuFrameInner::IoSurface(f) => Some(f.pixel_buffer),
        }
    }
}

pub struct CaptureSession {
    #[cfg(windows)]
    inner: windows_impl::DxgiCapture,
    #[cfg(target_os = "macos")]
    inner: macos::SckCapture,
}

impl CaptureSession {
    pub fn open() -> Result<Self> {
        #[cfg(windows)]
        {
            return Ok(Self {
                inner: windows_impl::DxgiCapture::new()?,
            });
        }
        #[cfg(target_os = "macos")]
        {
            return Ok(Self {
                inner: macos::SckCapture::new()?,
            });
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            Err(CaptureError::Message(format!(
                "capture not implemented on {:?}",
                Platform::current()
            )))
        }
    }

    pub fn next_frame(&mut self) -> Result<Option<GpuFrame>> {
        #[cfg(any(windows, target_os = "macos"))]
        {
            return self.inner.next_frame();
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        Err(CaptureError::Message("unsupported".into()))
    }

    pub fn size(&self) -> (u32, u32) {
        #[cfg(any(windows, target_os = "macos"))]
        {
            return self.inner.size();
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        (0, 0)
    }

    #[cfg(windows)]
    pub fn gpu(&self) -> &windows_impl::GpuContext {
        self.inner.gpu()
    }

    #[cfg(target_os = "macos")]
    pub fn next_audio(&mut self) -> Vec<f32> {
        self.inner.next_audio()
    }
}

#[cfg(windows)]
pub mod windows_impl;
#[cfg(windows)]
pub use windows_impl::{GpuContext, GpuVendor};

#[cfg(target_os = "macos")]
pub mod macos;
