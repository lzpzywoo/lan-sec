//! Media Foundation HEVC decoder with D3D11 surfaces, then Video Processor CSC to BGRA.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use lansec_capture::GpuContext;
use lansec_protocol::{Chroma, CodecCap, DecodeBackend};
use tracing::{info, warn};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL};
use windows::Win32::Media::MediaFoundation::{
    eAVEncH265VProfile_Main_420_8, eAVEncH265VProfile_Main_444_8, IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager,
    IMFMediaBuffer, IMFMediaEventGenerator, IMFSample, IMFTransform, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFTEnumEx, MFStartup, CODECAPI_AVLowLatencyMode, MEError,
    METransformHaveOutput, METransformNeedInput, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_INPUT_STREAM_INFO, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFMediaType_Video, MFVideoFormat_ARGB32,
    MFVideoFormat_AYUV, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFSTARTUP_FULL,
    MF_E_BUFFERTOOSMALL, MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

use crate::{DecodeError, DecodedFrame, DecodedInner, HardwareDecoder, Result};

#[link(name = "lansec_mfx")]
unsafe extern "C" {
    fn lansec_mfx_probe_hevc444(device: *mut std::ffi::c_void) -> i32;
    fn lansec_mfx_open(device: *mut std::ffi::c_void, width: u32, height: u32) -> *mut std::ffi::c_void;
    fn lansec_mfx_close(dec: *mut std::ffi::c_void);
    fn lansec_mfx_decode(
        dec: *mut std::ffi::c_void,
        annexb: *const u8,
        len: i32,
        out_tex: *mut *mut std::ffi::c_void,
    ) -> i32;
}

#[link(name = "lansec_hevc_dxva")]
unsafe extern "C" {
    fn lansec_hevc_dxva_probe(device: *mut std::ffi::c_void) -> i32;
    fn lansec_hevc_dxva_open(
        device: *mut std::ffi::c_void,
        ctx: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> *mut std::ffi::c_void;
    fn lansec_hevc_dxva_close(dec: *mut std::ffi::c_void);
    fn lansec_hevc_dxva_decode(
        dec: *mut std::ffi::c_void,
        annexb: *const u8,
        len: i32,
        out_tex: *mut *mut std::ffi::c_void,
    ) -> i32;
}

/// DXVA HEVC Main 4:4:4 8-bit (Moonlight / Chromium).
const HEVC_VLD_MAIN_444: GUID = GUID {
    data1: 0x4008_018f,
    data2: 0xf537,
    data3: 0x4b36,
    data4: [0x98, 0xcf, 0x61, 0xaf, 0x8a, 0x2c, 0x1a, 0x33],
};
/// Intel private HEVC Main 4:4:4 VLD device.
const HEVC_VLD_MAIN_444_INTEL: GUID = GUID {
    data1: 0x41a5_af96,
    data2: 0xe415,
    data3: 0x4b0c,
    data4: [0x9d, 0x03, 0x90, 0x78, 0x58, 0xe2, 0x3e, 0x78],
};

pub fn probe() -> Vec<CodecCap> {
    let hevc_mft = unsafe { hevc_decoder_available() };
    if hevc_mft {
        info!("Media Foundation HEVC decoder MFT registered");
    } else {
        warn!("HEVC decoder MFT not registered; install HEVC Video Extensions for Windows client decode");
    }
    let probe444 = unsafe { probe_hevc444() };
    match &probe444 {
        Ok(p) => {
            info!(
                d3d11 = p.d3d11,
                guid = p.guid_label.as_str(),
                mft = p.mft_name.as_str(),
                mft444 = p.mft_444,
                mfx = p.mfx,
                dxva = p.dxva,
                vp_ayuv = p.vp_ayuv,
                "hevc444_d3d11 probe"
            );
            println!(
                "hevc444_d3d11={} guid={} mft={} mft444={} mfx={} dxva={} vp_ayuv={}",
                p.d3d11, p.guid_label, p.mft_name, p.mft_444, p.mfx, p.dxva, p.vp_ayuv
            );
        }
        Err(e) => {
            warn!("hevc444_d3d11 probe failed: {e}");
            println!("hevc444_d3d11=false error={e}");
        }
    }
    if !hevc_mft {
        return Vec::new();
    }
    let mut caps = vec![CodecCap::decode(DecodeBackend::D3d11va, Chroma::Yuv420, 3840, 2160)];
    if probe444.as_ref().is_ok_and(|p| p.ok()) {
        caps.push(CodecCap::decode(DecodeBackend::D3d11va, Chroma::Yuv444, 3840, 2160));
    }
    caps
}

struct Hevc444Probe {
    d3d11: bool,
    guid_label: String,
    mft_name: String,
    mft_444: bool,
    mfx: bool,
    dxva: bool,
    vp_ayuv: bool,
}

impl Hevc444Probe {
    fn ok(&self) -> bool {
        self.d3d11 && self.vp_ayuv && (self.mft_444 || self.mfx || self.dxva)
    }
}

unsafe fn hevc_decoder_available() -> bool {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    // Many Intel systems only expose an async HW HEVC MFT (no Sync software MFT).
    find_hevc_mft().is_ok() || find_hevc_hw_decoder().is_ok()
}

unsafe fn probe_hevc444() -> windows::core::Result<Hevc444Probe> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    let gpu = GpuContext::new().map_err(|_| windows::core::Error::from(E_FAIL))?;
    let video: ID3D11VideoDevice = gpu.device.cast()?;
    let (d3d11, guid_label) = check_d3d11_hevc444(&video)?;
    let vp_ayuv = video_processor_accepts_ayuv(&gpu.device, &gpu.context);
    let (mft_name, mft_444) = match find_hevc_hw_decoder() {
        Ok((transform, name)) => {
            let _ = unlock_async(&transform);
            let ok = decoder_accepts_444(&transform);
            info!(%name, ok, "hardware HEVC decoder MFT 4:4:4 types");
            (name, ok)
        }
        Err(_) => {
            info!("no Intel/hardware HEVC decoder MFT (Microsoft sync MFT is 4:2:0 only)");
            ("none".into(), false)
        }
    };
    let mfx = lansec_mfx_probe_hevc444(gpu.device.as_raw()) != 0;
    let dxva = lansec_hevc_dxva_probe(gpu.device.as_raw()) != 0;
    info!(mfx, dxva, "Intel HEVC 4:4:4 decode backends");
    Ok(Hevc444Probe {
        d3d11,
        guid_label,
        mft_name,
        mft_444,
        mfx,
        dxva,
        vp_ayuv,
    })
}

fn guid_eq(a: GUID, b: GUID) -> bool {
    a == b
}

unsafe fn check_d3d11_hevc444(video: &ID3D11VideoDevice) -> windows::core::Result<(bool, String)> {
    let n = video.GetVideoDecoderProfileCount();
    for i in 0..n {
        let profile = video.GetVideoDecoderProfile(i)?;
        if decoder_format_ok(video, &profile, DXGI_FORMAT_AYUV) {
            if guid_eq(profile, HEVC_VLD_MAIN_444) {
                return Ok((true, "HEVC_VLD_MAIN_444".into()));
            }
            if guid_eq(profile, HEVC_VLD_MAIN_444_INTEL) {
                return Ok((true, "HEVC_VLD_MAIN_444_Intel".into()));
            }
        }
    }
    if decoder_format_ok(video, &HEVC_VLD_MAIN_444, DXGI_FORMAT_AYUV) {
        return Ok((true, "HEVC_VLD_MAIN_444".into()));
    }
    if decoder_format_ok(video, &HEVC_VLD_MAIN_444_INTEL, DXGI_FORMAT_AYUV) {
        return Ok((true, "HEVC_VLD_MAIN_444_Intel".into()));
    }
    Ok((false, "none".into()))
}

unsafe fn decoder_format_ok(video: &ID3D11VideoDevice, profile: &GUID, format: DXGI_FORMAT) -> bool {
    matches!(video.CheckVideoDecoderFormat(profile, format), Ok(v) if v.as_bool())
}

unsafe fn video_processor_accepts_ayuv(device: &ID3D11Device, _ctx: &ID3D11DeviceContext) -> bool {
    let Ok(video) = device.cast::<ID3D11VideoDevice>() else {
        return false;
    };
    let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        InputWidth: 1920,
        InputHeight: 1080,
        OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        OutputWidth: 1920,
        OutputHeight: 1080,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    let Ok(enumerator) = (unsafe { video.CreateVideoProcessorEnumerator(&desc) }) else {
        return false;
    };
    let Ok(flags) = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_AYUV) else {
        return false;
    };
    let input = flags & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 != 0;
    let Ok(out_flags) = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) else {
        return false;
    };
    let output = out_flags & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 != 0;
    input && output
}

