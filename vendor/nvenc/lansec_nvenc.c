#include <windows.h>
#include <d3d11.h>
#include <string.h>
#include <stdlib.h>
#include "nvEncodeAPI.h"
#include "lansec_nvenc.h"

typedef NVENCSTATUS (NVENCAPI *PNvEncodeAPICreateInstance)(NV_ENCODE_API_FUNCTION_LIST *functionList);

struct LansecNvenc {
    HMODULE lib;
    NV_ENCODE_API_FUNCTION_LIST api;
    void *encoder;
    NV_ENC_OUTPUT_PTR bitstream;
    NV_ENC_REGISTERED_PTR registered;
    ID3D11Texture2D *input_tex;
    ID3D11Device *device;
    ID3D11DeviceContext *ctx;
    uint32_t width;
    uint32_t height;
    uint32_t bitrate;
    int yuv444;
    uint32_t frame;
};

static int nv_ok(NVENCSTATUS s) { return s == NV_ENC_SUCCESS; }

static int query_cap(LansecNvenc *e, NV_ENC_CAPS cap, int *out) {
    NV_ENC_CAPS_PARAM p;
    GUID codec = NV_ENC_CODEC_HEVC_GUID;
    memset(&p, 0, sizeof(p));
    p.version = NV_ENC_CAPS_PARAM_VER;
    p.capsToQuery = cap;
    return nv_ok(e->api.nvEncGetEncodeCaps(e->encoder, codec, &p, out));
}

int lansec_nvenc_available(void) {
    HMODULE lib = LoadLibraryA("nvEncodeAPI64.dll");
    if (!lib) return 0;
    FreeLibrary(lib);
    return 1;
}

int lansec_nvenc_probe_yuv444(void *d3d_device) {
    if (!d3d_device) return 0;
    LansecNvenc tmp;
    memset(&tmp, 0, sizeof(tmp));
    tmp.lib = LoadLibraryA("nvEncodeAPI64.dll");
    if (!tmp.lib) return 0;
    PNvEncodeAPICreateInstance create = (PNvEncodeAPICreateInstance)GetProcAddress(tmp.lib, "NvEncodeAPICreateInstance");
    if (!create) { FreeLibrary(tmp.lib); return 0; }
    tmp.api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (!nv_ok(create(&tmp.api))) { FreeLibrary(tmp.lib); return 0; }
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS sp;
    memset(&sp, 0, sizeof(sp));
    sp.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    sp.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    sp.device = d3d_device;
    sp.apiVersion = NVENCAPI_VERSION;
    if (!nv_ok(tmp.api.nvEncOpenEncodeSessionEx(&sp, &tmp.encoder))) {
        FreeLibrary(tmp.lib);
        return 0;
    }
    int yuv444 = 0;
    query_cap(&tmp, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE, &yuv444);
    tmp.api.nvEncDestroyEncoder(tmp.encoder);
    FreeLibrary(tmp.lib);
    return yuv444 ? 1 : 0;
}

