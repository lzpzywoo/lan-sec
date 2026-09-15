#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#define INITGUID
#include <windows.h>
#include <d3d11.h>
#include <dxva.h>
#include <stdlib.h>
#include <string.h>
#include "lansec_hevc_dxva.h"

static const GUID kHevc444 = {0x4008018f, 0xf537, 0x4b36, {0x98, 0xcf, 0x61, 0xaf, 0x8a, 0x2c, 0x1a, 0x33}};
static const GUID kHevc444Intel = {0x41a5af96, 0xe415, 0x4b0c, {0x9d, 0x03, 0x90, 0x78, 0x58, 0xe2, 0x3e, 0x78}};

#define DPB 16
#define MAX_NAL 64

typedef struct {
    const uint8_t *p;
    size_t n, i;
    int bits;
    uint32_t acc;
    int err;
} Br;

typedef struct {
    int chroma_format_idc;
    int separate_colour_plane_flag;
    int bit_depth_luma_minus8;
    int bit_depth_chroma_minus8;
    int log2_max_poc_lsb_minus4;
    int sps_max_dec_pic_buffering_minus1;
    int log2_min_cb_minus3;
    int log2_diff_max_min_cb;
    int log2_min_tb_minus2;
    int log2_diff_max_min_tb;
    int max_tr_hier_inter;
    int max_tr_hier_intra;
    int num_short_term_ref_pic_sets;
    int amp_enabled_flag;
    int sao_enabled_flag;
    int pcm_enabled_flag;
    int scaling_list_enabled_flag;
    int long_term_ref_pics_present_flag;
    int sps_temporal_mvp_enabled_flag;
    int strong_intra_smoothing_enabled_flag;
    int pic_width;
    int pic_height;
    int max_sub_layers_minus1;
} Sps;

typedef struct {
    int num_ref_idx_l0_default_active_minus1;
    int num_ref_idx_l1_default_active_minus1;
    int init_qp_minus26;
    int dependent_slice_segments_enabled_flag;
    int output_flag_present_flag;
    int num_extra_slice_header_bits;
    int sign_data_hiding_enabled_flag;
    int cabac_init_present_flag;
    int constrained_intra_pred_flag;
    int transform_skip_enabled_flag;
    int cu_qp_delta_enabled_flag;
    int pps_slice_chroma_qp_offsets_present_flag;
    int weighted_pred_flag;
    int weighted_bipred_flag;
    int transquant_bypass_enabled_flag;
    int tiles_enabled_flag;
    int entropy_coding_sync_enabled_flag;
    int pps_loop_filter_across_slices_enabled_flag;
    int deblocking_filter_override_enabled_flag;
    int pps_deblocking_filter_disabled_flag;
    int lists_modification_present_flag;
    int slice_segment_header_extension_present_flag;
    int pps_cb_qp_offset;
    int pps_cr_qp_offset;
    int diff_cu_qp_delta_depth;
    int log2_parallel_merge_level_minus2;
} Pps;

typedef struct {
    ID3D11Device *device;
    ID3D11DeviceContext *ctx;
    ID3D11VideoDevice *vdev;
    ID3D11VideoContext *vctx;
    ID3D11VideoDecoder *decoder;
    GUID profile;
    D3D11_VIDEO_DECODER_CONFIG config;
    ID3D11Texture2D *tex[DPB];
    ID3D11VideoDecoderOutputView *view[DPB];
    int poc[DPB];
    int used[DPB];
    int have_sps;
    int have_pps;
    Sps sps;
    Pps pps;
    uint32_t w, h;
    int next_slot;
    int last_slot;
    int last_poc;
} DxvaDec;

static void br_init(Br *b, const uint8_t *p, size_t n) {
    memset(b, 0, sizeof(*b));
    b->p = p;
    b->n = n;
}

