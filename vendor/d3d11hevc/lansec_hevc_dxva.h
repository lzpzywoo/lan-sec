#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

int lansec_hevc_dxva_probe(void *d3d_device);
void *lansec_hevc_dxva_open(void *d3d_device, void *d3d_ctx, uint32_t width, uint32_t height);
void lansec_hevc_dxva_close(void *dec);
int lansec_hevc_dxva_decode(void *dec, const uint8_t *annexb, int len, void **out_tex);

#ifdef __cplusplus
}
#endif