unsafe fn decoder_accepts_444(transform: &IMFTransform) -> bool {
    let input = match MFCreateMediaType() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let _ = input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
    let _ = input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC);
    let _ = input.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_444_8.0 as u32);
    let _ = input.SetUINT64(&MF_MT_FRAME_SIZE, pack_wh(1920, 1080));
    let _ = input.SetUINT64(&MF_MT_FRAME_RATE, pack_wh(60, 1));
    let _ = input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
    if transform.SetInputType(0, &input, 0).is_err() {
        return false;
    }
    output_has(transform, MFVideoFormat_AYUV) || output_has(transform, MFVideoFormat_ARGB32)
}

unsafe fn output_has(transform: &IMFTransform, want: GUID) -> bool {
    for i in 0..32u32 {
        let Ok(ty) = transform.GetOutputAvailableType(0, i) else {
            break;
        };
        if ty.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default() == want {
            return true;
        }
    }
    false
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

unsafe fn unlock_async(transform: &IMFTransform) -> windows::core::Result<()> {
    let attrs = transform.GetAttributes()?;
    let _ = attrs.SetUINT32(&MF_SA_D3D11_AWARE, 1);
    let _ = attrs.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
    if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
        attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
    }
    Ok(())
}

unsafe fn find_hevc_hw_decoder() -> windows::core::Result<(IMFTransform, String)> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_HEVC,
    };
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        flags,
        Some(&input),
        None,
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
        let name = allocated_string(&act, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "hardware HEVC decoder".into());
        let vendor = allocated_string(&act, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute).unwrap_or_default();
        info!(%name, %vendor, "hardware HEVC decoder MFT");
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
            }
        }
    }
    CoTaskMemFree(Some(activates as *const _));
    intel.or(any).ok_or_else(|| windows::core::Error::from(E_FAIL))
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
    // 4:4:4: prefer HW-MFT; fall back to DXVA/MFX + VP CSC if MFT open fails.
    // 4:2:0: try Sync MFT first, then the same HW-MFT path (Intel often has no Sync HEVC).
    if chroma == Chroma::Yuv444 {
        match open_mf_hevc(gpu, chroma, width, height) {
            Ok(dec) => return Ok(dec),
            Err(e) => warn!("HW-MFT HEVC 4:4:4 open failed ({e}); trying D3D11VA/MFX"),
        }
        if let Some(dec) = open_dxva(gpu, width, height) {
            return Ok(dec);
        }
        if let Some(dec) = open_mfx(gpu, width, height) {
            return Ok(dec);
        }
        return Err(windows::core::Error::from(E_FAIL));
    }
    match open_mf_hevc(gpu, chroma, width, height) {
        Ok(dec) => return Ok(dec),
        Err(e) => warn!("HEVC 4:2:0 MFT open failed ({e}); trying D3D11VA/MFX"),
    }
    if let Some(dec) = open_dxva(gpu, width, height) {
        return Ok(dec);
    }
    if let Some(dec) = open_mfx(gpu, width, height) {
        return Ok(dec);
    }
    Err(windows::core::Error::from(E_FAIL))
}

