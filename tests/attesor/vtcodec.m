// VideoToolbox round trip: encode 60 generated 1280x720 frames to H.264 (and HEVC), decode them again,
// compare a pixel checksum region, report whether hardware was used. Same output expected as x86-64.
#import <Foundation/Foundation.h>
#import <VideoToolbox/VideoToolbox.h>
#import <CoreMedia/CoreMedia.h>

static NSMutableArray *samples;
static int decoded, badpix, callbackErrors;
static NSLock *sampleLock;
#define CHECK(call) do { \
    OSStatus result = (call); \
    if (result) { fprintf(stderr, "%s failed %d\n", #call, (int)result); return 1; } \
} while (0)

static void enc_cb(void *ref, void *frameRef, OSStatus st, VTEncodeInfoFlags f, CMSampleBufferRef sb) {
    [sampleLock lock];
    if (st == noErr && sb) [samples addObject:(__bridge id)sb];
    else callbackErrors++;
    [sampleLock unlock];
}

static void dec_cb(void *ref, void *frameRef, OSStatus st, VTDecodeInfoFlags f, CVImageBufferRef img, CMTime pts, CMTime dur) {
    [sampleLock lock];
    if (st != noErr || !img) { callbackErrors++; [sampleLock unlock]; return; }
    decoded++;
    CVPixelBufferLockBaseAddress(img, kCVPixelBufferLock_ReadOnly);
    const uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(img, 0);
    size_t stride = CVPixelBufferGetBytesPerRowOfPlane(img, 0);
    int frame = (int)pts.value;
    // the encoder saw luma = (x + frame*4) & 255 in a flat band: lossy, so allow a tolerance
    for (int x = 64; x < 1200; x += 97) {
        int want = (x + frame * 4) & 255, got = y[360 * stride + x];
        if (abs(want - got) > 24 && abs(want - got) < 232) badpix++;
    }
    CVPixelBufferUnlockBaseAddress(img, kCVPixelBufferLock_ReadOnly);
    [sampleLock unlock];
}

static int roundtrip(CMVideoCodecType codec, const char *name) {
    samples = [NSMutableArray array];
    sampleLock = [NSLock new];
    decoded = badpix = callbackErrors = 0;
    VTCompressionSessionRef cs = NULL;
    OSStatus st = VTCompressionSessionCreate(NULL, 1280, 720, codec, NULL, NULL, NULL, enc_cb, NULL, &cs);
    if (st) { printf("%s: compression session failed %d\n", name, (int)st); return 1; }
    CFBooleanRef hw = NULL;
    // Software codecs do not implement the property (kVTPropertyNotSupportedErr): that means "not hardware".
    { OSStatus ps = VTSessionCopyProperty(cs, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, NULL, &hw);
      if (ps == kVTPropertyNotSupportedErr) hw = kCFBooleanFalse;
      else CHECK(ps); }
    CHECK(VTSessionSetProperty(cs, kVTCompressionPropertyKey_RealTime, kCFBooleanFalse));
    CHECK(VTSessionSetProperty(cs, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse));
    NSDate *t0 = [NSDate date];
    for (int f = 0; f < 60; f++) {
        CVPixelBufferRef pb = NULL;
        CHECK(CVPixelBufferCreate(NULL, 1280, 720, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, (__bridge CFDictionaryRef)@{(id)kCVPixelBufferIOSurfacePropertiesKey: @{}}, &pb));
        CVPixelBufferLockBaseAddress(pb, 0);
        uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(pb, 0);
        size_t ys = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
        for (int r = 0; r < 720; r++) for (int x = 0; x < 1280; x++) y[r * ys + x] = (x + f * 4) & 255;
        uint8_t *uv = CVPixelBufferGetBaseAddressOfPlane(pb, 1);
        memset(uv, 128, CVPixelBufferGetBytesPerRowOfPlane(pb, 1) * 360);
        CVPixelBufferUnlockBaseAddress(pb, 0);
        CHECK(VTCompressionSessionEncodeFrame(cs, pb, CMTimeMake(f, 30), kCMTimeInvalid, NULL, NULL, NULL));
        CVPixelBufferRelease(pb);
    }
    CHECK(VTCompressionSessionCompleteFrames(cs, kCMTimeInvalid));
    double tenc = -[t0 timeIntervalSinceNow];
    size_t bytes = 0;
    for (id s in samples) bytes += CMSampleBufferGetTotalSampleSize((__bridge CMSampleBufferRef)s);
    if (!samples.count) { printf("%s: no encoded frames\n", name); return 1; }
    CMFormatDescriptionRef fd = CMSampleBufferGetFormatDescription((__bridge CMSampleBufferRef)samples[0]);
    VTDecompressionSessionRef ds = NULL;
    VTDecompressionOutputCallbackRecord rec = {dec_cb, NULL};
    NSDictionary *attrs = @{(id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)};
    st = VTDecompressionSessionCreate(NULL, fd, NULL, (__bridge CFDictionaryRef)attrs, &rec, &ds);
    if (st) { printf("%s: decompression session failed %d\n", name, (int)st); return 1; }
    CFBooleanRef dhw = NULL;
    // Software codecs do not implement the property (kVTPropertyNotSupportedErr): that means "not hardware".
    { OSStatus ps = VTSessionCopyProperty(ds, kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder, NULL, &dhw);
      if (ps == kVTPropertyNotSupportedErr) dhw = kCFBooleanFalse;
      else CHECK(ps); }
    t0 = [NSDate date];
    for (id s in samples) CHECK(VTDecompressionSessionDecodeFrame(ds, (__bridge CMSampleBufferRef)s, 0, NULL, NULL));
    CHECK(VTDecompressionSessionWaitForAsynchronousFrames(ds));
    double tdec = -[t0 timeIntervalSinceNow];
    printf("%s: encoded %lu frames (%zu bytes, hw=%d) decoded %d (hw=%d) bad=%d  enc %.0f ms dec %.0f ms\n", name,
           (unsigned long)samples.count, bytes > 0 ? (size_t)1 : 0, hw == kCFBooleanTrue, decoded, dhw == kCFBooleanTrue, badpix, tenc * 1000, tdec * 1000);
    // Hardware is required only where this host has it natively (HEVC on this machine; its H.264
    // encoder is software even for x86-64 programs), so the bridge must match native, not exceed it.
    BOOL requireHW = getenv("ATTESOR_GPU_VT") && !strcmp(getenv("ATTESOR_GPU_VT"), "1") && !strcmp(name, "hevc");
    BOOL hardwareOK = !requireHW || (hw == kCFBooleanTrue && dhw == kCFBooleanTrue);
    if (!hardwareOK) fprintf(stderr, "%s: hardware required but not used\n", name);
    if (hw) CFRelease(hw);
    if (dhw) CFRelease(dhw);
    VTCompressionSessionInvalidate(cs);
    VTDecompressionSessionInvalidate(ds);
    CFRelease(cs); CFRelease(ds);
    return samples.count == 60 && decoded == 60 && badpix == 0 && callbackErrors == 0 && hardwareOK ? 0 : 1;
}

int main(void) {
    @autoreleasepool {
        int r = roundtrip(kCMVideoCodecType_H264, "h264");
        r |= roundtrip(kCMVideoCodecType_HEVC, "hevc");
        printf(r ? "FAIL\n" : "ok\n");
        return r;
    }
}