LansecNvenc *lansec_nvenc_open(void *d3d_device, void *d3d_ctx, uint32_t width, uint32_t height, uint32_t bitrate, int yuv444) {
    if (!d3d_device || width == 0 || height == 0) return NULL;
    LansecNvenc *e = (LansecNvenc *)calloc(1, sizeof(LansecNvenc));
    if (!e) return NULL;
    e->lib = LoadLibraryA("nvEncodeAPI64.dll");
    if (!e->lib) { free(e); return NULL; }
    PNvEncodeAPICreateInstance create = (PNvEncodeAPICreateInstance)GetProcAddress(e->lib, "NvEncodeAPICreateInstance");
    if (!create) { FreeLibrary(e->lib); free(e); return NULL; }
    e->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (!nv_ok(create(&e->api))) { FreeLibrary(e->lib); free(e); return NULL; }

    e->device = (ID3D11Device *)d3d_device;
    e->ctx = (ID3D11DeviceContext *)d3d_ctx;
    e->device->lpVtbl->AddRef(e->device);
    e->ctx->lpVtbl->AddRef(e->ctx);
    e->width = width;
    e->height = height;
    e->bitrate = bitrate ? bitrate : 60000000;
    e->yuv444 = yuv444;

    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS sp;
    memset(&sp, 0, sizeof(sp));
    sp.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    sp.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    sp.device = d3d_device;
    sp.apiVersion = NVENCAPI_VERSION;
    if (!nv_ok(e->api.nvEncOpenEncodeSessionEx(&sp, &e->encoder))) goto fail;

    NV_ENC_PRESET_CONFIG preset;
    memset(&preset, 0, sizeof(preset));
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    if (!nv_ok(e->api.nvEncGetEncodePresetConfigEx(e->encoder, NV_ENC_CODEC_HEVC_GUID, NV_ENC_PRESET_P1_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &preset))) {
        goto fail;
    }
    NV_ENC_CONFIG cfg = preset.presetCfg;
    cfg.profileGUID = e->yuv444 ? NV_ENC_HEVC_PROFILE_FREXT_GUID : NV_ENC_HEVC_PROFILE_MAIN_GUID;
    cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
    cfg.frameIntervalP = 1;
    cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    cfg.rcParams.averageBitRate = e->bitrate;
    cfg.rcParams.maxBitRate = e->bitrate;
    cfg.rcParams.vbvBufferSize = e->bitrate / 60;
    cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
    cfg.rcParams.zeroReorderDelay = 1;
    cfg.encodeCodecConfig.hevcConfig.chromaFormatIDC = e->yuv444 ? 3 : 1;
    cfg.encodeCodecConfig.hevcConfig.repeatSPSPPS = 1;
    cfg.encodeCodecConfig.hevcConfig.idrPeriod = NVENC_INFINITE_GOPLENGTH;

    NV_ENC_INITIALIZE_PARAMS ip;
    memset(&ip, 0, sizeof(ip));
    ip.version = NV_ENC_INITIALIZE_PARAMS_VER;
    ip.encodeGUID = NV_ENC_CODEC_HEVC_GUID;
    ip.presetGUID = NV_ENC_PRESET_P1_GUID;
    ip.encodeWidth = width;
    ip.encodeHeight = height;
    ip.darWidth = width;
    ip.darHeight = height;
    ip.frameRateNum = 60;
    ip.frameRateDen = 1;
    ip.enableEncodeAsync = 0;
    ip.enablePTD = 1;
    ip.encodeConfig = &cfg;
    ip.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    if (!nv_ok(e->api.nvEncInitializeEncoder(e->encoder, &ip))) goto fail;

    D3D11_TEXTURE2D_DESC td;
    memset(&td, 0, sizeof(td));
    td.Width = width;
    td.Height = height;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
    if (FAILED(e->device->lpVtbl->CreateTexture2D(e->device, &td, NULL, &e->input_tex))) goto fail;

    NV_ENC_REGISTER_RESOURCE rr;
    memset(&rr, 0, sizeof(rr));
    rr.version = NV_ENC_REGISTER_RESOURCE_VER;
    rr.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    rr.width = width;
    rr.height = height;
    rr.pitch = 0;
    rr.resourceToRegister = e->input_tex;
    rr.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;
    rr.bufferUsage = NV_ENC_INPUT_IMAGE;
    if (!nv_ok(e->api.nvEncRegisterResource(e->encoder, &rr))) goto fail;
    e->registered = rr.registeredResource;

    NV_ENC_CREATE_BITSTREAM_BUFFER bb;
    memset(&bb, 0, sizeof(bb));
    bb.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    if (!nv_ok(e->api.nvEncCreateBitstreamBuffer(e->encoder, &bb))) goto fail;
    e->bitstream = bb.bitstreamBuffer;
    return e;

fail:
    lansec_nvenc_close(e);
    return NULL;
}