unsafe fn open_mf_hevc(
    gpu: &GpuContext,
    chroma: Chroma,
    width: u32,
    height: u32,
) -> windows::core::Result<Box<dyn HardwareDecoder>> {
    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
    let manager = manager.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    manager.ResetDevice(&gpu.device, reset_token)?;

    let (transform, name, async_hw) = if chroma == Chroma::Yuv444 {
        let (t, name) = find_hevc_hw_decoder()?;
        unlock_async(&t)?;
        (t, name, true)
    } else if let Ok(t) = find_hevc_mft() {
        if let Ok(attrs) = t.GetAttributes() {
            let _ = attrs.SetUINT32(&MF_SA_D3D11_AWARE, 1);
            let _ = attrs.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
        }
        (t, "Media Foundation HEVC decoder".into(), false)
    } else {
        // Windows 11 + Intel UHD often ships only an async hardware HEVC MFT.
        let (t, name) = find_hevc_hw_decoder()?;
        unlock_async(&t)?;
        info!(%name, "HEVC 4:2:0 using hardware MFT (no Sync MFT)");
        (t, name, true)
    };
    transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

    let input = MFCreateMediaType()?;
    input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
    let profile = if chroma == Chroma::Yuv444 {
        eAVEncH265VProfile_Main_444_8.0 as u32
    } else {
        eAVEncH265VProfile_Main_420_8.0 as u32
    };
    input.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
    input.SetUINT64(&MF_MT_FRAME_SIZE, pack_wh(width.max(1), height.max(1)))?;
    input.SetUINT64(&MF_MT_FRAME_RATE, pack_wh(60, 1))?;
    input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    transform.SetInputType(0, &input, 0)?;

    // 444: prefer AYUV + Video Processor CSC. Intel HW-MFT "ARGB32" for Main444 often
    // still carries YUV-ordered bytes in a BGRA-typed surface → solid green on present.
    let prefer: &[GUID] = if chroma == Chroma::Yuv444 {
        &[MFVideoFormat_AYUV, MFVideoFormat_ARGB32]
    } else {
        &[MFVideoFormat_NV12, MFVideoFormat_AYUV]
    };
    let out_label = set_output_type(&transform, width, height, prefer, chroma != Chroma::Yuv444)?;

    transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
    transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

    let provides = transform.GetOutputStreamInfo(0)?.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
    let mut in_info = MFT_INPUT_STREAM_INFO::default();
    transform.GetInputStreamInfo(0, &mut in_info)?;
    let events = if async_hw {
        Some(transform.cast::<IMFMediaEventGenerator>()?)
    } else {
        None
    };
    let csc = VideoCsc::try_new(&gpu.device, &gpu.context, width, height).ok();
    info!(
        %name,
        ?chroma,
        width,
        height,
        provides,
        async_hw,
        output = out_label,
        csc = csc.is_some(),
        input_cb = in_info.cbSize,
        input_align = in_info.cbAlignment,
        "MF HEVC decoder (D3D11) ready"
    );
    println!(
        "hevc-decode backend=HW-MFT name={name} chroma={chroma:?} output={out_label} csc={}",
        csc.is_some()
    );

    Ok(Box::new(MfDecoder {
        width,
        height,
        chroma,
        gpu: GpuHolder {
            device: gpu.device.clone(),
            context: gpu.context.clone(),
        },
        _manager: manager,
        transform,
        events,
        input_credits: 0,
        output_ready: 0,
        provides_samples: provides,
        min_input: in_info.cbSize,
        input_align: in_info.cbAlignment,
        origin: Instant::now(),
        csc,
        sample_clock: 0,
        diag_frames: 0,
    }))
}

