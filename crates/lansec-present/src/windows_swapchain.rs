//! DXGI flip-sequential swap chain. VSync on; presenter already dropped late frames.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::core::{w, Interface};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassExW, ShowWindow, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, SW_SHOW,
    WINDOW_EX_STYLE, WM_DESTROY, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

use crate::PresentError;

pub struct Swapchain {
    swap: IDXGISwapChain1,
    ctx: ID3D11DeviceContext,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_DESTROY {
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

impl Swapchain {
    pub fn create_window(title: &str, width: u32, height: u32) -> Result<HWND, PresentError> {
        unsafe {
            let class = w!("lansec_present");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: GetModuleHandleW(None)
                    .map_err(|e| PresentError::Message(e.to_string()))?
                    .into(),
                lpszClassName: class,
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);
            let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                windows::core::PCWSTR(wide.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                width as i32,
                height as i32,
                None,
                None,
                None,
                None,
            )
            .map_err(|e| PresentError::Message(e.to_string()))?;
            let _ = ShowWindow(hwnd, SW_SHOW);
            Ok(hwnd)
        }
    }

    pub fn from_hwnd(
        device: &ID3D11Device,
        ctx: ID3D11DeviceContext,
        hwnd: HWND,
        width: u32,
        height: u32,
    ) -> Result<Self, PresentError> {
        unsafe {
            let dxgi: IDXGIDevice = device.cast().map_err(|e| PresentError::Message(e.to_string()))?;
            let adapter = dxgi.GetAdapter().map_err(|e| PresentError::Message(e.to_string()))?;
            let factory: IDXGIFactory2 = adapter
                .GetParent()
                .map_err(|e| PresentError::Message(e.to_string()))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: Default::default(),
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: Default::default(),
                Flags: DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH.0 as u32,
            };
            let swap = factory
                .CreateSwapChainForHwnd(device, hwnd, &desc, None, None)
                .map_err(|e| PresentError::Message(e.to_string()))?;
            Ok(Self { swap, ctx })
        }
    }

    pub fn hwnd_from_winit(window: &winit::window::Window) -> Result<HWND, PresentError> {
        match window
            .window_handle()
            .map_err(|e| PresentError::Message(e.to_string()))?
            .as_raw()
        {
            RawWindowHandle::Win32(h) => Ok(HWND(h.hwnd.get() as *mut std::ffi::c_void)),
            _ => Err(PresentError::Message("not a Win32 window".into())),
        }
    }

    pub fn blit_and_present(&self, src: &ID3D11Texture2D) -> Result<(), PresentError> {
        unsafe {
            let back: ID3D11Texture2D = self
                .swap
                .GetBuffer(0)
                .map_err(|e| PresentError::Message(e.to_string()))?;
            let dst: ID3D11Resource = back.cast().map_err(|e| PresentError::Message(e.to_string()))?;
            let src_r: ID3D11Resource = src.cast().map_err(|e| PresentError::Message(e.to_string()))?;
            self.ctx.CopyResource(&dst, &src_r);
            self.swap
                .Present(1, DXGI_PRESENT(0))
                .ok()
                .map_err(|e| PresentError::Message(e.to_string()))?;
        }
        Ok(())
    }
}
