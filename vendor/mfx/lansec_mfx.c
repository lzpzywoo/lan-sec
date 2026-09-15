#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lansec_mfx.h"
#include "mfxvideo.h"

#define MAX_SURF 32
#define BS_CAP (4 * 1024 * 1024)

typedef mfxStatus(MFX_CDECL *fn_MFXInit)(mfxIMPL, mfxVersion *, mfxSession *);
typedef mfxStatus(MFX_CDECL *fn_MFXClose)(mfxSession);
typedef mfxStatus(MFX_CDECL *fn_SetHandle)(mfxSession, mfxHandleType, mfxHDL);
typedef mfxStatus(MFX_CDECL *fn_SetFrameAllocator)(mfxSession, mfxFrameAllocator *);
typedef mfxStatus(MFX_CDECL *fn_Sync)(mfxSession, mfxSyncPoint, mfxU32);
typedef mfxStatus(MFX_CDECL *fn_DecQuery)(mfxSession, mfxVideoParam *, mfxVideoParam *);
typedef mfxStatus(MFX_CDECL *fn_DecHeader)(mfxSession, mfxBitstream *, mfxVideoParam *);
typedef mfxStatus(MFX_CDECL *fn_DecIOSurf)(mfxSession, mfxVideoParam *, mfxFrameAllocRequest *);
typedef mfxStatus(MFX_CDECL *fn_DecInit)(mfxSession, mfxVideoParam *);
typedef mfxStatus(MFX_CDECL *fn_DecClose)(mfxSession);
typedef mfxStatus(MFX_CDECL *fn_DecAsync)(mfxSession, mfxBitstream *, mfxFrameSurface1 *, mfxFrameSurface1 **, mfxSyncPoint *);

typedef struct {
    fn_MFXInit Init;
    fn_MFXClose Close;
    fn_SetHandle SetHandle;
    fn_SetFrameAllocator SetAlloc;
    fn_Sync Sync;
    fn_DecQuery Query;
    fn_DecHeader Header;
    fn_DecIOSurf IOSurf;
    fn_DecInit DecInit;
    fn_DecClose DecClose;
    fn_DecAsync DecAsync;
} MfxApi;

typedef struct {
    ID3D11Device *device;
    ID3D11Texture2D *tex[MAX_SURF];
    mfxMemId mids[MAX_SURF];
    mfxU16 n;
} Alloc;

typedef struct {
    HMODULE dll;
    MfxApi api;
    mfxSession session;
    mfxFrameAllocator allocator;
    Alloc pool;
    mfxFrameSurface1 surf[MAX_SURF];
    mfxVideoParam par;
    mfxBitstream bs;
    uint8_t *bs_buf;
    int inited;
    uint32_t width;
    uint32_t height;
} MfxDec;