unsafe fn set_output_type(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    prefer: &[GUID],
    fallback: bool,
) -> windows::core::Result<&'static str> {
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
                return Ok(mf_subtype_label(*want));
            }
        }
    }
    if fallback {
        if let Ok(ty) = transform.GetOutputAvailableType(0, 0) {
            transform.SetOutputType(0, &ty, 0)?;
            let sub = ty.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
            return Ok(mf_subtype_label(sub));
        }
    }
    Err(windows::core::Error::from(E_FAIL))
}

fn mf_subtype_label(g: GUID) -> &'static str {
    if g == MFVideoFormat_ARGB32 {
        "ARGB32"
    } else if g == MFVideoFormat_AYUV {
        "AYUV"
    } else if g == MFVideoFormat_NV12 {
        "NV12"
    } else {
        "other"
    }
}

fn dxgi_format_label(f: DXGI_FORMAT) -> &'static str {
    match f {
        DXGI_FORMAT_B8G8R8A8_UNORM => "BGRA8",
        DXGI_FORMAT_AYUV => "AYUV",
        _ => "other",
    }
}

/// Read a few BGRA samples from a texture to detect all-0 / all-255 (white screen).
unsafe fn probe_bgra_nonzero(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
) -> Option<(u8, u8, u8, u8, bool)> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM || desc.Width == 0 || desc.Height == 0 {
        return None;
    }
    let mut sd = D3D11_TEXTURE2D_DESC::default();
    sd.Width = 1;
    sd.Height = 1;
    sd.MipLevels = 1;
    sd.ArraySize = 1;
    sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    sd.SampleDesc.Count = 1;
    sd.Usage = D3D11_USAGE_STAGING;
    sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let mut staging = None;
    device.CreateTexture2D(&sd, None, Some(&mut staging)).ok()?;
    let staging = staging?;
    let src: ID3D11Resource = tex.cast().ok()?;
    let dst: ID3D11Resource = staging.cast().ok()?;
    // Center pixel — desktop content almost never pure white/black there if decode works.
    let x = desc.Width / 2;
    let y = desc.Height / 2;
    ctx.CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, Some(&windows::Win32::Graphics::Direct3D11::D3D11_BOX {
        left: x,
        top: y,
        front: 0,
        right: x + 1,
        bottom: y + 1,
        back: 1,
    }));
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
    let p = mapped.pData as *const u8;
    let (b, g, r, a) = if p.is_null() {
        (0, 0, 0, 0)
    } else {
        (*p, *p.add(1), *p.add(2), *p.add(3))
    };
    ctx.Unmap(&dst, 0);
    let empty = (b == 0 && g == 0 && r == 0) || (b == 255 && g == 255 && r == 255);
    Some((b, g, r, a, !empty))
}

