// VideoToolbox round trip: encode 60 generated 1280x720 frames to H.264 (and HEVC), decode them again,
// compare a pixel checksum region, report whether hardware was used. Same output expected as x86-64.
#import <Foundation/Foundation.h>
#import <VideoToolbox/VideoToolbox.h>
#import <CoreMedia/CoreMedia.h>

static NSMutableArray *samples;
static int decoded, badpix;

static void enc_cb(void *ref, void *frameRef, OSStatus st, VTEncodeInfoFlags f, CMSampleBufferRef sb) {
    if (st == noErr && sb) [samples addObject:(__bridge id)sb];
}

static void dec_cb(void *ref, void *frameRef, OSStatus st, VTDecodeInfoFlags f, CVImageBufferRef img, CMTime pts, CMTime dur) {
    if (st != noErr || !img) return;
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
}

static int roundtrip(CMVideoCodecType codec, const char *name) {
    samples = [NSMutableArray array];
    decoded = badpix = 0;
    VTCompressionSessionRef cs = NULL;
    OSStatus st = VTCompressionSessionCreate(NULL, 1280, 720, codec, NULL, NULL, NULL, enc_cb, NULL, &cs);
    if (st) { printf("%s: compression session failed %d\n", name, (int)st); return 1; }
    CFBooleanRef hw = NULL;
    VTSessionCopyProperty(cs, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, NULL, &hw);
    VTSessionSetProperty(cs, kVTCompressionPropertyKey_RealTime, kCFBooleanFalse);
    VTSessionSetProperty(cs, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
    NSDate *t0 = [NSDate date];
    for (int f = 0; f < 60; f++) {
        CVPixelBufferRef pb = NULL;
        CVPixelBufferCreate(NULL, 1280, 720, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, (__bridge CFDictionaryRef)@{(id)kCVPixelBufferIOSurfacePropertiesKey: @{}}, &pb);
        CVPixelBufferLockBaseAddress(pb, 0);
        uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(pb, 0);
        size_t ys = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
        for (int r = 0; r < 720; r++) for (int x = 0; x < 1280; x++) y[r * ys + x] = (x + f * 4) & 255;
        uint8_t *uv = CVPixelBufferGetBaseAddressOfPlane(pb, 1);
        memset(uv, 128, CVPixelBufferGetBytesPerRowOfPlane(pb, 1) * 360);
        CVPixelBufferUnlockBaseAddress(pb, 0);
        VTCompressionSessionEncodeFrame(cs, pb, CMTimeMake(f, 30), kCMTimeInvalid, NULL, NULL, NULL);
        CVPixelBufferRelease(pb);
    }
    VTCompressionSessionCompleteFrames(cs, kCMTimeInvalid);
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
    VTSessionCopyProperty(ds, kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder, NULL, &dhw);
    t0 = [NSDate date];
    for (id s in samples) VTDecompressionSessionDecodeFrame(ds, (__bridge CMSampleBufferRef)s, 0, NULL, NULL);
    VTDecompressionSessionWaitForAsynchronousFrames(ds);
    double tdec = -[t0 timeIntervalSinceNow];
    printf("%s: encoded %lu frames (%zu bytes, hw=%d) decoded %d (hw=%d) bad=%d  enc %.0f ms dec %.0f ms\n", name,
           (unsigned long)samples.count, bytes > 0 ? (size_t)1 : 0, hw == kCFBooleanTrue, decoded, dhw == kCFBooleanTrue, badpix, tenc * 1000, tdec * 1000);
    VTCompressionSessionInvalidate(cs);
    VTDecompressionSessionInvalidate(ds);
    return decoded == 60 && badpix == 0 ? 0 : 1;
}

int main(void) {
    @autoreleasepool {
        int r = roundtrip(kCMVideoCodecType_H264, "h264");
        r |= roundtrip(kCMVideoCodecType_HEVC, "hevc");
        printf(r ? "FAIL\n" : "ok\n");
        return r;
    }
}
