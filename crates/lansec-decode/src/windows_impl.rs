//! Media Foundation HEVC decoder with D3D11 surfaces, then Video Processor CSC to BGRA.

use std::mem::ManuallyDrop;
use std::time::Instant;

use lansec_capture::GpuContext;
use lansec_protocol::{Chroma, CodecCap, DecodeBackend};
use tracing::{info, warn};
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaBuffer, IMFSample, IMFTransform, MFCreateDXGIDeviceManager,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFTEnumEx, MFStartup, CODECAPI_AVLowLatencyMode,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFMediaType_Video,
    MFVideoFormat_AYUV, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFSTARTUP_FULL,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

use crate::{DecodeError, DecodedFrame, DecodedInner, HardwareDecoder, Result};

pub fn probe() -> Vec<CodecCap> {
    let hevc_mft = unsafe { hevc_decoder_available() };
    if hevc_mft {
        info!("Media Foundation HEVC decoder MFT registered");
    } else {
        warn!("HEVC decoder MFT not registered; install HEVC Video Extensions for Windows client decode");
        return Vec::new();
    }
    let yuv444 = unsafe { hevc_decoder_ayuv() };
    if yuv444 {
        info!("MF HEVC decoder lists AYUV (4:4:4)");
    } else {
        info!("MF HEVC decoder: 4:2:0 only (no AYUV output type)");
    }
    let mut out = vec![CodecCap::decode(DecodeBackend::D3d11va, Chroma::Yuv420, 3840, 2160)];
    if yuv444 {
        out.insert(0, CodecCap::decode(DecodeBackend::D3d11va, Chroma::Yuv444, 3840, 2160));
    }
    out
}

unsafe fn hevc_decoder_available() -> bool {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    find_hevc_mft().is_ok()
}

unsafe fn hevc_decoder_ayuv() -> bool {
    let Ok(transform) = find_hevc_mft() else {
        return false;
    };
    let input = match MFCreateMediaType() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let _ = input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
    let _ = input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC);
    if transform.SetInputType(0, &input, 0).is_err() {
        return false;
    }
    for i in 0..32u32 {
        let Ok(ty) = transform.GetOutputAvailableType(0, i) else {
            break;
        };
        if ty.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default() == MFVideoFormat_AYUV {
            return true;
        }
    }
    false
}

pub fn open(chroma: Chroma, width: u32, height: u32) -> Result<Box<dyn HardwareDecoder>> {
    let gpu = GpuContext::new().map_err(|e| DecodeError::Message(e.to_string()))?;
    open_with_gpu(&gpu, chroma, width, height)
}

pub fn open_with_gpu(
    gpu: &GpuContext,
    chroma: Chroma,
    width: u32,
    height: u32,
) -> Result<Box<dyn HardwareDecoder>> {
    unsafe { open_inner(gpu, chroma, width, height) }.map_err(|e| DecodeError::Message(e.to_string()))
}

fn pack_wh(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | (h as u64)
}

unsafe fn find_hevc_mft() -> windows::core::Result<IMFTransform> {
    let info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_HEVC,
    };
    let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        flags,
        Some(&info),
        None,
        &mut activates,
        &mut count,
    )?;
    if count == 0 || activates.is_null() {
        return Err(windows::core::Error::from(E_FAIL));
    }
    let slice = std::slice::from_raw_parts_mut(activates, count as usize);
    let mut found = None;
    for slot in slice.iter_mut() {
        if found.is_none() {
            if let Some(act) = slot.take() {
                if let Ok(t) = act.ActivateObject::<IMFTransform>() {
                    found = Some(t);
                }
            }
        } else {
            *slot = None;
        }
    }
    CoTaskMemFree(Some(activates as *const _));
    found.ok_or_else(|| windows::core::Error::from(E_FAIL))
}

