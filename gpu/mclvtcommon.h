// VT wire values are plists, never CF objects or guest callbacks. H.264/HEVC,
// one compressed video sample per callback; CMTime layout is identical on both ABIs.
#import <VideoToolbox/VideoToolbox.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <IOSurface/IOSurface.h>

static NSData *vt_time(CMTime t) { return [NSData dataWithBytes:&t length:sizeof t]; }
static CMTime vt_untime(NSData *d) {
    CMTime t = kCMTimeInvalid;
    if (d.length == sizeof t) memcpy(&t, d.bytes, sizeof t);
    return t;
}
static NSDictionary *vt_format(CMFormatDescriptionRef fd) {
    CMVideoCodecType codec = CMFormatDescriptionGetMediaSubType(fd);
    if (codec != kCMVideoCodecType_H264 && codec != kCMVideoCodecType_HEVC) return nil;
    NSMutableArray *sets = [NSMutableArray array];
    size_t count = 0; int nal = 0;
    for (size_t i = 0; ; i++) {
        const uint8_t *p = NULL; size_t n = 0;
        OSStatus st = codec == kCMVideoCodecType_H264 ?
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fd, i, &p, &n, &count, &nal) :
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fd, i, &p, &n, &count, &nal);
        if (st || !p || !n) return nil;
        [sets addObject:[NSData dataWithBytes:p length:n]];
        if (i + 1 == count) break;
        if (count > 32) return nil;
    }
    return @{@"codec": @(codec), @"nal": @(nal), @"sets": sets,
             @"extensions": (__bridge NSDictionary *)CMFormatDescriptionGetExtensions(fd) ?: @{}};
}
static OSStatus vt_unformat(NSDictionary *d, CMVideoFormatDescriptionRef *fd) {
    NSArray *sets = d[@"sets"];
    if (!sets.count || sets.count > 32) return kVTParameterErr;
    const uint8_t *p[32]; size_t n[32];
    for (NSUInteger i = 0; i < sets.count; i++) {
        if (![sets[i] isKindOfClass:[NSData class]] || ![sets[i] length]) return kVTParameterErr;
        p[i] = [sets[i] bytes]; n[i] = [sets[i] length];
    }
    int nal = [d[@"nal"] intValue];
    CMVideoCodecType codec = [d[@"codec"] unsignedIntValue];
    if (codec == kCMVideoCodecType_H264) {
        OSStatus st = CMVideoFormatDescriptionCreateFromH264ParameterSets(NULL, sets.count, p, n, nal, fd);
        if (!st && [d[@"extensions"] count]) {
            CMVideoDimensions dims = CMVideoFormatDescriptionGetDimensions(*fd);
            NSMutableDictionary *extensions = [(__bridge NSDictionary *)CMFormatDescriptionGetExtensions(*fd) mutableCopy];
            [extensions addEntriesFromDictionary:d[@"extensions"]];
            CMVideoFormatDescriptionRef extended = NULL;
            st = CMVideoFormatDescriptionCreate(NULL, codec, dims.width, dims.height,
                     (__bridge CFDictionaryRef)extensions, &extended);
#if !__has_feature(objc_arc)
            [extensions release];
#endif
            CFRelease(*fd); *fd = extended;
        }
        return st;
    }
    if (codec == kCMVideoCodecType_HEVC)
        return CMVideoFormatDescriptionCreateFromHEVCParameterSets(NULL, sets.count, p, n, nal,
                   (__bridge CFDictionaryRef)d[@"extensions"], fd);
    return kVTParameterErr;
}
static NSDictionary *vt_sample(CMSampleBufferRef sb) {
    if (CMSampleBufferGetNumSamples(sb) != 1) return nil;
    CMBlockBufferRef bb = CMSampleBufferGetDataBuffer(sb);
    size_t size = bb ? CMBlockBufferGetDataLength(bb) : 0;
    if (!size) return nil;
    NSMutableData *data = [NSMutableData dataWithLength:size];
    if (CMBlockBufferCopyDataBytes(bb, 0, size, data.mutableBytes)) return nil;
    NSDictionary *fd = vt_format(CMSampleBufferGetFormatDescription(sb));
    if (!fd) return nil;
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sb, false);
    return @{@"data": data, @"format": fd,
             @"pts": vt_time(CMSampleBufferGetPresentationTimeStamp(sb)),
             @"dts": vt_time(CMSampleBufferGetDecodeTimeStamp(sb)),
             @"duration": vt_time(CMSampleBufferGetDuration(sb)),
             @"attachments": (__bridge NSArray *)attachments ?: @[]};
}
static OSStatus vt_unsample(NSDictionary *d, CMSampleBufferRef *sb) {
    CMVideoFormatDescriptionRef fd = NULL;
    OSStatus st = vt_unformat(d[@"format"], &fd);
    if (st) return st;
    NSData *data = d[@"data"]; size_t size = data.length;
    CMBlockBufferRef bb = NULL;
    st = size ? CMBlockBufferCreateWithMemoryBlock(NULL, NULL, size, NULL, NULL, 0, size, 0, &bb) : kVTParameterErr;
    if (!st) st = CMBlockBufferReplaceDataBytes(data.bytes, bb, 0, size);
    CMSampleTimingInfo timing = {vt_untime(d[@"duration"]), vt_untime(d[@"pts"]), vt_untime(d[@"dts"])};
    if (!st) st = CMSampleBufferCreateReady(NULL, bb, fd, 1, 1, &timing, 1, &size, sb);
    if (!st) {
        NSArray *a = d[@"attachments"];
        if (a.count) {
            CFArrayRef dest = CMSampleBufferGetSampleAttachmentsArray(*sb, true);
            if (CFArrayGetCount(dest)) {
                CFMutableDictionaryRef dict = (CFMutableDictionaryRef)CFArrayGetValueAtIndex(dest, 0);
                for (id key in a[0]) CFDictionarySetValue(dict, (__bridge CFTypeRef)key, (__bridge CFTypeRef)a[0][key]);
            }
        }
    }
    if (bb) CFRelease(bb); CFRelease(fd);
    return st;
}