static HMODULE try_load(const wchar_t *path) {
    return LoadLibraryExW(path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
}

static HMODULE load_mfx_dll(void) {
    HMODULE h = LoadLibraryW(L"libmfxhw64.dll");
    if (h) return h;
    h = LoadLibraryW(L"libmfx64-gen.dll");
    if (h) return h;
    WIN32_FIND_DATAW fd;
    HANDLE find = FindFirstFileW(L"C:\\Windows\\System32\\DriverStore\\FileRepository\\*", &fd);
    if (find == INVALID_HANDLE_VALUE) return NULL;
    do {
        if (!(fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY)) continue;
        if (fd.cFileName[0] == L'.') continue;
        wchar_t path[MAX_PATH];
        _snwprintf_s(path, MAX_PATH, _TRUNCATE,
                     L"C:\\Windows\\System32\\DriverStore\\FileRepository\\%s\\libmfxhw64.dll",
                     fd.cFileName);
        h = try_load(path);
        if (h) {
            FindClose(find);
            return h;
        }
        _snwprintf_s(path, MAX_PATH, _TRUNCATE,
                     L"C:\\Windows\\System32\\DriverStore\\FileRepository\\%s\\libmfx64-gen.dll",
                     fd.cFileName);
        h = try_load(path);
        if (h) {
            FindClose(find);
            return h;
        }
    } while (FindNextFileW(find, &fd));
    FindClose(find);
    return NULL;
}

static int fill_api(HMODULE dll, MfxApi *api) {
    api->Init = (fn_MFXInit)GetProcAddress(dll, "MFXInit");
    api->Close = (fn_MFXClose)GetProcAddress(dll, "MFXClose");
    api->SetHandle = (fn_SetHandle)GetProcAddress(dll, "MFXVideoCORE_SetHandle");
    api->SetAlloc = (fn_SetFrameAllocator)GetProcAddress(dll, "MFXVideoCORE_SetFrameAllocator");
    api->Sync = (fn_Sync)GetProcAddress(dll, "MFXVideoCORE_SyncOperation");
    api->Query = (fn_DecQuery)GetProcAddress(dll, "MFXVideoDECODE_Query");
    api->Header = (fn_DecHeader)GetProcAddress(dll, "MFXVideoDECODE_DecodeHeader");
    api->IOSurf = (fn_DecIOSurf)GetProcAddress(dll, "MFXVideoDECODE_QueryIOSurf");
    api->DecInit = (fn_DecInit)GetProcAddress(dll, "MFXVideoDECODE_Init");
    api->DecClose = (fn_DecClose)GetProcAddress(dll, "MFXVideoDECODE_Close");
    api->DecAsync = (fn_DecAsync)GetProcAddress(dll, "MFXVideoDECODE_DecodeFrameAsync");
    return api->Init && api->Close && api->SetHandle && api->SetAlloc && api->Sync && api->Query &&
           api->Header && api->IOSurf && api->DecInit && api->DecClose && api->DecAsync;
}

static mfxStatus MFX_CDECL alloc_frames(mfxHDL pthis, mfxFrameAllocRequest *req, mfxFrameAllocResponse *resp) {
    Alloc *a = (Alloc *)pthis;
    mfxU16 n = req->NumFrameSuggested;
    if (n < req->NumFrameMin) n = req->NumFrameMin;
    if (n > MAX_SURF) n = MAX_SURF;
    if (n == 0) n = 8;
    DXGI_FORMAT fmt = DXGI_FORMAT_AYUV;
    if (req->Info.FourCC == MFX_FOURCC_NV12) fmt = DXGI_FORMAT_NV12;
    D3D11_TEXTURE2D_DESC td;
    memset(&td, 0, sizeof(td));
    td.Width = req->Info.Width ? req->Info.Width : 1920;
    td.Height = req->Info.Height ? req->Info.Height : 1088;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = fmt;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_DECODER | D3D11_BIND_SHADER_RESOURCE;
    memset(resp, 0, sizeof(*resp));
    for (mfxU16 i = 0; i < n; i++) {
        ID3D11Texture2D *tex = NULL;
        HRESULT hr = ID3D11Device_CreateTexture2D(a->device, &td, NULL, &tex);
        if (FAILED(hr) || !tex) {
            for (mfxU16 j = 0; j < i; j++) ID3D11Texture2D_Release(a->tex[j]);
            a->n = 0;
            return MFX_ERR_MEMORY_ALLOC;
        }
        a->tex[i] = tex;
        a->mids[i] = tex;
    }
    a->n = n;
    resp->mids = a->mids;
    resp->NumFrameActual = n;
    return MFX_ERR_NONE;
}

static mfxStatus MFX_CDECL lock_frames(mfxHDL pthis, mfxMemId mid, mfxFrameData *ptr) {
    (void)pthis;
    (void)mid;
    (void)ptr;
    return MFX_ERR_NONE;
}

static mfxStatus MFX_CDECL unlock_frames(mfxHDL pthis, mfxMemId mid, mfxFrameData *ptr) {
    (void)pthis;
    (void)mid;
    (void)ptr;
    return MFX_ERR_NONE;
}

static mfxStatus MFX_CDECL get_hdl(mfxHDL pthis, mfxMemId mid, mfxHDL *handle) {
    (void)pthis;
    if (!handle || !mid) return MFX_ERR_NULL_PTR;
    *handle = mid;
    return MFX_ERR_NONE;
}

static mfxStatus MFX_CDECL free_frames(mfxHDL pthis, mfxFrameAllocResponse *resp) {
    Alloc *a = (Alloc *)pthis;
    (void)resp;
    for (mfxU16 i = 0; i < a->n; i++) {
        if (a->tex[i]) ID3D11Texture2D_Release(a->tex[i]);
        a->tex[i] = NULL;
        a->mids[i] = NULL;
    }
    a->n = 0;
    return MFX_ERR_NONE;
}

static void fill_hevc444_param(mfxVideoParam *par, mfxU16 w, mfxU16 h) {
    memset(par, 0, sizeof(*par));
    par->mfx.CodecId = MFX_CODEC_HEVC;
    par->mfx.CodecProfile = MFX_PROFILE_HEVC_REXT;
    par->mfx.FrameInfo.FourCC = MFX_FOURCC_AYUV;
    par->mfx.FrameInfo.ChromaFormat = MFX_CHROMAFORMAT_YUV444;
    par->mfx.FrameInfo.Width = (mfxU16)((w + 31) & ~31);
    par->mfx.FrameInfo.Height = (mfxU16)((h + 31) & ~31);
    par->mfx.FrameInfo.CropW = w;
    par->mfx.FrameInfo.CropH = h;
    par->mfx.FrameInfo.BitDepthLuma = 8;
    par->mfx.FrameInfo.BitDepthChroma = 8;
    par->mfx.FrameInfo.Shift = 0;
    par->mfx.FrameInfo.PicStruct = MFX_PICSTRUCT_PROGRESSIVE;
    par->IOPattern = MFX_IOPATTERN_OUT_VIDEO_MEMORY;
    par->AsyncDepth = 1;
}

static int session_query_444(MfxApi *api, mfxSession s, void *d3d_device) {
    if (d3d_device) {
        mfxStatus st = api->SetHandle(s, MFX_HANDLE_D3D11_DEVICE, d3d_device);
        if (st < MFX_ERR_NONE) return 0;
    }
    mfxVideoParam in, out;
    fill_hevc444_param(&in, 1920, 1080);
    memset(&out, 0, sizeof(out));
    mfxStatus st = api->Query(s, &in, &out);
    return st >= MFX_ERR_NONE && out.mfx.CodecId == MFX_CODEC_HEVC;
}

int lansec_mfx_probe_hevc444(void *d3d_device) {
    HMODULE dll = load_mfx_dll();
    if (!dll) return 0;
    MfxApi api;
    if (!fill_api(dll, &api)) {
        FreeLibrary(dll);
        return 0;
    }
    mfxVersion ver = {0};
    ver.Major = 1;
    ver.Minor = 0;
    mfxSession s = NULL;
    mfxIMPL impl = MFX_IMPL_HARDWARE_ANY | MFX_IMPL_VIA_D3D11;
    if (api.Init(impl, &ver, &s) < MFX_ERR_NONE || !s) {
        FreeLibrary(dll);
        return 0;
    }
    int ok = session_query_444(&api, s, d3d_device);
    api.Close(s);
    FreeLibrary(dll);
    return ok;
}

static void reset_bs(MfxDec *d) {
    d->bs.Data = d->bs_buf;
    d->bs.MaxLength = BS_CAP;
    d->bs.DataOffset = 0;
    d->bs.DataLength = 0;
}

void *lansec_mfx_open(void *d3d_device, uint32_t width, uint32_t height) {
    if (!d3d_device || width == 0 || height == 0) return NULL;
    MfxDec *d = (MfxDec *)calloc(1, sizeof(MfxDec));
    if (!d) return NULL;
    d->dll = load_mfx_dll();
    if (!d->dll || !fill_api(d->dll, &d->api)) {
        lansec_mfx_close(d);
        return NULL;
    }
    mfxVersion ver = {0};
    ver.Major = 1;
    ver.Minor = 0;
    mfxIMPL impl = MFX_IMPL_HARDWARE_ANY | MFX_IMPL_VIA_D3D11;
    if (d->api.Init(impl, &ver, &d->session) < MFX_ERR_NONE || !d->session) {
        lansec_mfx_close(d);
        return NULL;
    }
    if (d->api.SetHandle(d->session, MFX_HANDLE_D3D11_DEVICE, d3d_device) < MFX_ERR_NONE) {
        lansec_mfx_close(d);
        return NULL;
    }
    d->pool.device = (ID3D11Device *)d3d_device;
    ID3D11Device_AddRef(d->pool.device);
    d->allocator.pthis = &d->pool;
    d->allocator.Alloc = alloc_frames;
    d->allocator.Lock = lock_frames;
    d->allocator.Unlock = unlock_frames;
    d->allocator.GetHDL = get_hdl;
    d->allocator.Free = free_frames;
    if (d->api.SetAlloc(d->session, &d->allocator) < MFX_ERR_NONE) {
        lansec_mfx_close(d);
        return NULL;
    }
    d->width = width;
    d->height = height;
    fill_hevc444_param(&d->par, (mfxU16)width, (mfxU16)height);
    d->bs_buf = (uint8_t *)malloc(BS_CAP);
    if (!d->bs_buf) {
        lansec_mfx_close(d);
        return NULL;
    }
    reset_bs(d);
    return d;
}

void lansec_mfx_close(void *dec) {
    MfxDec *d = (MfxDec *)dec;
    if (!d) return;
    if (d->inited && d->session) d->api.DecClose(d->session);
    if (d->session) d->api.Close(d->session);
    free_frames(&d->pool, NULL);
    if (d->pool.device) ID3D11Device_Release(d->pool.device);
    free(d->bs_buf);
    if (d->dll) FreeLibrary(d->dll);
    free(d);
}

static int ensure_init(MfxDec *d) {
    if (d->inited) return 1;
    mfxStatus st = d->api.Header(d->session, &d->bs, &d->par);
    if (st == MFX_ERR_MORE_DATA) return 0;
    if (st < MFX_ERR_NONE) return 0;
    d->par.IOPattern = MFX_IOPATTERN_OUT_VIDEO_MEMORY;
    d->par.AsyncDepth = 1;
    if (d->par.mfx.FrameInfo.ChromaFormat != MFX_CHROMAFORMAT_YUV444) {
        return 0;
    }
    d->par.mfx.FrameInfo.FourCC = MFX_FOURCC_AYUV;
    if (d->api.DecInit(d->session, &d->par) < MFX_ERR_NONE) return 0;
    if (d->pool.n == 0) return 0;
    for (mfxU16 i = 0; i < d->pool.n; i++) {
        memset(&d->surf[i], 0, sizeof(d->surf[i]));
        d->surf[i].Info = d->par.mfx.FrameInfo;
        d->surf[i].Data.MemId = d->pool.mids[i];
    }
    d->inited = 1;
    return 1;
}

static mfxFrameSurface1 *free_surf(MfxDec *d) {
    for (mfxU16 i = 0; i < d->pool.n; i++) {
        if (d->surf[i].Data.Locked == 0) return &d->surf[i];
    }
    return NULL;
}

int lansec_mfx_decode(void *dec, const uint8_t *annexb, int len, void **out_tex) {
    MfxDec *d = (MfxDec *)dec;
    if (!d || !out_tex || !annexb || len <= 0) return -1;
    *out_tex = NULL;
    if (d->bs.DataOffset + d->bs.DataLength + (mfxU32)len > d->bs.MaxLength) {
        if (d->bs.DataLength) memmove(d->bs_buf, d->bs_buf + d->bs.DataOffset, d->bs.DataLength);
        d->bs.DataOffset = 0;
        d->bs.Data = d->bs_buf;
    }
    if (d->bs.DataOffset + d->bs.DataLength + (mfxU32)len > d->bs.MaxLength) return -1;
    memcpy(d->bs_buf + d->bs.DataOffset + d->bs.DataLength, annexb, (size_t)len);
    d->bs.DataLength += (mfxU32)len;
    d->bs.DataFlag = MFX_BITSTREAM_COMPLETE_FRAME;
    if (!ensure_init(d)) return 0;
    for (int tries = 0; tries < 8; tries++) {
        mfxFrameSurface1 *work = free_surf(d);
        if (!work) return 0;
        mfxFrameSurface1 *out = NULL;
        mfxSyncPoint sync = NULL;
        mfxStatus st = d->api.DecAsync(d->session, &d->bs, work, &out, &sync);
        if (st == MFX_ERR_MORE_DATA) return 0;
        if (st == MFX_ERR_MORE_SURFACE) continue;
        if (st == MFX_WRN_DEVICE_BUSY) {
            Sleep(1);
            continue;
        }
        if (st < MFX_ERR_NONE) return -1;
        if (!sync || !out) return 0;
        st = d->api.Sync(d->session, sync, 100);
        if (st < MFX_ERR_NONE) return -1;
        ID3D11Texture2D *tex = (ID3D11Texture2D *)out->Data.MemId;
        if (!tex) return -1;
        ID3D11Texture2D_AddRef(tex);
        *out_tex = tex;
        if (d->bs.DataLength == 0) d->bs.DataOffset = 0;
        return 1;
    }
    return 0;
}