static int br_get(Br *b) {
    if (b->err) return 0;
    if (b->bits == 0) {
        if (b->i >= b->n) {
            b->err = 1;
            return 0;
        }
        /* skip emulation prevention 0x000003 */
        if (b->i >= 2 && b->p[b->i] == 0x03 && b->p[b->i - 1] == 0 && b->p[b->i - 2] == 0) {
            b->i++;
            if (b->i >= b->n) {
                b->err = 1;
                return 0;
            }
        }
        b->acc = b->p[b->i++];
        b->bits = 8;
    }
    b->bits--;
    return (int)((b->acc >> b->bits) & 1);
}

static uint32_t br_u(Br *b, int n) {
    uint32_t v = 0;
    for (int i = 0; i < n; i++) v = (v << 1) | (uint32_t)br_get(b);
    return v;
}

static uint32_t br_ue(Br *b) {
    int z = 0;
    while (!br_get(b)) {
        z++;
        if (z > 31) {
            b->err = 1;
            return 0;
        }
    }
    return ((1u << z) - 1) + br_u(b, z);
}

static int32_t br_se(Br *b) {
    uint32_t u = br_ue(b);
    if (u & 1) return (int32_t)((u + 1) >> 1);
    return -(int32_t)(u >> 1);
}

static void skip_ptl(Br *b, int profile_present, int max_sub) {
    if (profile_present) {
        br_u(b, 2);
        br_u(b, 1);
        int idc = (int)br_u(b, 5);
        uint32_t compat = br_u(b, 32);
        br_u(b, 4);
        int rext = idc >= 4 && idc <= 11;
        if (!rext) {
            for (int i = 4; i <= 11; i++) if (compat & (1u << i)) rext = 1;
        }
        if (rext) {
            br_u(b, 8);
            br_u(b, 1);
            if (idc == 4 || idc == 5 || idc == 6 || idc == 7 || idc == 10 || (compat & 0x00000ff0)) {
                br_u(b, 1);
                br_u(b, 33);
            } else {
                br_u(b, 34);
            }
        } else if (idc == 2 || (compat & (1u << 2))) {
            br_u(b, 7);
            br_u(b, 35);
        } else {
            br_u(b, 43);
        }
        br_u(b, 1);
    }
    br_u(b, 8);
    int sub_profile[8] = {0};
    int sub_level[8] = {0};
    for (int i = 0; i < max_sub; i++) {
        sub_profile[i] = (int)br_u(b, 1);
        sub_level[i] = (int)br_u(b, 1);
    }
    if (max_sub > 0) br_u(b, 8 - max_sub);
    for (int i = 0; i < max_sub; i++) {
        if (sub_profile[i]) skip_ptl(b, 1, 0);
        else if (profile_present) {
            /* already consumed general; sub-layer profile block is skip_ptl with present */
        }
        if (sub_level[i]) br_u(b, 8);
    }
    (void)sub_profile;
}

