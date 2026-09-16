use std::time::Instant;

use tracing::{info, warn};
use windows::core::{Interface, Result as WinResult};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use crate::{CaptureError, FrameInfo, GpuFrame, GpuFrameInner, Result};

const NVIDIA: u32 = 0x10DE;
const INTEL: u32 = 0x8086;
const AMD: u32 = 0x1002;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Intel,
    Amd,
    Other(u32),
}

impl GpuVendor {
    pub fn from_id(id: u32) -> Self {
        match id {
            NVIDIA => Self::Nvidia,
            INTEL => Self::Intel,
            AMD => Self::Amd,
            other => Self::Other(other),
        }
    }
}

pub struct GpuContext {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: IDXGIAdapter1,
    pub vendor: GpuVendor,
    pub description: String,
}

impl GpuContext {
    pub fn new() -> Result<Self> {
        unsafe { Self::new_inner() }.map_err(|e| CaptureError::Message(e.to_string()))
    }

    unsafe fn new_inner() -> WinResult<Self> {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0)?;
        let desc = adapter.GetDesc1()?;
        let vendor = GpuVendor::from_id(desc.VendorId);
        let description = String::from_utf16_lossy(desc.Description.split(|c| *c == 0).next().unwrap_or(&[]));
        info!(vendor = ?vendor, adapter = %description, "D3D11 adapter");

        let mut device = None;
        let mut context = None;
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.unwrap();
        let context = context.unwrap();
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok(Self {
            device,
            context,
            adapter,
            vendor,
            description,
        })
    }

    pub fn as_device_ptr(&self) -> *mut std::ffi::c_void {
        self.device.as_raw()
    }

    pub fn as_context_ptr(&self) -> *mut std::ffi::c_void {
        self.context.as_raw()
    }
}

pub struct D3d11Frame {
    pub texture: ID3D11Texture2D,
}

pub struct DxgiCapture {
    gpu: GpuContext,
    duplication: IDXGIOutputDuplication,
    owned: ID3D11Texture2D,
    width: u32,
    height: u32,
    origin: Instant,
    have_frame: bool,
}

impl DxgiCapture {
    pub fn new() -> Result<Self> {
        let gpu = GpuContext::new()?;
        unsafe { Self::with_gpu(gpu) }.map_err(|e| CaptureError::Message(e.to_string()))
    }

    pub fn gpu(&self) -> &GpuContext {
        &self.gpu
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn next_frame(&mut self) -> Result<Option<GpuFrame>> {
        unsafe { self.acquire() }
    }

    unsafe fn with_gpu(gpu: GpuContext) -> WinResult<Self> {
        let output = gpu.adapter.EnumOutputs(0)?;
        let output1: IDXGIOutput1 = output.cast()?;
        let duplication = output1.DuplicateOutput(&gpu.device)?;
        let desc = duplication.GetDesc();
        let width = desc.ModeDesc.Width;
        let height = desc.ModeDesc.Height;
        let mut td = D3D11_TEXTURE2D_DESC::default();
        td.Width = width;
        td.Height = height;
        td.MipLevels = 1;
        td.ArraySize = 1;
        td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        td.SampleDesc.Count = 1;
        td.Usage = D3D11_USAGE_DEFAULT;
        td.BindFlags = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
        let mut owned = None;
        gpu.device.CreateTexture2D(&td, None, Some(&mut owned))?;
        info!(width, height, "DXGI desktop duplication (GPU CopyResource, no CPU readback)");
        Ok(Self {
            gpu,
            duplication,
            owned: owned.unwrap(),
            width,
            height,
            origin: Instant::now(),
            have_frame: false,
        })
    }

    fn recreate(&mut self) -> Result<()> {
        warn!("DXGI access lost, recreating duplication");
        *self = Self::new()?;
        Ok(())
    }

    unsafe fn acquire(&mut self) -> Result<Option<GpuFrame>> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match self.duplication.AcquireNextFrame(0, &mut info, &mut resource) {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                if self.have_frame {
                    return Ok(Some(self.repeat_frame()));
                }
                return Ok(None);
            }
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                let _ = self.duplication.ReleaseFrame();
                self.recreate()?;
                return Ok(None);
            }
            Err(e) => return Err(CaptureError::Message(e.to_string())),
        }
        let Some(resource) = resource else {
            let _ = self.duplication.ReleaseFrame();
            return Ok(None);
        };
        let acquired: ID3D11Texture2D = resource.cast().map_err(|e| CaptureError::Message(e.to_string()))?;
        let src: ID3D11Resource = acquired.cast()?;
        let dst: ID3D11Resource = self.owned.cast()?;
        self.gpu.context.CopyResource(&dst, &src);
        let _ = self.duplication.ReleaseFrame();
        self.have_frame = true;
        Ok(Some(GpuFrame {
            info: FrameInfo {
                width: self.width,
                height: self.height,
                capture_us: self.origin.elapsed().as_micros() as u64,
                fresh: true,
                content_gen: 0,
            },
            inner: GpuFrameInner::D3d11(D3d11Frame {
                texture: self.owned.clone(),
            }),
        }))
    }

    fn repeat_frame(&self) -> GpuFrame {
        GpuFrame {
            info: FrameInfo {
                width: self.width,
                height: self.height,
                capture_us: self.origin.elapsed().as_micros() as u64,
                fresh: false,
                content_gen: 0,
            },
            inner: GpuFrameInner::D3d11(D3d11Frame {
                texture: self.owned.clone(),
            }),
        }
    }
}

impl From<windows::core::Error> for CaptureError {
    fn from(value: windows::core::Error) -> Self {
        CaptureError::Message(value.to_string())
    }
}

#[allow(dead_code)]
fn _formats() -> DXGI_FORMAT {
    DXGI_FORMAT_B8G8R8A8_UNORM
}

#[allow(dead_code)]
fn _dxgi_device(device: &ID3D11Device) -> WinResult<IDXGIDevice> {
    device.cast()
}
