// Included by mclbridge.m (ARC). Only bridge threads enter host VT.
#include "mclvtcommon.h"
@interface MCLHostVT : NSObject {
@public
    VTCompressionSessionRef enc;
    VTDecompressionSessionRef dec;
    NSLock *eventsLock;
    NSRecursiveLock *operationLock;
    uint64_t handle;
    NSMutableDictionary *heldImages;
    uint64_t nextImage;
}
@end
@implementation MCLHostVT
- (instancetype)init {
    if ((self = [super init])) {
        eventsLock = [NSLock new]; operationLock = [NSRecursiveLock new];
        heldImages = [NSMutableDictionary new];
    }
    return self;
}
- (void)dealloc {
    if (enc) { VTCompressionSessionInvalidate(enc); CFRelease(enc); }
    if (dec) { VTDecompressionSessionInvalidate(dec); CFRelease(dec); }
}
@end
static void vt_encoded(void *ref, void *frame, OSStatus st, VTEncodeInfoFlags flags, CMSampleBufferRef sb) {
    MCLHostVT *s = (__bridge MCLHostVT *)ref;
    NSDictionary *sample = !st && sb ? vt_sample(sb) : nil;
    if (!st && sb && !sample) st = kVTParameterErr;
    [s->eventsLock lock];
    pushEvent(@{@"vt": @(s->handle), @"event": @{@"status": @(st), @"flags": @(flags),
                @"frame": @((uintptr_t)frame), @"sample": sample ?: @{}}});
    [s->eventsLock unlock];
}
static void vt_decoded(void *ref, void *frame, OSStatus st, VTDecodeInfoFlags flags,
                       CVImageBufferRef image, CMTime pts, CMTime duration) {
    MCLHostVT *s = (__bridge MCLHostVT *)ref;
    IOSurfaceRef surface = image ? CVPixelBufferGetIOSurface(image) : NULL;
    if (!st && image && !surface) st = kVTParameterErr;
    [s->eventsLock lock];
    uint64_t token = 0;
    if (!st && surface) {
        token = ++s->nextImage;
        // Retain the host CVPixelBuffer until the guest imports its IOSurface and acks.
        s->heldImages[@(token)] = (__bridge id)image;
    }
    pushEvent(@{@"vt": @(s->handle), @"event": @{@"status": @(st), @"flags": @(flags),
                @"frame": @((uintptr_t)frame), @"surface": @(surface && !st ? IOSurfaceGetID(surface) : 0),
                @"token": @(token), @"pts": vt_time(pts), @"duration": vt_time(duration)}});
    [s->eventsLock unlock];
}
static NSDictionary *mcl_vt_handle(NSDictionary *r) {
    if (!getenv("ATTESOR_GPU_VT") || strcmp(getenv("ATTESOR_GPU_VT"), "1"))
        return @{@"status": @(kVTInvalidSessionErr)};
    static NSMutableDictionary *sessions;
    static NSLock *lock;
    static uint64_t next;
    static dispatch_once_t once;
    dispatch_once(&once, ^{ sessions = [NSMutableDictionary new]; lock = [NSLock new]; });
    NSString *op = r[@"op"];
    if ([op isEqual:@"create-enc"] || [op isEqual:@"create-dec"]) {
        MCLHostVT *s = [MCLHostVT new];
        OSStatus st;
        if ([op isEqual:@"create-enc"]) {
            NSMutableDictionary *spec = [r[@"spec"] mutableCopy] ?: [NSMutableDictionary new];
            spec[(__bridge id)kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder] = @YES;
            spec[(__bridge id)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder] = @YES;
            st = VTCompressionSessionCreate(NULL, [r[@"width"] intValue], [r[@"height"] intValue],
                 [r[@"codec"] unsignedIntValue], (__bridge CFDictionaryRef)spec,
                 (__bridge CFDictionaryRef)r[@"attrs"], NULL, vt_encoded, (__bridge void *)s, &s->enc);
        } else {
            CMVideoFormatDescriptionRef fd = NULL;
            st = vt_unformat(r[@"format"], &fd);
            if (!st) {
                NSMutableDictionary *attrs = [r[@"attrs"] mutableCopy] ?: [NSMutableDictionary new];
                attrs[(__bridge id)kCVPixelBufferIOSurfacePropertiesKey] = @{};
                NSMutableDictionary *spec = [r[@"spec"] mutableCopy] ?: [NSMutableDictionary new];
                spec[(__bridge id)kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder] = @YES;
                spec[(__bridge id)kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder] = @YES;
                VTDecompressionOutputCallbackRecord cb = {vt_decoded, (__bridge void *)s};
                st = VTDecompressionSessionCreate(NULL, fd, (__bridge CFDictionaryRef)spec,
                     (__bridge CFDictionaryRef)attrs, &cb, &s->dec);
                CFRelease(fd);
            }
        }
        if (st) return @{@"status": @(st)};
        [lock lock]; uint64_t handle = ++next; s->handle = handle; sessions[@(handle)] = s; [lock unlock];
        return @{@"status": @0, @"handle": @(handle)};
    }
    [lock lock]; MCLHostVT *s = sessions[r[@"handle"]]; [lock unlock];
    if (!s) return @{@"status": @(kVTInvalidSessionErr)};
    [s->operationLock lock];
    @try {
        OSStatus st = noErr; uint32_t flags = 0; CFTypeRef value = NULL;
        VTSessionRef session = s->enc ? (VTSessionRef)s->enc : (VTSessionRef)s->dec;
        if ([op isEqual:@"destroy"]) {
            // Invalidate before removing the last owner: callbacks must finish while refcon lives.
            if (s->enc) VTCompressionSessionInvalidate(s->enc);
            if (s->dec) VTDecompressionSessionInvalidate(s->dec);
            [lock lock]; [sessions removeObjectForKey:r[@"handle"]]; [lock unlock];
        } else if ([op isEqual:@"set"]) {
            st = VTSessionSetProperty(session, (__bridge CFStringRef)r[@"key"], [r[@"reset"] boolValue] ? NULL : (__bridge CFTypeRef)r[@"value"]);
        } else if ([op isEqual:@"copy"]) {
            st = VTSessionCopyProperty(session, (__bridge CFStringRef)r[@"key"], NULL, &value);
        } else if ([op isEqual:@"supported"]) {
            CFDictionaryRef dict = NULL;
            st = VTSessionCopySupportedPropertyDictionary(session, &dict); value = dict;
        } else if ([op isEqual:@"serializable"]) {
            CFDictionaryRef dict = NULL;
            st = VTSessionCopySerializableProperties(session, NULL, &dict); value = dict;
        } else if ([op isEqual:@"prepare"] && s->enc) {
            st = VTCompressionSessionPrepareToEncodeFrames(s->enc);
        } else if ([op isEqual:@"encode"] && s->enc) {
            IOSurfaceRef surface = IOSurfaceLookup([r[@"surface"] unsignedIntValue]);
            CVPixelBufferRef pb = NULL;
            st = surface ? CVPixelBufferCreateWithIOSurface(NULL, surface, NULL, &pb) : kVTParameterErr;
            if (surface) CFRelease(surface);
            if (!st) st = VTCompressionSessionEncodeFrame(s->enc, pb, vt_untime(r[@"pts"]), vt_untime(r[@"duration"]),
                 (__bridge CFDictionaryRef)r[@"props"], (void *)(uintptr_t)[r[@"frame"] unsignedLongLongValue], &flags);
            if (pb) CFRelease(pb);
        } else if ([op isEqual:@"complete"] && s->enc) {
            st = VTCompressionSessionCompleteFrames(s->enc, vt_untime(r[@"until"]));
        } else if ([op isEqual:@"decode"] && s->dec) {
            CMSampleBufferRef sb = NULL;
            st = vt_unsample(r[@"sample"], &sb);
            if (!st) st = VTDecompressionSessionDecodeFrame(s->dec, sb, [r[@"flags"] unsignedIntValue],
                 (void *)(uintptr_t)[r[@"frame"] unsignedLongLongValue], &flags);
            if (sb) CFRelease(sb);
        } else if ([op isEqual:@"wait"] && s->dec) {
            st = VTDecompressionSessionWaitForAsynchronousFrames(s->dec);
        } else if ([op isEqual:@"finish"] && s->dec) {
            st = VTDecompressionSessionFinishDelayedFrames(s->dec);
        } else if ([op isEqual:@"accept"] && s->dec) {
            CMVideoFormatDescriptionRef fd = NULL;
            st = vt_unformat(r[@"format"], &fd);
            BOOL accepts = !st && VTDecompressionSessionCanAcceptFormatDescription(s->dec, fd);
            if (fd) CFRelease(fd);
            return @{@"status": @(st), @"accepts": @(accepts)};
        } else if ([op isEqual:@"ack"]) {
            [s->eventsLock lock];
            for (NSNumber *token in r[@"tokens"]) [s->heldImages removeObjectForKey:token];
            [s->eventsLock unlock];
            return @{@"status": @0};
        } else return @{@"status": @(kVTParameterErr)};
        NSMutableDictionary *reply = [@{@"status": @(st), @"flags": @(flags), @"barrier": @(gEvPushed)} mutableCopy];
        if (value) { reply[@"value"] = CFBridgingRelease(value); }
        return reply;
    } @finally { [s->operationLock unlock]; }
}
