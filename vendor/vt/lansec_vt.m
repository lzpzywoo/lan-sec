#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <VideoToolbox/VideoToolbox.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <CoreGraphics/CoreGraphics.h>
#import <Metal/Metal.h>
#import <QuartzCore/CAMetalLayer.h>
#import <AppKit/AppKit.h>
#import <CoreImage/CoreImage.h>
#include "lansec_vt.h"
#include <stdatomic.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <mach/mach_time.h>

#ifndef kVTProfileLevel_HEVC_Main444_AutoLevel
#define kVTProfileLevel_HEVC_Main444_AutoLevel CFSTR("HEVC_Main444_AutoLevel")
#endif
#ifndef kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality
#define kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality CFSTR("PrioritizeEncodingSpeedOverQuality")
#endif

typedef struct {
    VTCompressionSessionRef session;
    uint32_t bitrate;
    NSMutableData *pending;
    int pending_key;
    dispatch_semaphore_t sem;
} VtEnc;

typedef struct {
    VTDecompressionSessionRef session;
    CMVideoFormatDescriptionRef format;
    CVPixelBufferRef last;
    uint8_t *vps; size_t vps_len;
    uint8_t *sps; size_t sps_len;
    uint8_t *pps; size_t pps_len;
    int yuv444;
    int64_t pts;
} VtDec;

@interface LansecSckSink : NSObject <SCStreamOutput, SCStreamDelegate>
@property(atomic, assign) IOSurfaceRef surface;
@property(atomic) uint64_t capture_us;
@property(atomic) uint64_t gen;
@property(atomic) uint32_t width;
@property(atomic) uint32_t height;
@property(strong) NSMutableData *audio;
@property(strong) NSLock *audioLock;
@end

@implementation LansecSckSink
- (instancetype)init {
    self = [super init];
    if (self) {
        _audio = [NSMutableData data];
        _audioLock = [NSLock new];
    }
    return self;
}
- (void)dealloc {
    IOSurfaceRef old = _surface;
    _surface = NULL;
    if (old) CFRelease(old);
}
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer ofType:(SCStreamOutputType)type {
    if (type == SCStreamOutputTypeAudio) {
        CMBlockBufferRef bb = CMSampleBufferGetDataBuffer(sampleBuffer);
        if (!bb) return;
        size_t total = 0;
        char *ptr = NULL;
        if (CMBlockBufferGetDataPointer(bb, 0, NULL, &total, &ptr) != noErr || !ptr || total == 0) return;
        [self.audioLock lock];
        [self.audio appendBytes:ptr length:total];
        [self.audioLock unlock];
        return;
    }
    if (type != SCStreamOutputTypeScreen) return;
    CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!pb) return;
    IOSurfaceRef surf = CVPixelBufferGetIOSurface(pb);
    if (!surf) return;
    CFRetain(surf);
    IOSurfaceRef old = self.surface;
    self.surface = surf;
    if (old) CFRelease(old);
    self.capture_us = mach_absolute_time() / 1000;
    self.width = (uint32_t)CVPixelBufferGetWidth(pb);
    self.height = (uint32_t)CVPixelBufferGetHeight(pb);
    self.gen = self.gen + 1;
}
@end

typedef struct {
    SCStream *stream;
    LansecSckSink *sink;
    uint64_t last_gen;
} SckCap;

static uint64_t now_us(void) {
    return mach_absolute_time() / 1000;
}

static void append_annexb(NSMutableData *dst, const uint8_t *nal, size_t len) {
    static const uint8_t sc[4] = {0, 0, 0, 1};
    [dst appendBytes:sc length:4];
    [dst appendBytes:nal length:len];
}

static void avcc_to_annexb(NSMutableData *dst, const uint8_t *data, size_t len) {
    size_t i = 0;
    while (i + 4 <= len) {
        uint32_t n = ((uint32_t)data[i] << 24) | ((uint32_t)data[i + 1] << 16) | ((uint32_t)data[i + 2] << 8) | (uint32_t)data[i + 3];
        i += 4;
        if (n == 0 || i + n > len) break;
        append_annexb(dst, data + i, n);
        i += n;
    }
}

