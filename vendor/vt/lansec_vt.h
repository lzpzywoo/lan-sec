#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

int lansec_vt_probe_444(void);
void *lansec_vt_open(uint32_t width, uint32_t height, uint32_t bitrate, int yuv444);
void lansec_vt_close(void *session);
void lansec_vt_set_bitrate(void *session, uint32_t bitrate);
int lansec_vt_encode(void *session, void *pixel_buffer, int force_idr, uint8_t *out, int cap, int *len, int *key);

void *lansec_vt_dec_open(int yuv444);
void lansec_vt_dec_close(void *session);
int lansec_vt_dec_decode(void *session, const uint8_t *data, int len, void **pixel_buffer);

void *lansec_sck_start(uint32_t *width, uint32_t *height);
void lansec_sck_stop(void *cap);
void *lansec_sck_next(void *cap, uint64_t *capture_us);
int lansec_sck_next_audio(void *cap, float *out, int cap_samples);
void lansec_cf_release(void *obj);

int lansec_vt_probe_decode_444(void);
int lansec_cg_mouse_abs(uint16_t x, uint16_t y);
int lansec_cg_mouse_rel(int16_t dx, int16_t dy);
int lansec_cg_button(uint8_t button, int down);
int lansec_cg_wheel(int16_t dx, int16_t dy);
int lansec_cg_key(uint16_t vk, int down);

void *lansec_metal_open(void *nsview, uint32_t w, uint32_t h);
int lansec_metal_present(void *ctx, void *pixel_buffer);
void lansec_metal_resize(void *ctx, uint32_t w, uint32_t h);
void lansec_metal_close(void *ctx);

#ifdef __cplusplus
}
#endif