static int parse_sps(const uint8_t *nal, size_t len, Sps *sps) {
    memset(sps, 0, sizeof(*sps));
    if (len < 3) return 0;
    Br b;
    br_init(&b, nal + 2, len - 2);
    br_u(&b, 4);
    sps->max_sub_layers_minus1 = (int)br_u(&b, 3);
    br_u(&b, 1);
    skip_ptl(&b, 1, sps->max_sub_layers_minus1);
    br_ue(&b);
    sps->chroma_format_idc = (int)br_ue(&b);
    if (sps->chroma_format_idc == 3) sps->separate_colour_plane_flag = (int)br_u(&b, 1);
    sps->pic_width = (int)br_ue(&b);
    sps->pic_height = (int)br_ue(&b);
    if (br_u(&b, 1)) {
        br_ue(&b);
        br_ue(&b);
        br_ue(&b);
        br_ue(&b);
    }
    sps->bit_depth_luma_minus8 = (int)br_ue(&b);
    sps->bit_depth_chroma_minus8 = (int)br_ue(&b);
    sps->log2_max_poc_lsb_minus4 = (int)br_ue(&b);
    int sub_ord = (int)br_u(&b, 1);
    int start = sub_ord ? 0 : sps->max_sub_layers_minus1;
    for (int i = start; i <= sps->max_sub_layers_minus1; i++) {
        sps->sps_max_dec_pic_buffering_minus1 = (int)br_ue(&b);
        br_ue(&b);
        br_ue(&b);
    }
    sps->log2_min_cb_minus3 = (int)br_ue(&b);
    sps->log2_diff_max_min_cb = (int)br_ue(&b);
    sps->log2_min_tb_minus2 = (int)br_ue(&b);
    sps->log2_diff_max_min_tb = (int)br_ue(&b);
    sps->max_tr_hier_inter = (int)br_ue(&b);
    sps->max_tr_hier_intra = (int)br_ue(&b);
    sps->scaling_list_enabled_flag = (int)br_u(&b, 1);
    if (sps->scaling_list_enabled_flag && br_u(&b, 1)) return 0;
    sps->amp_enabled_flag = (int)br_u(&b, 1);
    sps->sao_enabled_flag = (int)br_u(&b, 1);
    sps->pcm_enabled_flag = (int)br_u(&b, 1);
    if (sps->pcm_enabled_flag) return 0;
    sps->num_short_term_ref_pic_sets = (int)br_ue(&b);
    /* skip short-term RPS tables; slice will carry a set or index we approximate */
    (void)sps->num_short_term_ref_pic_sets;
    if (b.err) return 0;
    if (sps->chroma_format_idc != 3) return 0;
    return 1;
}

static int parse_pps(const uint8_t *nal, size_t len, Pps *pps) {
    memset(pps, 0, sizeof(*pps));
    if (len < 3) return 0;
    Br b;
    br_init(&b, nal + 2, len - 2);
    br_ue(&b);
    br_ue(&b);
    pps->dependent_slice_segments_enabled_flag = (int)br_u(&b, 1);
    pps->output_flag_present_flag = (int)br_u(&b, 1);
    pps->num_extra_slice_header_bits = (int)br_u(&b, 3);
    pps->sign_data_hiding_enabled_flag = (int)br_u(&b, 1);
    pps->cabac_init_present_flag = (int)br_u(&b, 1);
    pps->num_ref_idx_l0_default_active_minus1 = (int)br_ue(&b);
    pps->num_ref_idx_l1_default_active_minus1 = (int)br_ue(&b);
    pps->init_qp_minus26 = (int)br_se(&b);
    pps->constrained_intra_pred_flag = (int)br_u(&b, 1);
    pps->transform_skip_enabled_flag = (int)br_u(&b, 1);
    pps->cu_qp_delta_enabled_flag = (int)br_u(&b, 1);
    if (pps->cu_qp_delta_enabled_flag) pps->diff_cu_qp_delta_depth = (int)br_ue(&b);
    pps->pps_cb_qp_offset = (int)br_se(&b);
    pps->pps_cr_qp_offset = (int)br_se(&b);
    pps->pps_slice_chroma_qp_offsets_present_flag = (int)br_u(&b, 1);
    pps->weighted_pred_flag = (int)br_u(&b, 1);
    pps->weighted_bipred_flag = (int)br_u(&b, 1);
    pps->transquant_bypass_enabled_flag = (int)br_u(&b, 1);
    pps->tiles_enabled_flag = (int)br_u(&b, 1);
    pps->entropy_coding_sync_enabled_flag = (int)br_u(&b, 1);
    if (pps->tiles_enabled_flag) return 0;
    pps->pps_loop_filter_across_slices_enabled_flag = (int)br_u(&b, 1);
    int deblock = (int)br_u(&b, 1);
    if (deblock) {
        pps->deblocking_filter_override_enabled_flag = (int)br_u(&b, 1);
        pps->pps_deblocking_filter_disabled_flag = (int)br_u(&b, 1);
        if (!pps->pps_deblocking_filter_disabled_flag) {
            br_se(&b);
            br_se(&b);
        }
    }
    br_u(&b, 1); /* scaling list data present — skip if set */
    pps->lists_modification_present_flag = (int)br_u(&b, 1);
    pps->log2_parallel_merge_level_minus2 = (int)br_ue(&b);
    pps->slice_segment_header_extension_present_flag = (int)br_u(&b, 1);
    return !b.err;
}

