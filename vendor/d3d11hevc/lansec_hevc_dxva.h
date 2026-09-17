#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/** Probe DXVA HEVC. yuv444=0 → Main + NV12; yuv444!=0 → Main444 + AYUV. */
int lansec_hevc_dxva_probe(void *d3d_device, int yuv444);
/** Open DXVA HEVC decoder. yuv444 selects Main/NV12 vs Main444/AYUV. */
void *lansec_hevc_dxva_open(void *d3d_device, void *d3d_ctx, uint32_t width, uint32_t height, int yuv444);
void lansec_hevc_dxva_close(void *dec);
int lansec_hevc_dxva_decode(void *dec, const uint8_t *annexb, int len, void **out_tex);

#ifdef __cplusplus
}
#endif
