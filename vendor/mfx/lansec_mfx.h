#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

int lansec_mfx_probe_hevc444(void *d3d_device);
void *lansec_mfx_open(void *d3d_device, uint32_t width, uint32_t height);
void lansec_mfx_close(void *dec);
int lansec_mfx_decode(void *dec, const uint8_t *annexb, int len, void **out_tex);

#ifdef __cplusplus
}
#endif
