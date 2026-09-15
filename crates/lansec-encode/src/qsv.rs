//! Intel HEVC encode via the hardware Media Foundation encoder MFT
//! (Quick Sync / Intel Graphics). Shares the DXGI capture D3D11 device.
//! Never falls back to a software encoder.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use lansec_capture::{GpuContext, GpuFrame};
use lansec_protocol::{Chroma, EncodeBackend};
use tracing::{info, warn};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_RATIONAL};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH265VProfile_Main_420_8, eAVEncH265VProfile_Main_444_8, ICodecAPI,
    IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator, IMFSample, IMFTransform,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFTEnumEx, MFStartup, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncVideoForceKeyFrame,
    CODECAPI_AVEncVideoMaxNumRefFrame, CODECAPI_AVLowLatencyMode,
    MEError, METransformHaveOutput, METransformNeedInput, MFMediaType_Video, MFSampleExtension_CleanPoint,
    MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFVideoFormat_ARGB32,
    MFVideoFormat_AYUV, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MF_E_NO_EVENTS_AVAILABLE,
    MF_EVENT_FLAG_NO_WAIT, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4};

use crate::{EncodeError, EncodedAu, EncoderConfig, HardwareEncoder, Result};

#[derive(Clone, Copy, Debug)]
pub struct IntelHevcCaps {
    pub hevc: bool,
    pub yuv444: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputFmt {
    Nv12,
    Argb32,
    Ayuv,
}

pub fn probe() -> IntelHevcCaps {
    match unsafe { probe_inner() } {
        Ok(caps) => caps,
        Err(e) => {
            warn!("Intel HEVC encoder MFT probe failed: {e}");
            IntelHevcCaps {
                hevc: false,
                yuv444: false,
            }
        }
    }
}

pub fn open(gpu: &GpuContext, cfg: EncoderConfig) -> Result<Box<dyn HardwareEncoder>> {
    unsafe { open_inner(gpu, cfg) }.map_err(|e| EncodeError::Message(e.to_string()))
}

fn pack_wh(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | (h as u64)
}

unsafe fn variant_ui4(value: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

unsafe fn set_codec_u32(codec: &ICodecAPI, key: &GUID, value: u32) -> windows::core::Result<()> {
    let v = variant_ui4(value);
    codec.SetValue(key, &v)
}

unsafe fn allocated_string(attrs: &windows::Win32::Media::MediaFoundation::IMFAttributes, key: &GUID) -> Option<String> {
    let mut pwstr = PWSTR::null();
    let mut len = 0u32;
    attrs.GetAllocatedString(key, &mut pwstr, &mut len).ok()?;
    if pwstr.is_null() {
        return None;
    }
    let s = pwstr.to_string().ok();
    CoTaskMemFree(Some(pwstr.0 as *const _));
    s
}

unsafe fn find_hevc_hw_encoder() -> windows::core::Result<(IMFTransform, String)> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_HEVC,
    };
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        flags,
        None,
        Some(&output),
        &mut activates,
        &mut count,
    )?;
    if count == 0 || activates.is_null() {
        return Err(windows::core::Error::from(E_FAIL));
    }
    let slice = std::slice::from_raw_parts_mut(activates, count as usize);
    let mut intel = None;
    let mut any = None;
    for slot in slice.iter_mut() {
        let Some(act) = slot.take() else {
            continue;
        };
        let name = allocated_string(&act, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "hardware HEVC encoder".into());
        let vendor = allocated_string(&act, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute).unwrap_or_default();
        if name.to_ascii_lowercase().contains("microsoft") {
            continue;
        }
        if let Ok(t) = act.ActivateObject::<IMFTransform>() {
            if vendor.to_ascii_uppercase().contains("8086") || name.to_ascii_lowercase().contains("intel") {
                if intel.is_none() {
                    intel = Some((t, name));
                    continue;
                }
            } else if any.is_none() {
                any = Some((t, name));
                continue;
            }
        }
    }
    CoTaskMemFree(Some(activates as *const _));
    intel
        .or(any)
        .ok_or_else(|| windows::core::Error::from(E_FAIL))
}