struct GpuHolder {
    device: ID3D11Device,
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
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            // Desktop capture is full-range. Nominal_Range values:
            // 0=undefined, 1=16–235 (studio), 2=0–255 (full). Using 1 was wrong and
            // paired badly with AYUV→BGRA; ARGB32-as-YUV also reads as green (Y in G).
            let stream_bits: u32 = (1 << 2) | (2 << 4); // BT.709 + full 0–255
            let stream_cs: D3D11_VIDEO_PROCESSOR_COLOR_SPACE = std::mem::transmute(stream_bits);
            vctx.VideoProcessorSetStreamColorSpace(&processor, 0, &stream_cs);
            let out_bits: u32 = 2 << 4; // full-range RGB
            let out_cs: D3D11_VIDEO_PROCESSOR_COLOR_SPACE = std::mem::transmute(out_bits);
            vctx.VideoProcessorSetOutputColorSpace(&processor, &out_cs);
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

fn open_dxva(gpu: &GpuContext, width: u32, height: u32) -> Option<Box<dyn HardwareDecoder>> {
    let ptr = unsafe {
        lansec_hevc_dxva_open(
            gpu.device.as_raw(),
            gpu.context.as_raw(),
            width.max(1),
            height.max(1),
        )
    };
    if ptr.is_null() {
        return None;
    }
    info!(width, height, "D3D11VA HEVC 4:4:4 decoder ready");
    let csc = VideoCsc::try_new(&gpu.device, &gpu.context, width.max(1), height.max(1)).ok();
    println!(
        "hevc-decode backend=DXVA chroma=Yuv444 output=AYUV csc={}",
        csc.is_some()
    );
    Some(Box::new(DxvaDecoder {
        ptr,
        width: width.max(1),
        height: height.max(1),
        origin: Instant::now(),
        csc,
        device: gpu.device.clone(),
        context: gpu.context.clone(),
        diag_frames: 0,
    }))
}

struct DxvaDecoder {
    ptr: *mut std::ffi::c_void,
    width: u32,
    height: u32,
    origin: Instant,
    csc: Option<VideoCsc>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    diag_frames: u32,
}

unsafe impl Send for DxvaDecoder {}

impl Drop for DxvaDecoder {
    fn drop(&mut self) {
        unsafe { lansec_hevc_dxva_close(self.ptr) };
    }
}

impl HardwareDecoder for DxvaDecoder {
    fn backend(&self) -> DecodeBackend {
        DecodeBackend::D3d11va
    }