unsafe fn open_inner(
    gpu: &GpuContext,
    chroma: Chroma,
    width: u32,
    height: u32,
) -> windows::core::Result<Box<dyn HardwareDecoder>> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    if let Ok(mt) = gpu.device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
    let manager = manager.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    manager.ResetDevice(&gpu.device, reset_token)?;

    let transform = find_hevc_mft()?;
    if let Ok(attrs) = transform.GetAttributes() {
        let _ = attrs.SetUINT32(&MF_SA_D3D11_AWARE, 1);
        let _ = attrs.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
    }
    transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

    let input = MFCreateMediaType()?;
    input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
    input.SetUINT64(&MF_MT_FRAME_SIZE, pack_wh(width.max(1), height.max(1)))?;
    input.SetUINT64(&MF_MT_FRAME_RATE, pack_wh(60, 1))?;
    input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    transform.SetInputType(0, &input, 0)?;

    let prefer = if chroma == Chroma::Yuv444 {
        [MFVideoFormat_AYUV, MFVideoFormat_NV12]
    } else {
        [MFVideoFormat_NV12, MFVideoFormat_AYUV]
    };
    set_output_type(&transform, width, height, &prefer)?;

    transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
    transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

    let provides = transform.GetOutputStreamInfo(0)?.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
    info!(?chroma, width, height, provides, "MF HEVC decoder (D3D11) ready");

    Ok(Box::new(MfDecoder {
        width,
        height,
        gpu: GpuHolder {
            device: gpu.device.clone(),
            context: gpu.context.clone(),
        },
        _manager: manager,
        transform,
        provides_samples: provides,
        origin: Instant::now(),
        csc: VideoCsc::try_new(&gpu.device, &gpu.context, width, height).ok(),
        sample_clock: 0,
    }))
}

unsafe fn set_output_type(transform: &IMFTransform, width: u32, height: u32, prefer: &[GUID]) -> windows::core::Result<()> {
    for want in prefer {
        for i in 0..32u32 {
            let Ok(ty) = transform.GetOutputAvailableType(0, i) else {
                break;
            };
            let sub = ty.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
            if sub != *want {
                continue;
            }
            let _ = ty.SetUINT64(&MF_MT_FRAME_SIZE, pack_wh(width.max(1), height.max(1)));
            if transform.SetOutputType(0, &ty, 0).is_ok() {
                return Ok(());
            }
        }
    }
    if let Ok(ty) = transform.GetOutputAvailableType(0, 0) {
        transform.SetOutputType(0, &ty, 0)?;
        return Ok(());
    }
    Err(windows::core::Error::from(E_FAIL))
}

struct GpuHolder {
    #[allow(dead_code)]
    device: ID3D11Device,
    #[allow(dead_code)]
    context: ID3D11DeviceContext,
}

struct VideoCsc {
    video: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    bgra: ID3D11Texture2D,
}

impl VideoCsc {
    fn try_new(device: &ID3D11Device, ctx: &ID3D11DeviceContext, width: u32, height: u32) -> windows::core::Result<Self> {
        unsafe {
            let video: ID3D11VideoDevice = device.cast()?;
            let vctx: ID3D11VideoContext = ctx.cast()?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                InputWidth: width.max(1),
                InputHeight: height.max(1),
                OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                OutputWidth: width.max(1),
                OutputHeight: height.max(1),
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video.CreateVideoProcessor(&enumerator, 0)?;
            let mut td = D3D11_TEXTURE2D_DESC::default();
            td.Width = width.max(1);
            td.Height = height.max(1);
            td.MipLevels = 1;
            td.ArraySize = 1;
            td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
            td.SampleDesc.Count = 1;
            td.Usage = D3D11_USAGE_DEFAULT;
            td.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
            let mut bgra = None;
            device.CreateTexture2D(&td, None, Some(&mut bgra))?;
            Ok(Self {
                video,
                vctx,
                enumerator,
                processor,
                bgra: bgra.unwrap(),
            })
        }
    }

    fn convert(&self, src: &ID3D11Texture2D, array_slice: u32) -> windows::core::Result<ID3D11Texture2D> {
        unsafe {
            let mut idesc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC::default();
            idesc.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
            idesc.Anonymous.Texture2D = D3D11_TEX2D_VPIV {
                MipSlice: 0,
                ArraySlice: array_slice,
            };
            let src_res: ID3D11Resource = src.cast()?;
            let mut iview = None;
            self.video
                .CreateVideoProcessorInputView(&src_res, &self.enumerator, &idesc, Some(&mut iview))?;
            let mut odesc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC::default();
            odesc.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
            odesc.Anonymous.Texture2D = D3D11_TEX2D_VPOV { MipSlice: 0 };
            let dst_res: ID3D11Resource = self.bgra.cast()?;
            let mut oview = None;
            self.video
                .CreateVideoProcessorOutputView(&dst_res, &self.enumerator, &odesc, Some(&mut oview))?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM::default();
            stream.Enable = true.into();
            stream.pInputSurface = ManuallyDrop::new(iview);
            self.vctx.VideoProcessorBlt(
                &self.processor,
                &oview.unwrap(),
                0,
                std::slice::from_ref(&stream),
            )?;
            let _ = ManuallyDrop::take(&mut stream.pInputSurface);
            let _ = ManuallyDrop::take(&mut stream.pInputSurfaceRight);
            Ok(self.bgra.clone())
        }
    }
}