static void on_encoded(void *outputCallbackRefCon, void *sourceFrameRefCon, OSStatus status,
                       VTEncodeInfoFlags infoFlags, CMSampleBufferRef sampleBuffer) {
    (void)sourceFrameRefCon; (void)infoFlags;
    VtEnc *e = (VtEnc *)outputCallbackRefCon;
    if (status != noErr || !sampleBuffer) {
        dispatch_semaphore_signal(e->sem);
        return;
    }
    BOOL key = NO;
    CFArrayRef atts = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
    if (atts && CFArrayGetCount(atts) > 0) {
        CFDictionaryRef d = CFArrayGetValueAtIndex(atts, 0);
        CFBooleanRef notSync = CFDictionaryGetValue(d, kCMSampleAttachmentKey_NotSync);
        key = !(notSync == kCFBooleanTrue);
    }
    NSMutableData *out = [NSMutableData data];
    CMFormatDescriptionRef fmt = CMSampleBufferGetFormatDescription(sampleBuffer);
    if (fmt && key) {
        size_t psCount = 0;
        int nalLen = 4;
        if (CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, 0, NULL, NULL, &psCount, &nalLen) == noErr) {
            for (size_t i = 0; i < psCount; i++) {
                const uint8_t *ps = NULL;
                size_t psLen = 0;
                if (CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, i, &ps, &psLen, NULL, NULL) == noErr && ps && psLen) {
                    append_annexb(out, ps, psLen);
                }
            }
        }
    }
    CMBlockBufferRef bb = CMSampleBufferGetDataBuffer(sampleBuffer);
    size_t total = 0;
    char *ptr = NULL;
    if (bb) CMBlockBufferGetDataPointer(bb, 0, NULL, &total, &ptr);
    if (ptr && total) avcc_to_annexb(out, (const uint8_t *)ptr, total);
    @synchronized (e->pending) {
        [e->pending setData:out];
        e->pending_key = key ? 1 : 0;
    }
    dispatch_semaphore_signal(e->sem);
}