    fn decode(&mut self, annexb: &[u8], _is_keyframe: bool) -> Result<Option<DecodedFrame>> {
        if annexb.is_empty() {
            return Ok(None);
        }
        let mut raw = std::ptr::null_mut();
        let rc = unsafe {
            lansec_hevc_dxva_decode(self.ptr, annexb.as_ptr(), annexb.len() as i32, &mut raw)
        };
        if rc < 0 {
            return Err(DecodeError::Message(format!("dxva hevc444 {rc}")));
        }
        if rc == 0 || raw.is_null() {
            return Ok(None);
        }
        let tex = unsafe { ID3D11Texture2D::from_raw(raw as *mut _) };
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut src_desc) };
        let tex = if let Some(csc) = self.csc.as_ref() {
            match csc.convert(&tex, 0) {
                Ok(bgra) => bgra,
                Err(e) => {
                    warn!("video processor CSC failed: {e}");
                    return Ok(None);
                }
            }
        } else {
            warn!("DXVA 444 decode has no Video Processor CSC");
            return Ok(None);
        };
        if self.diag_frames < 3 {
            self.diag_frames += 1;
            let mut out_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut out_desc) };
            let probe = unsafe { probe_bgra_nonzero(&self.device, &self.context, &tex) };
            info!(
                frame = self.diag_frames,
                src = dxgi_format_label(src_desc.Format),
                out = dxgi_format_label(out_desc.Format),
                probe = ?probe,
                "444 DXVA frame diag"
            );
            println!(
                "444-dxva diag frame={} src={} out={} probe={:?}",
                self.diag_frames,
                dxgi_format_label(src_desc.Format),
                dxgi_format_label(out_desc.Format),
                probe
            );
        }
        Ok(Some(DecodedFrame {
            width: self.width,
            height: self.height,
            decode_done_us: self.origin.elapsed().as_micros() as u64,
            inner: DecodedInner::D3d11(tex),
        }))
    }
}

fn open_mfx(gpu: &GpuContext, width: u32, height: u32) -> Option<Box<dyn HardwareDecoder>> {
    let ptr = unsafe { lansec_mfx_open(gpu.device.as_raw(), width.max(1), height.max(1)) };
    if ptr.is_null() {
        return None;
    }
    info!(width, height, "Intel MFX HEVC 4:4:4 decoder ready");
    let csc = VideoCsc::try_new(&gpu.device, &gpu.context, width.max(1), height.max(1)).ok();
    println!(
        "hevc-decode backend=MFX chroma=Yuv444 output=AYUV csc={}",
        csc.is_some()
    );
    Some(Box::new(MfxDecoder {
        ptr,
        width: width.max(1),
        height: height.max(1),
        origin: Instant::now(),
        csc,
        device: gpu.device.clone(),
        context: gpu.context.clone(),
        diag_frames: 0,
    }))
}

struct MfxDecoder {
    ptr: *mut std::ffi::c_void,
    width: u32,
    height: u32,
    origin: Instant,
    csc: Option<VideoCsc>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    diag_frames: u32,
}

unsafe impl Send for MfxDecoder {}

impl Drop for MfxDecoder {
    fn drop(&mut self) {
        unsafe { lansec_mfx_close(self.ptr) };
    }
}

impl HardwareDecoder for MfxDecoder {
    fn backend(&self) -> DecodeBackend {
        DecodeBackend::D3d11va
    }