unsafe fn unlock_async(transform: &IMFTransform) -> windows::core::Result<()> {
    let attrs = transform.GetAttributes()?;
    let _ = attrs.SetUINT32(&MF_SA_D3D11_AWARE, 1);
    let _ = attrs.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
    if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
        attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
    }
    Ok(())
}

unsafe fn set_hevc_output(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    bitrate_bps: u32,
    profile: u32,
) -> windows::core::Result<()> {
    let ty = MFCreateMediaType()?;
    ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
    ty.SetUINT64(&MF_MT_FRAME_SIZE, pack_wh(width.max(1), height.max(1)))?;
    ty.SetUINT64(&MF_MT_FRAME_RATE, pack_wh(60, 1))?;
    ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    ty.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps.max(1_000_000))?;
    ty.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
    ty.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
    transform.SetOutputType(0, &ty, 0)
}

unsafe fn set_input_subtype(transform: &IMFTransform, width: u32, height: u32, subtype: GUID) -> windows::core::Result<()> {
    let ty = MFCreateMediaType()?;
    ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    ty.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
    ty.SetUINT64(&MF_MT_FRAME_SIZE, pack_wh(width.max(1), height.max(1)))?;
    ty.SetUINT64(&MF_MT_FRAME_RATE, pack_wh(60, 1))?;
    ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    transform.SetInputType(0, &ty, 0)
}

unsafe fn input_has(transform: &IMFTransform, want: GUID) -> bool {
    for i in 0..32u32 {
        let Ok(ty) = transform.GetInputAvailableType(0, i) else {
            break;
        };
        if ty.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default() == want {
            return true;
        }
    }
    false
}

unsafe fn probe_inner() -> windows::core::Result<IntelHevcCaps> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    let (transform, name) = find_hevc_hw_encoder()?;
    unlock_async(&transform)?;
    // Dummy size is enough to learn profiles / input subtypes.
    if set_hevc_output(&transform, 1920, 1080, 8_000_000, eAVEncH265VProfile_Main_420_8.0 as u32).is_err() {
        warn!(%name, "hardware HEVC encoder MFT rejected Main 4:2:0 output");
        return Ok(IntelHevcCaps {
            hevc: false,
            yuv444: false,
        });
    }
    let nv12 = input_has(&transform, MFVideoFormat_NV12);
    let argb = input_has(&transform, MFVideoFormat_ARGB32);
    let ayuv = input_has(&transform, MFVideoFormat_AYUV);
    let yuv444 = set_hevc_output(&transform, 1920, 1080, 8_000_000, eAVEncH265VProfile_Main_444_8.0 as u32).is_ok()
        && (input_has(&transform, MFVideoFormat_AYUV) || ayuv);
    info!(%name, nv12, argb, ayuv, yuv444, "Intel hardware HEVC encoder MFT");
    Ok(IntelHevcCaps {
        hevc: nv12 || argb || ayuv,
        yuv444,
    })
}