int lansec_vt_probe_444(void) {
    CFMutableDictionaryRef spec = CFDictionaryCreateMutable(kCFAllocatorDefault, 2, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(spec, kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, kCFBooleanTrue);
    VTCompressionSessionRef s = NULL;
    OSStatus st = VTCompressionSessionCreate(kCFAllocatorDefault, 1280, 720, kCMVideoCodecType_HEVC, spec, NULL, NULL, NULL, NULL, &s);
    CFRelease(spec);
    if (st != noErr || !s) return 0;
    OSStatus set = VTSessionSetProperty(s, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main444_AutoLevel);
    CFBooleanRef hw = NULL;
    VTSessionCopyProperty(s, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, kCFAllocatorDefault, &hw);
    if (hw) CFRelease(hw);
    VTCompressionSessionInvalidate(s);
    CFRelease(s);
    return (set == noErr) ? 1 : 0;
}

int lansec_vt_probe_decode_444(void) {
    return 1;
}

static void vt_apply_bitrate(VTCompressionSessionRef s, uint32_t bitrate) {
    if (!s || !bitrate) return;
    CFNumberRef br = CFNumberCreate(NULL, kCFNumberSInt32Type, &bitrate);
    VTSessionSetProperty(s, kVTCompressionPropertyKey_AverageBitRate, br);
    CFRelease(br);
    // Hard cap over a 1s window so IDRs cannot dump 80 Mbps onto the LAN.
    int64_t bytes = (int64_t)(bitrate / 8);
    double seconds = 1.0;
    CFNumberRef nbytes = CFNumberCreate(NULL, kCFNumberSInt64Type, &bytes);
    CFNumberRef nsec = CFNumberCreate(NULL, kCFNumberDoubleType, &seconds);
    const void *vals[2] = { nbytes, nsec };
    CFArrayRef limits = CFArrayCreate(kCFAllocatorDefault, vals, 2, &kCFTypeArrayCallBacks);
    VTSessionSetProperty(s, kVTCompressionPropertyKey_DataRateLimits, limits);
    CFRelease(limits);
    CFRelease(nbytes);
    CFRelease(nsec);
}

void *lansec_vt_open(uint32_t width, uint32_t height, uint32_t bitrate, int yuv444) {
    VtEnc *e = calloc(1, sizeof(VtEnc));
    if (!e) return NULL;
    e->bitrate = bitrate ? bitrate : 20000000;
    e->pending = [NSMutableData data];
    e->sem = dispatch_semaphore_create(0);
    CFMutableDictionaryRef spec = CFDictionaryCreateMutable(kCFAllocatorDefault, 2, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(spec, kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, kCFBooleanTrue);
    CFDictionarySetValue(spec, kVTVideoEncoderSpecification_EnableLowLatencyRateControl, kCFBooleanTrue);
    OSStatus st = VTCompressionSessionCreate(kCFAllocatorDefault, (int32_t)width, (int32_t)height, kCMVideoCodecType_HEVC,
                                             spec, NULL, NULL, on_encoded, e, &e->session);
    CFRelease(spec);
    if (st != noErr) { free(e); return NULL; }
    VTSessionSetProperty(e->session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
    VTSessionSetProperty(e->session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
    int delay = 0;
    CFNumberRef dly = CFNumberCreate(NULL, kCFNumberIntType, &delay);
    VTSessionSetProperty(e->session, kVTCompressionPropertyKey_MaxFrameDelayCount, dly);
    CFRelease(dly);
    VTSessionSetProperty(e->session, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue);
    int fps = 60;
    CFNumberRef efps = CFNumberCreate(NULL, kCFNumberIntType, &fps);
    VTSessionSetProperty(e->session, kVTCompressionPropertyKey_ExpectedFrameRate, efps);
    CFRelease(efps);
    int gop = 60;
    CFNumberRef kgop = CFNumberCreate(NULL, kCFNumberIntType, &gop);
    VTSessionSetProperty(e->session, kVTCompressionPropertyKey_MaxKeyFrameInterval, kgop);
    CFRelease(kgop);
    double gop_s = 1.0;
    CFNumberRef kgopd = CFNumberCreate(NULL, kCFNumberDoubleType, &gop_s);
    VTSessionSetProperty(e->session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kgopd);
    CFRelease(kgopd);
    vt_apply_bitrate(e->session, e->bitrate);
    if (yuv444) {
        if (VTSessionSetProperty(e->session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main444_AutoLevel) != noErr) {
            lansec_vt_close(e);
            return NULL;
        }
    } else {
        VTSessionSetProperty(e->session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel);
    }
    VTCompressionSessionPrepareToEncodeFrames(e->session);
    return e;
}

void lansec_vt_close(void *session) {
    VtEnc *e = (VtEnc *)session;
    if (!e) return;
    if (e->session) {
        VTCompressionSessionInvalidate(e->session);
        CFRelease(e->session);
    }
    e->pending = nil;
    free(e);
}

void lansec_vt_set_bitrate(void *session, uint32_t bitrate) {
    VtEnc *e = (VtEnc *)session;
    if (!e || !e->session || !bitrate) return;
    e->bitrate = bitrate;
    vt_apply_bitrate(e->session, bitrate);
}

int lansec_vt_encode(void *session, void *pixel_buffer, int force_idr, uint8_t *out, int cap, int *len, int *key) {
    VtEnc *e = (VtEnc *)session;
    if (!e || !pixel_buffer || !out || !len) return 0;
    CMTime pts = CMTimeMake((int64_t)now_us(), 1000000);
    CFMutableDictionaryRef props = NULL;
    if (force_idr) {
        props = CFDictionaryCreateMutable(NULL, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        CFDictionarySetValue(props, kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue);
    }
    // Drain leftover signals from a previous timeout BEFORE starting this
    // frame, otherwise we consume this frame's callback and wait on nothing.
    while (dispatch_semaphore_wait(e->sem, DISPATCH_TIME_NOW) == 0) {}
    @synchronized (e->pending) {
        [e->pending setLength:0];
        e->pending_key = 0;
    }
    OSStatus st = VTCompressionSessionEncodeFrame(e->session, (CVPixelBufferRef)pixel_buffer, pts, kCMTimeInvalid, props, NULL, NULL);
    if (props) CFRelease(props);
    if (st != noErr) return 0;
    // Encode is typically ~8ms; the wait must outlast that. Timing out at 8ms
    // drops the frame and makes motion feel like a low FPS stream.
    dispatch_semaphore_wait(e->sem, dispatch_time(DISPATCH_TIME_NOW, 16 * NSEC_PER_MSEC));
    @synchronized (e->pending) {
        int n = (int)e->pending.length;
        if (n > cap) n = cap;
        if (n > 0) memcpy(out, e->pending.bytes, (size_t)n);
        *len = n;
        if (key) *key = e->pending_key;
    }
    return 1;
}

static void on_decoded(void *decompressionOutputRefCon, void *sourceFrameRefCon, OSStatus status,
                       VTDecodeInfoFlags infoFlags, CVImageBufferRef imageBuffer, CMTime presentationTimeStamp,
                       CMTime presentationDuration) {
    (void)sourceFrameRefCon; (void)presentationTimeStamp; (void)presentationDuration;
    VtDec *d = (VtDec *)decompressionOutputRefCon;
    if (status != noErr || !imageBuffer) {
        fprintf(stderr, "vt-dec: callback status=%d flags=%u image=%p\n", (int)status, (unsigned)infoFlags, imageBuffer);
        return;
    }
    CVPixelBufferRef pb = (CVPixelBufferRef)imageBuffer;
    CVPixelBufferRetain(pb);
    if (d->last) CVPixelBufferRelease(d->last);
    d->last = pb;
}

static int nal_type(const uint8_t *nal, size_t len) {
    if (len < 2) return -1;
    return (nal[0] >> 1) & 0x3F;
}

static void copy_ps(uint8_t **dst, size_t *dst_len, const uint8_t *nal, size_t len) {
    free(*dst);
    *dst = (uint8_t *)malloc(len);
    if (*dst) {
        memcpy(*dst, nal, len);
        *dst_len = len;
    } else {
        *dst_len = 0;
    }
}

static int parse_annexb_ps(VtDec *d, const uint8_t *data, int len) {
    int i = 0;
    int found = 0;
    while (i + 3 < len) {
        int sc = 0;
        if (i + 4 <= len && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1) sc = 4;
        else if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1) sc = 3;
        else { i++; continue; }
        int start = i + sc;
        int j = start;
        while (j + 3 < len) {
            if (data[j] == 0 && data[j + 1] == 0 && (data[j + 2] == 1 || (data[j + 2] == 0 && j + 4 <= len && data[j + 3] == 1))) break;
            j++;
        }
        if (j + 3 >= len) j = len;
        const uint8_t *nal = data + start;
        size_t nlen = (size_t)(j - start);
        int t = nal_type(nal, nlen);
        if (t == 32) { copy_ps(&d->vps, &d->vps_len, nal, nlen); found = 1; }
        else if (t == 33) { copy_ps(&d->sps, &d->sps_len, nal, nlen); found = 1; }
        else if (t == 34) { copy_ps(&d->pps, &d->pps_len, nal, nlen); found = 1; }
        i = j;
    }
    return found;
}

static int annexb_to_avcc(const uint8_t *data, int len, uint8_t **out, int *out_len) {
    uint8_t *buf = (uint8_t *)malloc((size_t)len + 64);
    if (!buf) return 0;
    int o = 0;
    int i = 0;
    while (i + 3 < len) {
        int sc = 0;
        if (i + 4 <= len && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1) sc = 4;
        else if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1) sc = 3;
        else { i++; continue; }
        int start = i + sc;
        int j = start;
        while (j + 3 < len) {
            if (data[j] == 0 && data[j + 1] == 0 && (data[j + 2] == 1 || (data[j + 2] == 0 && j + 4 <= len && data[j + 3] == 1))) break;
            j++;
        }
        if (j + 3 >= len) j = len;
        uint32_t n = (uint32_t)(j - start);
        buf[o++] = (uint8_t)(n >> 24);
        buf[o++] = (uint8_t)(n >> 16);
        buf[o++] = (uint8_t)(n >> 8);
        buf[o++] = (uint8_t)n;
        memcpy(buf + o, data + start, n);
        o += (int)n;
        i = j;
    }
    *out = buf;
    *out_len = o;
    return o > 0;
}

static int ensure_vt_dec(VtDec *d, int yuv444) {
    if (d->session) return 1;
    if (!d->vps || !d->sps || !d->pps) {
        fprintf(stderr, "vt-dec: missing parameter sets vps=%zu sps=%zu pps=%zu\n", d->vps_len, d->sps_len, d->pps_len);
        return 0;
    }
    const uint8_t *sets[3] = { d->vps, d->sps, d->pps };
    size_t sizes[3] = { d->vps_len, d->sps_len, d->pps_len };
    CMVideoFormatDescriptionRef fmt = NULL;
    OSStatus ps = CMVideoFormatDescriptionCreateFromHEVCParameterSets(kCFAllocatorDefault, 3, sets, sizes, 4, NULL, &fmt);
    if (ps != noErr || !fmt) {
        fprintf(stderr, "vt-dec: CreateFromHEVCParameterSets status=%d vps=%zu sps=%zu pps=%zu\n", (int)ps, d->vps_len, d->sps_len, d->pps_len);
        return 0;
    }
    CFMutableDictionaryRef attrs = CFDictionaryCreateMutable(kCFAllocatorDefault, 2, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    // Apple HEVC 4:4:4 decode rejects packed 444YpCbCr8; BGRA is the working hardware path.
    int32_t pf = yuv444 ? kCVPixelFormatType_32BGRA : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
    CFNumberRef pix = CFNumberCreate(NULL, kCFNumberSInt32Type, &pf);
    CFDictionarySetValue(attrs, kCVPixelBufferPixelFormatTypeKey, pix);
    CFRelease(pix);
    CFDictionarySetValue(attrs, kCVPixelBufferMetalCompatibilityKey, kCFBooleanTrue);
    VTDecompressionOutputCallbackRecord cb = { on_decoded, d };
    OSStatus st = VTDecompressionSessionCreate(kCFAllocatorDefault, fmt, NULL, attrs, &cb, &d->session);
    CFRelease(attrs);
    if (st != noErr) {
        fprintf(stderr, "vt-dec: VTDecompressionSessionCreate status=%d yuv444=%d, retry with default attrs\n", (int)st, yuv444);
        st = VTDecompressionSessionCreate(kCFAllocatorDefault, fmt, NULL, NULL, &cb, &d->session);
    }
    if (st != noErr || !d->session) {
        fprintf(stderr, "vt-dec: VTDecompressionSessionCreate failed status=%d\n", (int)st);
        CFRelease(fmt);
        return 0;
    }
    d->format = fmt;
    fprintf(stderr, "vt-dec: session ready yuv444=%d\n", yuv444);
    return 1;
}

void *lansec_vt_dec_open(int yuv444) {
    VtDec *d = calloc(1, sizeof(VtDec));
    if (d) d->yuv444 = yuv444;
    return d;
}

void lansec_vt_dec_close(void *session) {
    VtDec *d = (VtDec *)session;
    if (!d) return;
    if (d->session) {
        VTDecompressionSessionInvalidate(d->session);
        CFRelease(d->session);
    }
    if (d->format) CFRelease(d->format);
    if (d->last) CVPixelBufferRelease(d->last);
    free(d->vps); free(d->sps); free(d->pps);
    free(d);
}

int lansec_vt_dec_decode(void *session, const uint8_t *data, int len, void **pixel_buffer) {
    VtDec *d = (VtDec *)session;
    if (!d || !data || len <= 0 || !pixel_buffer) return 0;
    parse_annexb_ps(d, data, len);
    if (!ensure_vt_dec(d, d->yuv444)) return 0;
    uint8_t *avcc = NULL;
    int avcc_len = 0;
    if (!annexb_to_avcc(data, len, &avcc, &avcc_len)) return 0;
    CMBlockBufferRef bb = NULL;
    if (CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, avcc, (size_t)avcc_len, kCFAllocatorMalloc, NULL, 0, (size_t)avcc_len, 0, &bb) != noErr) {
        free(avcc);
        return 0;
    }
    CMSampleBufferRef sb = NULL;
    d->pts += 1;
    CMSampleTimingInfo timing = {CMTimeMake(1, 60), CMTimeMake((int64_t)d->pts, 60), kCMTimeInvalid};
    OSStatus sb_st = CMSampleBufferCreateReady(kCFAllocatorDefault, bb, d->format, 1, 1, &timing, 1, (size_t[]){(size_t)avcc_len}, &sb);
    CFRelease(bb);
    if (sb_st != noErr || !sb) {
        fprintf(stderr, "vt-dec: CMSampleBufferCreateReady status=%d\n", (int)sb_st);
        return 0;
    }
    OSStatus dec_st = VTDecompressionSessionDecodeFrame(d->session, sb, 0, NULL, NULL);
    if (dec_st != noErr) {
        fprintf(stderr, "vt-dec: DecodeFrame status=%d avcc_len=%d\n", (int)dec_st, avcc_len);
    }
    VTDecompressionSessionWaitForAsynchronousFrames(d->session);
    CFRelease(sb);
    if (!d->last) {
        fprintf(stderr, "vt-dec: no output frame after decode status=%d\n", (int)dec_st);
        return 0;
    }
    *pixel_buffer = d->last;
    CVPixelBufferRetain(d->last);
    return 1;
}

void *lansec_sck_start(uint32_t *width, uint32_t *height) {
    SckCap *c = calloc(1, sizeof(SckCap));
    c->sink = [LansecSckSink new];
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block SCShareableContent *content = nil;
    [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *cont, NSError *err) {
        (void)err;
        content = cont;
        dispatch_semaphore_signal(sem);
    }];
    dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
    if (!content || content.displays.count == 0) { free(c); return NULL; }
    SCDisplay *disp = content.displays.firstObject;
    SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:disp excludingWindows:@[]];
    SCStreamConfiguration *cfg = [SCStreamConfiguration new];
    cfg.width = disp.width;
    cfg.height = disp.height;
    cfg.pixelFormat = kCVPixelFormatType_32BGRA;
    // Parsec-style: do not bake the cursor into the video. The client draws its
    // local cursor immediately; the captured cursor would lag by one encode/decode.
    cfg.showsCursor = NO;
    cfg.capturesAudio = YES;
    cfg.sampleRate = 48000;
    cfg.channelCount = 2;
    cfg.minimumFrameInterval = CMTimeMake(1, 60);
    // Valid range is 3–8. Deeper queues add capture latency when encode is slow.
    cfg.queueDepth = 3;
    c->stream = [[SCStream alloc] initWithFilter:filter configuration:cfg delegate:c->sink];
    NSError *err = nil;
    [c->stream addStreamOutput:c->sink type:SCStreamOutputTypeScreen sampleHandlerQueue:dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0) error:&err];
    [c->stream addStreamOutput:c->sink type:SCStreamOutputTypeAudio sampleHandlerQueue:dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0) error:&err];
    dispatch_semaphore_t started = dispatch_semaphore_create(0);
    [c->stream startCaptureWithCompletionHandler:^(NSError *e) { (void)e; dispatch_semaphore_signal(started); }];
    dispatch_semaphore_wait(started, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
    if (width) *width = (uint32_t)disp.width;
    if (height) *height = (uint32_t)disp.height;
    return c;
}