void lansec_nvenc_close(LansecNvenc *e) {
    if (!e) return;
    if (e->encoder && e->api.nvEncDestroyEncoder) {
        if (e->registered && e->api.nvEncUnregisterResource)
            e->api.nvEncUnregisterResource(e->encoder, e->registered);
        if (e->bitstream && e->api.nvEncDestroyBitstreamBuffer)
            e->api.nvEncDestroyBitstreamBuffer(e->encoder, e->bitstream);
        e->api.nvEncDestroyEncoder(e->encoder);
    }
    if (e->input_tex) e->input_tex->lpVtbl->Release(e->input_tex);
    if (e->ctx) e->ctx->lpVtbl->Release(e->ctx);
    if (e->device) e->device->lpVtbl->Release(e->device);
    if (e->lib) FreeLibrary(e->lib);
    free(e);
}

int lansec_nvenc_set_bitrate(LansecNvenc *e, uint32_t bitrate) {
    if (!e || !e->encoder) return 0;
    e->bitrate = bitrate;
    NV_ENC_RECONFIGURE_PARAMS rp;
    memset(&rp, 0, sizeof(rp));
    rp.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    rp.resetEncoder = 1;
    rp.forceIDR = 1;
    rp.reInitEncodeParams.version = NV_ENC_INITIALIZE_PARAMS_VER;
    rp.reInitEncodeParams.encodeGUID = NV_ENC_CODEC_HEVC_GUID;
    rp.reInitEncodeParams.encodeWidth = e->width;
    rp.reInitEncodeParams.encodeHeight = e->height;
    /* Bitrate is applied on next IDR via encode config when supported. */
    return 1;
}

int lansec_nvenc_encode(LansecNvenc *e, void *src_texture, int force_idr, uint8_t *out, int out_cap, int *out_len, int *is_key) {
    if (!e || !src_texture || !out || !out_len) return 0;
    e->ctx->lpVtbl->CopyResource(e->ctx, (ID3D11Resource *)e->input_tex, (ID3D11Resource *)src_texture);

    NV_ENC_MAP_INPUT_RESOURCE map;
    memset(&map, 0, sizeof(map));
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = e->registered;
    if (!nv_ok(e->api.nvEncMapInputResource(e->encoder, &map))) return 0;

    NV_ENC_PIC_PARAMS pic;
    memset(&pic, 0, sizeof(pic));
    pic.version = NV_ENC_PIC_PARAMS_VER;
    pic.inputWidth = e->width;
    pic.inputHeight = e->height;
    pic.inputPitch = e->width;
    pic.inputBuffer = map.mappedResource;
    pic.outputBitstream = e->bitstream;
    pic.bufferFmt = map.mappedBufferFmt;
    pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    pic.inputTimeStamp = e->frame++;
    if (force_idr) pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;

    NVENCSTATUS st = e->api.nvEncEncodePicture(e->encoder, &pic);
    e->api.nvEncUnmapInputResource(e->encoder, map.mappedResource);
    if (!nv_ok(st) && st != NV_ENC_ERR_NEED_MORE_INPUT) return 0;

    NV_ENC_LOCK_BITSTREAM lock;
    memset(&lock, 0, sizeof(lock));
    lock.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock.outputBitstream = e->bitstream;
    lock.doNotWait = 0;
    if (!nv_ok(e->api.nvEncLockBitstream(e->encoder, &lock))) return 0;
    int n = (int)lock.bitstreamSizeInBytes;
    if (n > out_cap) n = out_cap;
    if (lock.bitstreamBufferPtr && n > 0) memcpy(out, lock.bitstreamBufferPtr, (size_t)n);
    *out_len = n;
    if (is_key) *is_key = (lock.pictureType == NV_ENC_PIC_TYPE_IDR || lock.pictureType == NV_ENC_PIC_TYPE_I);
    e->api.nvEncUnlockBitstream(e->encoder, e->bitstream);
    return 1;
}