static int nal_type(const uint8_t *nal, size_t len) {
    if (len < 2) return -1;
    return (nal[0] >> 1) & 0x3f;
}

static int next_nal(const uint8_t *data, int len, int *off, const uint8_t **nal, int *nlen) {
    int i = *off;
    while (i + 3 <= len) {
        if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1) {
            int start = i + 3;
            int j = start;
            while (j + 3 <= len) {
                if (data[j] == 0 && data[j + 1] == 0 && (data[j + 2] == 1 || (data[j + 2] == 0 && j + 4 <= len && data[j + 3] == 1))) break;
                j++;
            }
            if (j + 3 > len) j = len;
            /* 4-byte start code */
            if (start >= 4 && data[start - 4] == 0) {
                /* already accounted */
            }
            *nal = data + start;
            *nlen = j - start;
            *off = j;
            return 1;
        }
        if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && i + 4 <= len && data[i + 3] == 1) {
            int start = i + 4;
            int j = start;
            while (j + 3 <= len) {
                if (data[j] == 0 && data[j + 1] == 0 && (data[j + 2] == 1 || (data[j + 2] == 0 && j + 4 <= len && data[j + 3] == 1))) break;
                j++;
            }
            if (j + 3 > len) j = len;
            *nal = data + start;
            *nlen = j - start;
            *off = j;
            return 1;
        }
        i++;
    }
    return 0;
}

static int pick_profile(ID3D11VideoDevice *vdev, GUID *out) {
    BOOL ok = FALSE;
    if (SUCCEEDED(ID3D11VideoDevice_CheckVideoDecoderFormat(vdev, &kHevc444Intel, DXGI_FORMAT_AYUV, &ok)) && ok) {
        *out = kHevc444Intel;
        return 1;
    }
    ok = FALSE;
    if (SUCCEEDED(ID3D11VideoDevice_CheckVideoDecoderFormat(vdev, &kHevc444, DXGI_FORMAT_AYUV, &ok)) && ok) {
        *out = kHevc444;
        return 1;
    }
    return 0;
}

static int pick_config(ID3D11VideoDevice *vdev, const GUID *profile, UINT w, UINT h, D3D11_VIDEO_DECODER_CONFIG *cfg) {
    D3D11_VIDEO_DECODER_DESC desc;
    memset(&desc, 0, sizeof(desc));
    desc.Guid = *profile;
    desc.SampleWidth = w;
    desc.SampleHeight = h;
    desc.OutputFormat = DXGI_FORMAT_AYUV;
    UINT n = 0;
    if (FAILED(ID3D11VideoDevice_GetVideoDecoderConfigCount(vdev, &desc, &n)) || n == 0) return 0;
    for (UINT i = 0; i < n; i++) {
        D3D11_VIDEO_DECODER_CONFIG c;
        memset(&c, 0, sizeof(c));
        if (FAILED(ID3D11VideoDevice_GetVideoDecoderConfig(vdev, &desc, i, &c))) continue;
        if (c.ConfigBitstreamRaw == 1 || c.ConfigBitstreamRaw == 2) {
            *cfg = c;
            return 1;
        }
    }
    return SUCCEEDED(ID3D11VideoDevice_GetVideoDecoderConfig(vdev, &desc, 0, cfg));
}

static int create_decoder(ID3D11VideoDevice *vdev, UINT w, UINT h, GUID *profile, D3D11_VIDEO_DECODER_CONFIG *cfg, ID3D11VideoDecoder **dec) {
    if (!pick_profile(vdev, profile)) return 0;
    if (!pick_config(vdev, profile, w, h, cfg)) return 0;
    D3D11_VIDEO_DECODER_DESC desc;
    memset(&desc, 0, sizeof(desc));
    desc.Guid = *profile;
    desc.SampleWidth = w;
    desc.SampleHeight = h;
    desc.OutputFormat = DXGI_FORMAT_AYUV;
    return SUCCEEDED(ID3D11VideoDevice_CreateVideoDecoder(vdev, &desc, cfg, dec));
}