void lansec_sck_stop(void *cap) {
    SckCap *c = (SckCap *)cap;
    if (!c) return;
    [c->stream stopCaptureWithCompletionHandler:^(NSError *e) { (void)e; }];
    c->stream = nil;
    c->sink = nil;
    free(c);
}

void *lansec_sck_next(void *cap, uint64_t *capture_us, int *fresh) {
    SckCap *c = (SckCap *)cap;
    if (!c) return NULL;
    IOSurfaceRef s = c->sink.surface;
    if (!s) return NULL;
    CFRetain(s);
    uint64_t gen = c->sink.gen;
    if (fresh) *fresh = (gen != c->last_gen) ? 1 : 0;
    c->last_gen = gen;
    if (capture_us) *capture_us = c->sink.capture_us;
    CVPixelBufferRef pb = NULL;
    CVPixelBufferCreateWithIOSurface(kCFAllocatorDefault, s, NULL, &pb);
    CFRelease(s);
    return pb;
}

int lansec_sck_next_audio(void *cap, float *out, int cap_samples) {
    SckCap *c = (SckCap *)cap;
    if (!c || !out || cap_samples <= 0) return 0;
    [c->sink.audioLock lock];
    NSUInteger bytes = c->sink.audio.length;
    int n = (int)(bytes / sizeof(float));
    if (n > cap_samples) n = cap_samples;
    if (n > 0) {
        memcpy(out, c->sink.audio.bytes, (size_t)n * sizeof(float));
        NSUInteger used = (NSUInteger)n * sizeof(float);
        [c->sink.audio replaceBytesInRange:NSMakeRange(0, used) withBytes:NULL length:0];
    }
    [c->sink.audioLock unlock];
    return n;
}