    fn decode(&mut self, annexb: &[u8], _is_keyframe: bool) -> Result<Option<DecodedFrame>> {
        if annexb.is_empty() {
            return Ok(None);
        }
        let mut raw = std::ptr::null_mut();
        let rc = unsafe {
            lansec_mfx_decode(
                self.ptr,
                annexb.as_ptr(),
                annexb.len() as i32,
                &mut raw,
            )
        };
        if rc < 0 {
            return Err(DecodeError::Message(format!("mfx decode {rc}")));
        }
        if rc == 0 || raw.is_null() {
            return Ok(None);
        }
        let tex = unsafe { ID3D11Texture2D::from_raw(raw as *mut _) };
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut src_desc) };
        let tex = if let Some(csc) = self.csc.as_ref() {
            match csc.convert(&tex, 0) {
                Ok(bgra) => bgra,
                Err(e) => {
                    warn!("video processor CSC failed: {e}");
                    return Ok(None);
                }
            }
        } else {
            warn!("MFX 444 decode has no Video Processor CSC");
            return Ok(None);
        };
        if self.diag_frames < 3 {
            self.diag_frames += 1;
            let mut out_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut out_desc) };
            let probe = unsafe { probe_bgra_nonzero(&self.device, &self.context, &tex) };
            info!(
                frame = self.diag_frames,
                src = dxgi_format_label(src_desc.Format),
                out = dxgi_format_label(out_desc.Format),
                probe = ?probe,
                "444 MFX frame diag"
            );
            println!(
                "444-mfx diag frame={} src={} out={} probe={:?}",
                self.diag_frames,
                dxgi_format_label(src_desc.Format),
                dxgi_format_label(out_desc.Format),
                probe
            );
        }
        Ok(Some(DecodedFrame {
            width: self.width,
            height: self.height,
            decode_done_us: self.origin.elapsed().as_micros() as u64,
            inner: DecodedInner::D3d11(tex),
        }))
    }
}

