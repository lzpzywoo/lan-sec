#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct LansecNvenc LansecNvenc;

int lansec_nvenc_available(void);
int lansec_nvenc_probe_yuv444(void *d3d_device);
LansecNvenc *lansec_nvenc_open(void *d3d_device, void *d3d_ctx, uint32_t width, uint32_t height, uint32_t bitrate, int yuv444);
void lansec_nvenc_close(LansecNvenc *enc);
int lansec_nvenc_set_bitrate(LansecNvenc *enc, uint32_t bitrate);
int lansec_nvenc_encode(LansecNvenc *enc, void *src_texture, int force_idr, uint8_t *out, int out_cap, int *out_len, int *is_key);

#ifdef __cplusplus
}
#endif