void lansec_cf_release(void *obj) {
    if (obj) CFRelease(obj);
}

int lansec_cg_mouse_abs(uint16_t x, uint16_t y) {
    CGEventRef e = CGEventCreateMouseEvent(NULL, kCGEventMouseMoved, CGPointMake(x, y), kCGMouseButtonLeft);
    if (!e) return 0;
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
    return 1;
}

int lansec_cg_mouse_rel(int16_t dx, int16_t dy) {
    CGEventRef loc = CGEventCreate(NULL);
    CGPoint p = CGEventGetLocation(loc);
    CFRelease(loc);
    return lansec_cg_mouse_abs((uint16_t)(p.x + dx), (uint16_t)(p.y + dy));
}

int lansec_cg_button(uint8_t button, int down) {
    CGEventType type = kCGEventLeftMouseDown;
    CGMouseButton b = kCGMouseButtonLeft;
    if (button == 0) { type = down ? kCGEventLeftMouseDown : kCGEventLeftMouseUp; b = kCGMouseButtonLeft; }
    else if (button == 1) { type = down ? kCGEventRightMouseDown : kCGEventRightMouseUp; b = kCGMouseButtonRight; }
    else { type = down ? kCGEventOtherMouseDown : kCGEventOtherMouseUp; b = kCGMouseButtonCenter; }
    CGEventRef loc = CGEventCreate(NULL);
    CGPoint p = CGEventGetLocation(loc);
    CFRelease(loc);
    CGEventRef e = CGEventCreateMouseEvent(NULL, type, p, b);
    if (!e) return 0;
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
    return 1;
}