struct MfDecoder {
    width: u32,
    height: u32,
    #[allow(dead_code)]
    gpu: GpuHolder,
    _manager: IMFDXGIDeviceManager,
    transform: IMFTransform,
    provides_samples: bool,
    origin: Instant,
    csc: Option<VideoCsc>,
    sample_clock: i64,
}

unsafe impl Send for MfDecoder {}

impl HardwareDecoder for MfDecoder {
    fn backend(&self) -> DecodeBackend {
        DecodeBackend::D3d11va
    }

    fn decode(&mut self, annexb: &[u8], _is_keyframe: bool) -> Result<Option<DecodedFrame>> {
        if annexb.is_empty() {
            return Ok(None);
        }
        unsafe { self.feed(annexb) }.map_err(|e| DecodeError::Message(e.to_string()))
    }
}

impl MfDecoder {
    unsafe fn feed(&mut self, annexb: &[u8]) -> windows::core::Result<Option<DecodedFrame>> {
        let sample: IMFSample = MFCreateSample()?;
        let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(annexb.len() as u32)?;
        let mut max = 0u32;
        let mut current = 0u32;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, Some(&mut max), Some(&mut current))?;
        std::ptr::copy_nonoverlapping(annexb.as_ptr(), ptr, annexb.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(annexb.len() as u32)?;
        sample.AddBuffer(&buffer)?;
        self.sample_clock += 10_000;
        sample.SetSampleTime(self.sample_clock)?;
        sample.SetSampleDuration(10_000)?;
        self.transform.ProcessInput(0, &sample, 0)?;
        self.drain()
    }

    unsafe fn drain(&mut self) -> windows::core::Result<Option<DecodedFrame>> {
        loop {
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(if self.provides_samples {
                    None
                } else {
                    Some(self.make_output_sample()?)
                }),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            match self.transform.ProcessOutput(0, std::slice::from_mut(&mut out), &mut status) {
                Ok(()) => {
                    let sample = ManuallyDrop::take(&mut out.pSample);
                    let _ = ManuallyDrop::take(&mut out.pEvents);
                    if let Some(sample) = sample {
                        return self.sample_to_frame(&sample);
                    }
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    let _ = ManuallyDrop::take(&mut out.pSample);
                    let _ = ManuallyDrop::take(&mut out.pEvents);
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let _ = ManuallyDrop::take(&mut out.pSample);
                    let _ = ManuallyDrop::take(&mut out.pEvents);
                    let prefer = [MFVideoFormat_NV12, MFVideoFormat_AYUV];
                    set_output_type(&self.transform, self.width, self.height, &prefer)?;
                    continue;
                }
                Err(e) => {
                    let _ = ManuallyDrop::take(&mut out.pSample);
                    let _ = ManuallyDrop::take(&mut out.pEvents);
                    return Err(e);
                }
            }
        }
    }

    unsafe fn make_output_sample(&self) -> windows::core::Result<IMFSample> {
        let info = self.transform.GetOutputStreamInfo(0)?;
        let sample = MFCreateSample()?;
        let buf = MFCreateMemoryBuffer(info.cbSize.max(1))?;
        sample.AddBuffer(&buf)?;
        Ok(sample)
    }

    unsafe fn sample_to_frame(&self, sample: &IMFSample) -> windows::core::Result<Option<DecodedFrame>> {
        let buffer = sample.GetBufferByIndex(0)?;
        let tex = match buffer.cast::<IMFDXGIBuffer>() {
            Ok(dxgi) => {
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                if raw.is_null() {
                    return Ok(None);
                }
                let tex = ID3D11Texture2D::from_raw(raw as *mut _);
                let slice = dxgi.GetSubresourceIndex().unwrap_or(0);
                if let Some(csc) = self.csc.as_ref() {
                    match csc.convert(&tex, slice) {
                        Ok(bgra) => bgra,
                        Err(e) => {
                            warn!("video processor CSC failed: {e}");
                            tex
                        }
                    }
                } else {
                    tex
                }
            }
            Err(_) => return Ok(None),
        };
        Ok(Some(DecodedFrame {
            width: self.width,
            height: self.height,
            decode_done_us: self.origin.elapsed().as_micros() as u64,
            inner: DecodedInner::D3d11(tex),
        }))
    }
}