unsafe fn configure_codec(codec: &ICodecAPI, bitrate_bps: u32) {
    let _ = set_codec_u32(codec, &CODECAPI_AVLowLatencyMode, 1);
    let _ = set_codec_u32(codec, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
    let _ = set_codec_u32(codec, &CODECAPI_AVEncCommonMeanBitRate, bitrate_bps.max(1_000_000));
    let _ = set_codec_u32(codec, &CODECAPI_AVEncMPVGOPSize, 60);
    let _ = set_codec_u32(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
    let _ = set_codec_u32(codec, &CODECAPI_AVEncVideoMaxNumRefFrame, 1);
}

unsafe fn open_inner(gpu: &GpuContext, cfg: EncoderConfig) -> windows::core::Result<Box<dyn HardwareEncoder>> {
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

    let (transform, name) = find_hevc_hw_encoder()?;
    unlock_async(&transform)?;
    transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

    let want444 = cfg.prefer_444;
    let mut chroma = Chroma::Yuv420;
    if want444
        && set_hevc_output(
            &transform,
            cfg.width,
            cfg.height,
            cfg.bitrate_bps,
            eAVEncH265VProfile_Main_444_8.0 as u32,
        )
        .is_ok()
        && input_has(&transform, MFVideoFormat_AYUV)
    {
        chroma = Chroma::Yuv444;
    } else {
        if want444 {
            warn!("Intel HEVC 4:4:4 hardware encode unavailable; using 4:2:0");
        }
        set_hevc_output(
            &transform,
            cfg.width,
            cfg.height,
            cfg.bitrate_bps,
            eAVEncH265VProfile_Main_420_8.0 as u32,
        )?;
    }

    let mut input = if chroma == Chroma::Yuv444 {
        if set_input_subtype(&transform, cfg.width, cfg.height, MFVideoFormat_AYUV).is_ok() {
            InputFmt::Ayuv
        } else {
            return Err(windows::core::Error::from(E_FAIL));
        }
    } else if set_input_subtype(&transform, cfg.width, cfg.height, MFVideoFormat_NV12).is_ok() {
        InputFmt::Nv12
    } else if set_input_subtype(&transform, cfg.width, cfg.height, MFVideoFormat_ARGB32).is_ok() {
        InputFmt::Argb32
    } else {
        warn!(%name, "hardware HEVC encoder accepted neither NV12 nor ARGB32 input");
        return Err(windows::core::Error::from(E_FAIL));
    };

    let csc = match input {
        InputFmt::Nv12 => match ColorConvert::try_new(&gpu.device, &gpu.context, cfg.width, cfg.height, DXGI_FORMAT_NV12) {
            Ok(c) => Some(c),
            Err(e) => {
                warn!("BGRA→NV12 video processor failed ({e}); trying ARGB32 DXGI input");
                set_input_subtype(&transform, cfg.width, cfg.height, MFVideoFormat_ARGB32)?;
                input = InputFmt::Argb32;
                None
            }
        },
        InputFmt::Ayuv => Some(ColorConvert::try_new(
            &gpu.device,
            &gpu.context,
            cfg.width,
            cfg.height,
            DXGI_FORMAT_AYUV,
        )?),
        InputFmt::Argb32 => None,
    };

    let codec = transform.cast::<ICodecAPI>().ok();
    if let Some(c) = codec.as_ref() {
        configure_codec(c, cfg.bitrate_bps);
    }

    transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
    transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

    let events: IMFMediaEventGenerator = transform.cast()?;
    let provides = transform.GetOutputStreamInfo(0)?.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

    info!(
        %name,
        ?chroma,
        ?input,
        width = cfg.width,
        height = cfg.height,
        provides,
        "Intel MF hardware HEVC encoder ready"
    );

    let mut enc = MfHevcEncoder {
        chroma,
        _input: input,
        device: gpu.device.clone(),
        _context: gpu.context.clone(),
        _manager: manager,
        transform,
        events,
        codec,
        csc,
        provides_samples: provides,
        input_credits: 0,
        output_ready: 0,
        sample_clock: 0,
        bitrate_bps: cfg.bitrate_bps,
    };
    enc.pump(Duration::from_millis(50))?;
    Ok(Box::new(enc))
}

struct ColorConvert {
    video: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    dst: ID3D11Texture2D,
}

impl ColorConvert {
    fn try_new(
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Self> {
        unsafe {
            let video: ID3D11VideoDevice = device.cast()?;
            let vctx: ID3D11VideoContext = ctx.cast()?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: width.max(1),
                InputHeight: height.max(1),
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: width.max(1),
                OutputHeight: height.max(1),
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video.CreateVideoProcessor(&enumerator, 0)?;
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let mut td = D3D11_TEXTURE2D_DESC::default();
            td.Width = width.max(1);
            td.Height = height.max(1);
            td.MipLevels = 1;
            td.ArraySize = 1;
            td.Format = format;
            td.SampleDesc.Count = 1;
            td.Usage = D3D11_USAGE_DEFAULT;
            td.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
            let mut dst = None;
            if device.CreateTexture2D(&td, None, Some(&mut dst)).is_err() {
                td.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | windows::Win32::Graphics::Direct3D11::D3D11_BIND_VIDEO_ENCODER.0) as u32;
                dst = None;
                device.CreateTexture2D(&td, None, Some(&mut dst))?;
            }
            Ok(Self {
                video,
                vctx,
                enumerator,
                processor,
                dst: dst.ok_or_else(|| windows::core::Error::from(E_FAIL))?,
            })
        }
    }

    fn convert(&self, src: &ID3D11Texture2D) -> windows::core::Result<ID3D11Texture2D> {
        unsafe {
            let mut idesc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC::default();
            idesc.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
            idesc.Anonymous.Texture2D = D3D11_TEX2D_VPIV {
                MipSlice: 0,
                ArraySlice: 0,
            };
            let src_res: ID3D11Resource = src.cast()?;
            let mut iview = None;
            self.video
                .CreateVideoProcessorInputView(&src_res, &self.enumerator, &idesc, Some(&mut iview))?;
            let mut odesc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC::default();
            odesc.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
            odesc.Anonymous.Texture2D = D3D11_TEX2D_VPOV { MipSlice: 0 };
            let dst_res: ID3D11Resource = self.dst.cast()?;
            let mut oview = None;
            self.video
                .CreateVideoProcessorOutputView(&dst_res, &self.enumerator, &odesc, Some(&mut oview))?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM::default();
            stream.Enable = true.into();
            stream.pInputSurface = ManuallyDrop::new(iview);
            self.vctx.VideoProcessorBlt(&self.processor, &oview.unwrap(), 0, std::slice::from_ref(&stream))?;
            let _ = ManuallyDrop::take(&mut stream.pInputSurface);
            let _ = ManuallyDrop::take(&mut stream.pInputSurfaceRight);
            Ok(self.dst.clone())
        }
    }
}

struct MfHevcEncoder {
    chroma: Chroma,
    _input: InputFmt,
    #[allow(dead_code)]
    device: ID3D11Device,
    _context: ID3D11DeviceContext,
    _manager: IMFDXGIDeviceManager,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    csc: Option<ColorConvert>,
    provides_samples: bool,
    input_credits: u32,
    output_ready: u32,
    sample_clock: i64,
    bitrate_bps: u32,
}

unsafe impl Send for MfHevcEncoder {}

impl MfHevcEncoder {
    unsafe fn handle_event(&mut self, event: &windows::Win32::Media::MediaFoundation::IMFMediaEvent) -> windows::core::Result<()> {
        let ty = event.GetType()?;
        if ty == METransformNeedInput.0 as u32 {
            self.input_credits = self.input_credits.saturating_add(1);
        } else if ty == METransformHaveOutput.0 as u32 {
            self.output_ready = self.output_ready.saturating_add(1);
        } else if ty == MEError.0 as u32 {
            let st = event.GetStatus().unwrap_or(E_FAIL);
            return Err(windows::core::Error::from(st));
        }
        Ok(())
    }

    unsafe fn pump_nowait(&mut self) -> windows::core::Result<()> {
        loop {
            match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(ev) => self.handle_event(&ev)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    unsafe fn pump(&mut self, budget: Duration) -> windows::core::Result<()> {
        self.pump_nowait()?;
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(ev) => self.handle_event(&ev)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    unsafe fn wait_credit(&mut self, want_output: bool) -> windows::core::Result<bool> {
        let deadline = Instant::now() + Duration::from_millis(750);
        while Instant::now() < deadline {
            self.pump_nowait()?;
            if want_output && self.output_ready > 0 {
                return Ok(true);
            }
            if !want_output && self.input_credits > 0 {
                return Ok(true);
            }
            match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(ev) => self.handle_event(&ev)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    unsafe fn wrap_texture(&self, tex: &ID3D11Texture2D) -> windows::core::Result<IMFSample> {
        let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buf)?;
        Ok(sample)
    }

    unsafe fn make_output_sample(&self) -> windows::core::Result<IMFSample> {
        let info = self.transform.GetOutputStreamInfo(0)?;
        let sample = MFCreateSample()?;
        let buf = MFCreateMemoryBuffer(info.cbSize.max(64 * 1024))?;
        sample.AddBuffer(&buf)?;
        Ok(sample)
    }

    unsafe fn take_output(&mut self) -> windows::core::Result<Option<EncodedAu>> {
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
        let hr = self
            .transform
            .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status);
        match hr {
            Ok(()) => {
                let sample = ManuallyDrop::take(&mut out.pSample);
                let _ = ManuallyDrop::take(&mut out.pEvents);
                match sample {
                    Some(sample) => self.sample_to_au(&sample).map(Some),
                    None => Ok(None),
                }
            }
            Err(e) => {
                let _ = ManuallyDrop::take(&mut out.pSample);
                let _ = ManuallyDrop::take(&mut out.pEvents);
                Err(e)
            }
        }
    }

    unsafe fn sample_to_au(&self, sample: &IMFSample) -> windows::core::Result<EncodedAu> {
        let buf: IMFMediaBuffer = sample.ConvertToContiguousBuffer()?;
        let mut max = 0u32;
        let mut current = 0u32;
        let mut ptr = std::ptr::null_mut();
        buf.Lock(&mut ptr, Some(&mut max), Some(&mut current))?;
        let annexb = if ptr.is_null() || current == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(ptr, current as usize).to_vec()
        };
        buf.Unlock()?;
        let is_keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        Ok(EncodedAu {
            annexb,
            is_keyframe,
            chroma: self.chroma,
            backend: EncodeBackend::Qsv,
        })
    }

    unsafe fn encode_inner(&mut self, frame: &GpuFrame, force_idr: bool) -> windows::core::Result<Option<EncodedAu>> {
        let src = frame
            .d3d11_texture()
            .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        let tex = if let Some(csc) = self.csc.as_ref() {
            csc.convert(src)?
        } else {
            src.clone()
        };

        if !self.wait_credit(false)? {
            warn!("Intel encoder: timed out waiting for METransformNeedInput");
            return Ok(None);
        }

        if force_idr {
            if let Some(c) = self.codec.as_ref() {
                let _ = set_codec_u32(c, &CODECAPI_AVEncVideoForceKeyFrame, 1);
            }
        }

        let sample = self.wrap_texture(&tex)?;
        self.sample_clock += 166_667;
        sample.SetSampleTime(self.sample_clock)?;
        sample.SetSampleDuration(166_667)?;
        if force_idr {
            let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
        }
        self.transform.ProcessInput(0, &sample, 0)?;
        self.input_credits = self.input_credits.saturating_sub(1);

        if !self.wait_credit(true)? {
            // Low-latency encoders usually emit on the first input; a one-frame delay is still OK.
            return Ok(None);
        }
        self.output_ready = self.output_ready.saturating_sub(1);
        let au = self.take_output()?;
        if let Some(mut au) = au {
            if force_idr {
                au.is_keyframe = true;
            }
            if au.annexb.is_empty() {
                return Ok(None);
            }
            return Ok(Some(au));
        }
        Ok(None)
    }
}

impl HardwareEncoder for MfHevcEncoder {
    fn encode(&mut self, frame: &GpuFrame, force_idr: bool) -> Result<Option<EncodedAu>> {
        unsafe { self.encode_inner(frame, force_idr) }.map_err(|e| EncodeError::Message(e.to_string()))
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate_bps = bps;
        if let Some(c) = self.codec.as_ref() {
            let _ = unsafe { set_codec_u32(c, &CODECAPI_AVEncCommonMeanBitRate, bps.max(1_000_000)) };
        }
    }

    fn backend(&self) -> EncodeBackend {
        EncodeBackend::Qsv
    }

    fn chroma(&self) -> Chroma {
        self.chroma
    }
}

impl Drop for MfHevcEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}