int lansec_cg_wheel(int16_t dx, int16_t dy) {
    CGEventRef e = CGEventCreateScrollWheelEvent(NULL, kCGScrollEventUnitPixel, 2, dy, dx);
    if (!e) return 0;
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
    return 1;
}

int lansec_cg_key(uint16_t vk, int down) {
    CGEventRef e = CGEventCreateKeyboardEvent(NULL, (CGKeyCode)vk, down ? true : false);
    if (!e) return 0;
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
    return 1;
}

typedef struct {
    id<MTLDevice> device;
    id<MTLCommandQueue> queue;
    CAMetalLayer *layer;
    CIContext *ci;
    NSView *view;
} MetalPres;

void *lansec_metal_open(void *nsview, uint32_t w, uint32_t h) {
    if (!nsview) return NULL;
    NSView *view = (__bridge NSView *)nsview;
    id<MTLDevice> device = MTLCreateSystemDefaultDevice();
    if (!device) return NULL;
    CAMetalLayer *layer = [CAMetalLayer layer];
    layer.device = device;
    layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    layer.framebufferOnly = YES;
    layer.drawableSize = CGSizeMake(w > 0 ? w : 1280, h > 0 ? h : 720);
    void (^attach)(void) = ^{
        view.wantsLayer = YES;
        view.layer = layer;
    };
    if ([NSThread isMainThread]) attach();
    else dispatch_sync(dispatch_get_main_queue(), attach);
    MetalPres *m = calloc(1, sizeof(MetalPres));
    m->device = device;
    m->queue = [device newCommandQueue];
    m->layer = layer;
    m->ci = [CIContext contextWithMTLDevice:device];
    m->view = view;
    return m;
}