int lansec_hevc_dxva_probe(void *d3d_device) {
    if (!d3d_device) return 0;
    ID3D11Device *dev = (ID3D11Device *)d3d_device;
    ID3D11VideoDevice *vdev = NULL;
    if (FAILED(ID3D11Device_QueryInterface(dev, &IID_ID3D11VideoDevice, (void **)&vdev)) || !vdev) return 0;
    GUID profile;
    D3D11_VIDEO_DECODER_CONFIG cfg;
    ID3D11VideoDecoder *dec = NULL;
    int ok = create_decoder(vdev, 1920, 1080, &profile, &cfg, &dec);
    if (dec) ID3D11VideoDecoder_Release(dec);
    ID3D11VideoDevice_Release(vdev);
    return ok;
}

void *lansec_hevc_dxva_open(void *d3d_device, void *d3d_ctx, uint32_t width, uint32_t height) {
    if (!d3d_device || !d3d_ctx || width == 0 || height == 0) return NULL;
    DxvaDec *d = (DxvaDec *)calloc(1, sizeof(DxvaDec));
    if (!d) return NULL;
    d->device = (ID3D11Device *)d3d_device;
    d->ctx = (ID3D11DeviceContext *)d3d_ctx;
    ID3D11Device_AddRef(d->device);
    ID3D11DeviceContext_AddRef(d->ctx);
    if (FAILED(ID3D11Device_QueryInterface(d->device, &IID_ID3D11VideoDevice, (void **)&d->vdev))) {
        lansec_hevc_dxva_close(d);
        return NULL;
    }
    if (FAILED(ID3D11DeviceContext_QueryInterface(d->ctx, &IID_ID3D11VideoContext, (void **)&d->vctx))) {
        lansec_hevc_dxva_close(d);
        return NULL;
    }
    d->w = width;
    d->h = height;
    if (!create_decoder(d->vdev, width, height, &d->profile, &d->config, &d->decoder)) {
        lansec_hevc_dxva_close(d);
        return NULL;
    }
    D3D11_TEXTURE2D_DESC td;
    memset(&td, 0, sizeof(td));
    td.Width = width;
    td.Height = height;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = DXGI_FORMAT_AYUV;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_DECODER;
    for (int i = 0; i < DPB; i++) {
        if (FAILED(ID3D11Device_CreateTexture2D(d->device, &td, NULL, &d->tex[i]))) {
            lansec_hevc_dxva_close(d);
            return NULL;
        }
        D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC vd;
        memset(&vd, 0, sizeof(vd));
        vd.DecodeProfile = d->profile;
        vd.ViewDimension = D3D11_VDOV_DIMENSION_TEXTURE2D;
        vd.Texture2D.ArraySlice = 0;
        if (FAILED(ID3D11VideoDevice_CreateVideoDecoderOutputView(d->vdev, (ID3D11Resource *)d->tex[i], &vd, &d->view[i]))) {
            lansec_hevc_dxva_close(d);
            return NULL;
        }
        d->poc[i] = 0;
    }
    d->last_slot = -1;
    return d;
}

void lansec_hevc_dxva_close(void *dec) {
    DxvaDec *d = (DxvaDec *)dec;
    if (!d) return;
    for (int i = 0; i < DPB; i++) {
        if (d->view[i]) ID3D11VideoDecoderOutputView_Release(d->view[i]);
        if (d->tex[i]) ID3D11Texture2D_Release(d->tex[i]);
    }
    if (d->decoder) ID3D11VideoDecoder_Release(d->decoder);
    if (d->vctx) ID3D11VideoContext_Release(d->vctx);
    if (d->vdev) ID3D11VideoDevice_Release(d->vdev);
    if (d->ctx) ID3D11DeviceContext_Release(d->ctx);
    if (d->device) ID3D11Device_Release(d->device);
    free(d);
}