struct MfDecoder {
    width: u32,
    height: u32,
    chroma: Chroma,
    gpu: GpuHolder,
    _manager: IMFDXGIDeviceManager,
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    input_credits: u32,
    output_ready: u32,
    provides_samples: bool,
    min_input: u32,
    input_align: u32,
    origin: Instant,
    csc: Option<VideoCsc>,
    sample_clock: i64,
    diag_frames: u32,
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
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        loop {
            match events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(ev) => self.handle_event(&ev)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    unsafe fn wait_credit(&mut self, want_output: bool) -> windows::core::Result<bool> {
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            self.pump_nowait()?;
            if want_output && self.output_ready > 0 {
                return Ok(true);
            }
            if !want_output && self.input_credits > 0 {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(false)
    }

    unsafe fn feed(&mut self, annexb: &[u8]) -> windows::core::Result<Option<DecodedFrame>> {
        let sample = self.make_input_sample(annexb)?;
        self.sample_clock += 10_000;
        sample.SetSampleTime(self.sample_clock)?;
        sample.SetSampleDuration(10_000)?;
        if self.events.is_some() {
            if !self.wait_credit(false)? {
                warn!("hardware HEVC decoder: timed out waiting for METransformNeedInput");
                return Ok(None);
            }
            self.transform.ProcessInput(0, &sample, 0)?;
            self.input_credits = self.input_credits.saturating_sub(1);
            if !self.wait_credit(true)? {
                return Ok(None);
            }
            self.output_ready = self.output_ready.saturating_sub(1);
            return self.drain();
        }
        self.transform.ProcessInput(0, &sample, 0)?;
        self.drain()
    }

    /// MFT HEVC decoders rewrite Annex B in place and require GetInputStreamInfo.cbSize
    /// (and spare tail bytes). A buffer sized exactly to the AU returns MF_E_BUFFERTOOSMALL.
    unsafe fn make_input_sample(&self, annexb: &[u8]) -> windows::core::Result<IMFSample> {
        let align = self.input_align.max(1);
        let min = (annexb.len() as u32)
            .max(self.min_input)
            .saturating_add(1024)
            .max(1);
        let size = min.saturating_add(align - 1) / align * align;
        let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(size)?;
        let mut max = 0u32;
        let mut current = 0u32;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, Some(&mut max), Some(&mut current))?;
        let copy = annexb.len().min(max as usize);
        std::ptr::copy_nonoverlapping(annexb.as_ptr(), ptr, copy);
        buffer.Unlock()?;
        buffer.SetCurrentLength(copy as u32)?;
        let sample: IMFSample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }

    unsafe fn drain(&mut self) -> windows::core::Result<Option<DecodedFrame>> {
        let mut too_small = 0u8;
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
                    let prefer: &[GUID] = if self.chroma == Chroma::Yuv444 {
                        &[MFVideoFormat_AYUV, MFVideoFormat_ARGB32]
                    } else {
                        &[MFVideoFormat_NV12, MFVideoFormat_AYUV]
                    };
                    let label = set_output_type(
                        &self.transform,
                        self.width,
                        self.height,
                        prefer,
                        self.chroma != Chroma::Yuv444,
                    )?;
                    info!(output = label, "MF stream change renegotiated output");
                    let info = self.transform.GetOutputStreamInfo(0)?;
                    self.provides_samples =
                        info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                    continue;
                }
                Err(e) if e.code() == MF_E_BUFFERTOOSMALL => {
                    let _ = ManuallyDrop::take(&mut out.pSample);
                    let _ = ManuallyDrop::take(&mut out.pEvents);
                    too_small += 1;
                    if too_small > 4 {
                        return Err(e);
                    }
                    self.provides_samples = false;
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
        let fallback = self.width.saturating_mul(self.height).saturating_mul(4);
        let sample = MFCreateSample()?;
        let buf = MFCreateMemoryBuffer(info.cbSize.max(fallback).max(1))?;
        sample.AddBuffer(&buf)?;
        Ok(sample)
    }

    unsafe fn sample_to_frame(&mut self, sample: &IMFSample) -> windows::core::Result<Option<DecodedFrame>> {
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
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                let tex = if desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM {
                    // Intel Main444 sometimes labels YUV bytes as BGRA → green present.
                    // Probe once; if center looks like Y-in-G (high G, near-zero R/B), run CSC from AYUV instead is impossible — drop frame.
                    if self.chroma == Chroma::Yuv444 {
                        if let Some((b, g, r, _a, ok)) =
                            probe_bgra_nonzero(&self.gpu.device, &self.gpu.context, &tex)
                        {
                            let yuv_as_rgb = ok && g > 40 && r < 20 && b < 20;
                            if yuv_as_rgb {
                                warn!(
                                    b, g, r,
                                    "444 BGRA looks like YUV-as-RGB (green); dropping frame"
                                );
                                return Ok(None);
                            }
                        }
                    }
                    tex
                } else if let Some(csc) = self.csc.as_ref() {
                    match csc.convert(&tex, slice) {
                        Ok(bgra) => bgra,
                        Err(e) => {
                            warn!("video processor CSC failed: {e}");
                            // Never present AYUV/NV12 as BGRA — that shows solid green.
                            return Ok(None);
                        }
                    }
                } else {
                    warn!(
                        format = dxgi_format_label(desc.Format),
                        "no CSC for non-BGRA decode output"
                    );
                    return Ok(None);
                };
                if self.chroma == Chroma::Yuv444 && self.diag_frames < 3 {
                    self.diag_frames += 1;
                    let mut out_desc = D3D11_TEXTURE2D_DESC::default();
                    tex.GetDesc(&mut out_desc);
                    let probe = probe_bgra_nonzero(&self.gpu.device, &self.gpu.context, &tex);
                    info!(
                        frame = self.diag_frames,
                        src = dxgi_format_label(desc.Format),
                        out = dxgi_format_label(out_desc.Format),
                        probe = ?probe,
                        "444 MF frame diag"
                    );
                    println!(
                        "444-mf diag frame={} src={} out={} probe={:?}",
                        self.diag_frames,
                        dxgi_format_label(desc.Format),
                        dxgi_format_label(out_desc.Format),
                        probe
                    );
                }
                tex
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