int lansec_metal_present(void *ctx, void *pixel_buffer) {
    MetalPres *m = (MetalPres *)ctx;
    if (!m || !pixel_buffer) return 0;
    CVPixelBufferRef pb = (CVPixelBufferRef)pixel_buffer;
    CIImage *img = [CIImage imageWithCVPixelBuffer:pb];
    if (!img) return 0;
    id<CAMetalDrawable> drawable = [m->layer nextDrawable];
    if (!drawable) return 0;
    id<MTLCommandBuffer> cmd = [m->queue commandBuffer];
    CGColorSpaceRef cs = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
    [m->ci render:img toMTLTexture:drawable.texture commandBuffer:cmd bounds:img.extent colorSpace:cs];
    CGColorSpaceRelease(cs);
    [cmd presentDrawable:drawable];
    [cmd commit];
    return 1;
}

void lansec_metal_resize(void *ctx, uint32_t w, uint32_t h) {
    MetalPres *m = (MetalPres *)ctx;
    if (!m || !m->layer) return;
    m->layer.drawableSize = CGSizeMake(w, h);
}

void lansec_metal_close(void *ctx) {
    MetalPres *m = (MetalPres *)ctx;
    if (!m) return;
    m->ci = nil;
    m->queue = nil;
    m->layer = nil;
    m->device = nil;
    m->view = nil;
    free(m);
}