static void fill_picparams(DxvaDec *d, int irap, int idr, int intra, int poc, int slot, DXVA_PicParams_HEVC_RangeExt *pp) {
    memset(pp, 0, sizeof(*pp));
    Sps *s = &d->sps;
    Pps *p = &d->pps;
    int min_cb = 1 << (s->log2_min_cb_minus3 + 3);
    if (min_cb < 8) min_cb = 8;
    pp->params.PicWidthInMinCbsY = (USHORT)(s->pic_width / min_cb);
    pp->params.PicHeightInMinCbsY = (USHORT)(s->pic_height / min_cb);
    pp->params.chroma_format_idc = (USHORT)s->chroma_format_idc;
    pp->params.separate_colour_plane_flag = (USHORT)s->separate_colour_plane_flag;
    pp->params.bit_depth_luma_minus8 = (USHORT)s->bit_depth_luma_minus8;
    pp->params.bit_depth_chroma_minus8 = (USHORT)s->bit_depth_chroma_minus8;
    pp->params.log2_max_pic_order_cnt_lsb_minus4 = (USHORT)s->log2_max_poc_lsb_minus4;
    pp->params.NoPicReorderingFlag = 1;
    pp->params.NoBiPredFlag = 1;
    pp->params.CurrPic.Index7Bits = (UCHAR)slot;
    pp->params.CurrPic.AssociatedFlag = 0;
    pp->params.sps_max_dec_pic_buffering_minus1 = (UCHAR)s->sps_max_dec_pic_buffering_minus1;
    pp->params.log2_min_luma_coding_block_size_minus3 = (UCHAR)s->log2_min_cb_minus3;
    pp->params.log2_diff_max_min_luma_coding_block_size = (UCHAR)s->log2_diff_max_min_cb;
    pp->params.log2_min_transform_block_size_minus2 = (UCHAR)s->log2_min_tb_minus2;
    pp->params.log2_diff_max_min_transform_block_size = (UCHAR)s->log2_diff_max_min_tb;
    pp->params.max_transform_hierarchy_depth_inter = (UCHAR)s->max_tr_hier_inter;
    pp->params.max_transform_hierarchy_depth_intra = (UCHAR)s->max_tr_hier_intra;
    pp->params.num_short_term_ref_pic_sets = (UCHAR)s->num_short_term_ref_pic_sets;
    pp->params.num_ref_idx_l0_default_active_minus1 = (UCHAR)p->num_ref_idx_l0_default_active_minus1;
    pp->params.num_ref_idx_l1_default_active_minus1 = (UCHAR)p->num_ref_idx_l1_default_active_minus1;
    pp->params.init_qp_minus26 = (CHAR)p->init_qp_minus26;
    pp->params.amp_enabled_flag = s->amp_enabled_flag;
    pp->params.sample_adaptive_offset_enabled_flag = s->sao_enabled_flag;
    pp->params.pcm_enabled_flag = 0;
    pp->params.sps_temporal_mvp_enabled_flag = s->sps_temporal_mvp_enabled_flag;
    pp->params.strong_intra_smoothing_enabled_flag = s->strong_intra_smoothing_enabled_flag;
    pp->params.dependent_slice_segments_enabled_flag = p->dependent_slice_segments_enabled_flag;
    pp->params.output_flag_present_flag = p->output_flag_present_flag;
    pp->params.num_extra_slice_header_bits = p->num_extra_slice_header_bits;
    pp->params.sign_data_hiding_enabled_flag = p->sign_data_hiding_enabled_flag;
    pp->params.cabac_init_present_flag = p->cabac_init_present_flag;
    pp->params.constrained_intra_pred_flag = p->constrained_intra_pred_flag;
    pp->params.transform_skip_enabled_flag = p->transform_skip_enabled_flag;
    pp->params.cu_qp_delta_enabled_flag = p->cu_qp_delta_enabled_flag;
    pp->params.pps_slice_chroma_qp_offsets_present_flag = p->pps_slice_chroma_qp_offsets_present_flag;
    pp->params.weighted_pred_flag = p->weighted_pred_flag;
    pp->params.weighted_bipred_flag = p->weighted_bipred_flag;
    pp->params.transquant_bypass_enabled_flag = p->transquant_bypass_enabled_flag;
    pp->params.tiles_enabled_flag = 0;
    pp->params.entropy_coding_sync_enabled_flag = p->entropy_coding_sync_enabled_flag;
    pp->params.pps_loop_filter_across_slices_enabled_flag = p->pps_loop_filter_across_slices_enabled_flag;
    pp->params.deblocking_filter_override_enabled_flag = p->deblocking_filter_override_enabled_flag;
    pp->params.pps_deblocking_filter_disabled_flag = p->pps_deblocking_filter_disabled_flag;
    pp->params.lists_modification_present_flag = p->lists_modification_present_flag;
    pp->params.slice_segment_header_extension_present_flag = p->slice_segment_header_extension_present_flag;
    pp->params.IrapPicFlag = irap ? 1u : 0u;
    pp->params.IdrPicFlag = idr ? 1u : 0u;
    pp->params.IntraPicFlag = intra ? 1u : 0u;
    pp->params.pps_cb_qp_offset = (CHAR)p->pps_cb_qp_offset;
    pp->params.pps_cr_qp_offset = (CHAR)p->pps_cr_qp_offset;
    pp->params.diff_cu_qp_delta_depth = (UCHAR)p->diff_cu_qp_delta_depth;
    pp->params.log2_parallel_merge_level_minus2 = (UCHAR)p->log2_parallel_merge_level_minus2;
    pp->params.CurrPicOrderCntVal = poc;
    pp->params.StatusReportFeedbackNumber = 1;
    for (int i = 0; i < 15; i++) pp->params.RefPicList[i].bPicEntry = 0xff;
    for (int i = 0; i < 8; i++) {
        pp->params.RefPicSetStCurrBefore[i] = 0xff;
        pp->params.RefPicSetStCurrAfter[i] = 0xff;
        pp->params.RefPicSetLtCurr[i] = 0xff;
    }
    if (!idr && d->last_slot >= 0) {
        pp->params.RefPicList[0].Index7Bits = (UCHAR)d->last_slot;
        pp->params.RefPicList[0].AssociatedFlag = 0;
        pp->params.PicOrderCntValList[0] = d->last_poc;
        pp->params.RefPicSetStCurrBefore[0] = 0;
    }
}

static HRESULT copy_buf(ID3D11VideoContext *vctx, ID3D11VideoDecoder *dec, D3D11_VIDEO_DECODER_BUFFER_TYPE ty, const void *src, UINT n) {
    void *dst = NULL;
    UINT sz = 0;
    HRESULT hr = ID3D11VideoContext_GetDecoderBuffer(vctx, dec, ty, &sz, &dst);
    if (FAILED(hr) || !dst || sz < n) {
        if (dst) ID3D11VideoContext_ReleaseDecoderBuffer(vctx, dec, ty);
        return E_FAIL;
    }
    memcpy(dst, src, n);
    if (sz > n) {
        UINT pad = sz - n;
        if (pad > 128) pad = 128;
        memset((uint8_t *)dst + n, 0, pad);
    }
    return ID3D11VideoContext_ReleaseDecoderBuffer(vctx, dec, ty);
}

int lansec_hevc_dxva_decode(void *dec, const uint8_t *annexb, int len, void **out_tex) {
    DxvaDec *d = (DxvaDec *)dec;
    if (!d || !annexb || len <= 0 || !out_tex) return -1;
    *out_tex = NULL;
    int off = 0, nlen = 0, slice_type_nal = -1, irap = 0, idr = 0;
    const uint8_t *nal = NULL;
    const uint8_t *slice = NULL;
    int slice_len = 0;
    while (next_nal(annexb, len, &off, &nal, &nlen)) {
        int t = nal_type(nal, (size_t)nlen);
        if (t == 33) {
            if (!parse_sps(nal, (size_t)nlen, &d->sps)) return -1;
            d->have_sps = 1;
        } else if (t == 34) {
            if (!parse_pps(nal, (size_t)nlen, &d->pps)) return -1;
            d->have_pps = 1;
        } else if (t == 19 || t == 20) {
            idr = 1;
            irap = 1;
            slice = nal;
            slice_len = nlen;
            slice_type_nal = t;
        } else if (t == 21) {
            irap = 1;
            slice = nal;
            slice_len = nlen;
            slice_type_nal = t;
        } else if ((t >= 0 && t <= 9) || (t >= 16 && t <= 18)) {
            slice = nal;
            slice_len = nlen;
            slice_type_nal = t;
        }
    }
    if (!d->have_sps || !d->have_pps || !slice) return 0;
    int poc = 0;
    if (!idr) {
        /* slice_pic_order_cnt_lsb after first_slice + pps id + slice_type */
        Br b;
        br_init(&b, slice + 2, (size_t)slice_len - 2);
        br_u(&b, 1);
        if (irap) br_u(&b, 1);
        br_ue(&b);
        int st = (int)br_ue(&b);
        (void)st;
        if (d->pps.output_flag_present_flag) br_u(&b, 1);
        int lsb_bits = d->sps.log2_max_poc_lsb_minus4 + 4;
        poc = (int)br_u(&b, lsb_bits);
        if (b.err) poc = d->last_poc + 1;
    }
    int slot = d->next_slot % DPB;
    d->next_slot++;
    if (idr) {
        memset(d->used, 0, sizeof(d->used));
        d->last_slot = -1;
        poc = 0;
    }
    DXVA_PicParams_HEVC_RangeExt pp;
    fill_picparams(d, irap, idr, idr || irap, poc, slot, &pp);
    DXVA_Slice_HEVC_Short sl;
    memset(&sl, 0, sizeof(sl));
    sl.BSNALunitDataLocation = 0;
    sl.SliceBytesInBuffer = (UINT)len;
    sl.wBadSliceChopping = 0;

    HRESULT hr = ID3D11VideoContext_DecoderBeginFrame(d->vctx, d->decoder, d->view[slot], 0, NULL);
    if (FAILED(hr)) return -1;
    hr = copy_buf(d->vctx, d->decoder, D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, &pp, sizeof(pp));
    if (FAILED(hr)) goto fail;
    hr = copy_buf(d->vctx, d->decoder, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, &sl, sizeof(sl));
    if (FAILED(hr)) goto fail;
    hr = copy_buf(d->vctx, d->decoder, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, annexb, (UINT)len);
    if (FAILED(hr)) goto fail;
    D3D11_VIDEO_DECODER_BUFFER_DESC bd[3];
    memset(bd, 0, sizeof(bd));
    bd[0].BufferType = D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS;
    bd[0].DataSize = sizeof(pp);
    bd[1].BufferType = D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL;
    bd[1].DataSize = sizeof(sl);
    bd[2].BufferType = D3D11_VIDEO_DECODER_BUFFER_BITSTREAM;
    bd[2].DataSize = (UINT)len;
    hr = ID3D11VideoContext_SubmitDecoderBuffers(d->vctx, d->decoder, 3, bd);
    if (FAILED(hr)) goto fail;
    hr = ID3D11VideoContext_DecoderEndFrame(d->vctx, d->decoder);
    if (FAILED(hr)) return -1;
    ID3D11Texture2D_AddRef(d->tex[slot]);
    *out_tex = d->tex[slot];
    d->used[slot] = 1;
    d->poc[slot] = poc;
    d->last_slot = slot;
    d->last_poc = poc;
    (void)slice_type_nal;
    return 1;
fail:
    ID3D11VideoContext_DecoderEndFrame(d->vctx, d->decoder);
    return -1;
}
