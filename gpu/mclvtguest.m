// Included by mclmetal.m (MRC). All CF reconstruction and client callbacks stay on guest threads.
#include "mclvtcommon.h"
@interface MCLGuestVT : NSObject {
@public
    uint64_t handle;
    BOOL encoder;
    _Atomic BOOL invalid;
    VTCompressionOutputCallback encCallback;
    VTDecompressionOutputCallback decCallback;
    void *refcon;
}
@end
static BOOL vt_enabled(void) {
#ifdef MCL_LOOPBACK
    if (mcl_in_host) return NO;
#endif
    const char *v = getenv("ATTESOR_GPU_VT");
    return v && !strcmp(v, "1");
}
static MCLGuestVT *vt_session(VTSessionRef ref) {
    return ref && object_getClass((id)ref) == [MCLGuestVT class] ? (MCLGuestVT *)ref : nil;
}
static NSDictionary *vt_rpc(NSDictionary *r) {
    NSDictionary *reply = mcl_op(MCL_OP_VT, r);
    return reply[@"status"] ? reply : @{@"status": @(kVTInvalidSessionErr)};
}
// Zeroing-weak registry: the pump may retain a live wrapper, but never keeps sessions alive
// indefinitely. CFRelease without Invalidate still destroys the host session.
static NSMapTable *gVTSessions;
static NSLock *gVTLock;
static void vt_register(MCLGuestVT *s) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{ gVTSessions = [[NSMapTable strongToWeakObjectsMapTable] retain]; gVTLock = [NSLock new]; });
    [gVTLock lock]; [gVTSessions setObject:s forKey:@(s->handle)]; [gVTLock unlock];
    start_pump();
}
static void vt_unregister(MCLGuestVT *s) {
    [gVTLock lock]; [gVTSessions removeObjectForKey:@(s->handle)]; [gVTLock unlock];
}
static void vt_barrier(NSDictionary *reply) {
    // A callback may reenter VT. Never wait for the pump to finish its own current callback.
    if (gInPump) return;
    uint64_t barrier = [reply[@"barrier"] unsignedLongLongValue];
    while (atomic_load(&gEvHandled) < barrier) usleep(50);
}
@implementation MCLGuestVT
- (CFTypeID)_cfTypeID { return encoder ? VTCompressionSessionGetTypeID() : VTDecompressionSessionGetTypeID(); }
- (void)dealloc {
    vt_unregister(self);
    if (!invalid && handle) vt_barrier(vt_rpc(@{@"op": @"destroy", @"handle": @(handle)}));
    [super dealloc];
}
@end
static void mcl_vt_event(NSDictionary *event) {
    [gVTLock lock]; MCLGuestVT *s = [[gVTSessions objectForKey:event[@"vt"]] retain]; [gVTLock unlock];
    if (!s) return; // Invalidated/released: host invalidation has released any held surfaces.
    NSDictionary *e = event[@"event"];
    OSStatus st = [e[@"status"] intValue]; uint32_t flags = [e[@"flags"] unsignedIntValue];
    void *frame = (void *)(uintptr_t)[e[@"frame"] unsignedLongLongValue];
    if (s->encoder) {
        CMSampleBufferRef sb = NULL;
        if (!st && [e[@"sample"] count]) st = vt_unsample(e[@"sample"], &sb);
        if (s->encCallback && !s->invalid) s->encCallback(s->refcon, frame, st, flags, sb);
        if (sb) CFRelease(sb);
    } else {
        CVPixelBufferRef pb = NULL; uint32_t sid = [e[@"surface"] unsignedIntValue];
        if (!st && sid) {
            IOSurfaceRef surface = IOSurfaceLookup(sid);
            st = surface ? CVPixelBufferCreateWithIOSurface(NULL, surface, NULL, &pb) : kVTParameterErr;
            if (surface) CFRelease(surface);
        }
        // Import retains the surface. Ack BEFORE the client callback, which may reenter/invalidate VT.
        if ([e[@"token"] unsignedLongLongValue] && !s->invalid)
            vt_rpc(@{@"op": @"ack", @"handle": @(s->handle), @"tokens": @[e[@"token"]]});
        if (s->decCallback && !s->invalid) s->decCallback(s->refcon, frame, st, flags, pb, vt_untime(e[@"pts"]), vt_untime(e[@"duration"]));
        if (pb) CFRelease(pb);
    }
    [s release];
}
static OSStatus vt_request(MCLGuestVT *s, NSDictionary *args, uint32_t *flags, CFTypeRef *value) {
    if (flags) *flags = 0;
    if (value) *value = NULL;
    if (s->invalid) return kVTInvalidSessionErr;
    NSMutableDictionary *r = [[args mutableCopy] autorelease]; r[@"handle"] = @(s->handle);
    [s retain];
    NSDictionary *reply = [vt_rpc(r) retain]; OSStatus status = [reply[@"status"] intValue];
    uint32_t info = [reply[@"flags"] unsignedIntValue];
    if (flags) *flags = info;
    if (value && reply[@"value"]) *value = CFRetain((CFTypeRef)reply[@"value"]);
    NSString *op = args[@"op"];
    if ([op isEqual:@"complete"] || [op isEqual:@"wait"] || [op isEqual:@"finish"] ||
        ([op isEqual:@"encode"] && !(info & kVTEncodeInfo_Asynchronous)) ||
        ([op isEqual:@"decode"] && !(info & kVTDecodeInfo_Asynchronous))) vt_barrier(reply);
    [reply release]; [s release]; return status;
}
static OSStatus mcl_VTCompressionSessionCreate(CFAllocatorRef allocator, int32_t width, int32_t height,
    CMVideoCodecType codec, CFDictionaryRef spec, CFDictionaryRef attrs, CFAllocatorRef compressedAllocator,
    VTCompressionOutputCallback callback, void *refcon, VTCompressionSessionRef *out) {
    if (!vt_enabled()) return VTCompressionSessionCreate(allocator, width, height, codec, spec, attrs,
                                                        compressedAllocator, callback, refcon, out);
    if (!out) return kVTParameterErr;
    *out = NULL;
    if (allocator || compressedAllocator || (codec != kCMVideoCodecType_H264 && codec != kCMVideoCodecType_HEVC)) return kVTParameterErr;
    NSDictionary *r = vt_rpc(@{@"op": @"create-enc", @"width": @(width), @"height": @(height),
        @"codec": @(codec), @"spec": (id)spec ?: @{}, @"attrs": (id)attrs ?: @{}});
    OSStatus st = [r[@"status"] intValue];
    if (st) return st;
    if (![r[@"handle"] unsignedLongLongValue]) return kVTInvalidSessionErr;
    MCLGuestVT *s = [MCLGuestVT new]; s->handle = [r[@"handle"] unsignedLongLongValue];
    s->encoder = YES; s->encCallback = callback; s->refcon = refcon;
    vt_register(s);
    *out = (VTCompressionSessionRef)s; return noErr;
}
static OSStatus mcl_VTDecompressionSessionCreate(CFAllocatorRef allocator, CMVideoFormatDescriptionRef format,
    CFDictionaryRef spec, CFDictionaryRef attrs, const VTDecompressionOutputCallbackRecord *callback,
    VTDecompressionSessionRef *out) {
    if (!vt_enabled()) return VTDecompressionSessionCreate(allocator, format, spec, attrs, callback, out);
    if (!out) return kVTParameterErr;
    *out = NULL;
    NSDictionary *fd = format ? vt_format(format) : nil;
    if (allocator || !fd) return kVTParameterErr;
    NSDictionary *r = vt_rpc(@{@"op": @"create-dec", @"format": fd, @"spec": (id)spec ?: @{}, @"attrs": (id)attrs ?: @{}});
    OSStatus st = [r[@"status"] intValue];
    if (st) return st;
    if (![r[@"handle"] unsignedLongLongValue]) return kVTInvalidSessionErr;
    MCLGuestVT *s = [MCLGuestVT new]; s->handle = [r[@"handle"] unsignedLongLongValue];
    if (callback) { s->decCallback = callback->decompressionOutputCallback; s->refcon = callback->decompressionOutputRefCon; }
    vt_register(s);
    *out = (VTDecompressionSessionRef)s; return noErr;
}
static OSStatus mcl_VTSessionSetProperty(VTSessionRef ref, CFStringRef key, CFTypeRef value) {
    MCLGuestVT *s = vt_session(ref);
    if (!s) return VTSessionSetProperty(ref, key, value);
    if (!key) return kVTParameterErr;
    return vt_request(s, @{@"op": @"set", @"key": (id)key, @"value": (id)value ?: @{}, @"reset": @(!value)}, NULL, NULL);
}
static OSStatus mcl_VTSessionSetProperties(VTSessionRef ref, CFDictionaryRef values) {
    MCLGuestVT *s = vt_session(ref);
    if (!s) return VTSessionSetProperties(ref, values);
    if (!values) return kVTParameterErr;
    for (id key in (NSDictionary *)values) {
        OSStatus st = mcl_VTSessionSetProperty(ref, (CFStringRef)key, (CFTypeRef)[(NSDictionary *)values objectForKey:key]);
        if (st) return st;
    }
    return noErr;
}
static OSStatus mcl_VTSessionCopyProperty(VTSessionRef ref, CFStringRef key, CFAllocatorRef allocator, CFTypeRef *out) {
    MCLGuestVT *s = vt_session(ref);
    if (!s) return VTSessionCopyProperty(ref, key, allocator, out);
    if (!key || !out || allocator) return kVTParameterErr;
    return vt_request(s, @{@"op": @"copy", @"key": (id)key}, NULL, out);
}
static OSStatus mcl_VTSessionCopySupportedPropertyDictionary(VTSessionRef ref, CFDictionaryRef *out) {
    MCLGuestVT *s = vt_session(ref);
    if (!s) return VTSessionCopySupportedPropertyDictionary(ref, out);
    if (!out) return kVTParameterErr;
    return vt_request(s, @{@"op": @"supported"}, NULL, (CFTypeRef *)out);
}
static OSStatus mcl_VTSessionCopySerializableProperties(VTSessionRef ref, CFAllocatorRef allocator, CFDictionaryRef *out) {
    MCLGuestVT *s = vt_session(ref);
    if (!s) return VTSessionCopySerializableProperties(ref, allocator, out);
    if (!out || allocator) return kVTParameterErr;
    return vt_request(s, @{@"op": @"serializable"}, NULL, (CFTypeRef *)out);
}
static OSStatus mcl_VTCompressionSessionPrepareToEncodeFrames(VTCompressionSessionRef ref) {
    MCLGuestVT *s = vt_session(ref);
    return s ? vt_request(s, @{@"op": @"prepare"}, NULL, NULL) : VTCompressionSessionPrepareToEncodeFrames(ref);
}
static OSStatus mcl_VTCompressionSessionEncodeFrame(VTCompressionSessionRef ref, CVImageBufferRef image,
    CMTime pts, CMTime duration, CFDictionaryRef props, void *frame, VTEncodeInfoFlags *flags) {
    MCLGuestVT *s = vt_session(ref);
    if (!s) return VTCompressionSessionEncodeFrame(ref, image, pts, duration, props, frame, flags);
    if (flags) *flags = 0;
    IOSurfaceRef surface = image ? CVPixelBufferGetIOSurface(image) : NULL;
    if (!surface) return kVTParameterErr; // No copying fallback that silently changes the sharing contract.
    return vt_request(s, @{@"op": @"encode", @"surface": @(IOSurfaceGetID(surface)), @"pts": vt_time(pts),
        @"duration": vt_time(duration), @"props": (id)props ?: @{}, @"frame": @((uintptr_t)frame)}, flags, NULL);
}
static OSStatus mcl_VTCompressionSessionCompleteFrames(VTCompressionSessionRef ref, CMTime until) {
    MCLGuestVT *s = vt_session(ref);
    return s ? vt_request(s, @{@"op": @"complete", @"until": vt_time(until)}, NULL, NULL) : VTCompressionSessionCompleteFrames(ref, until);
}
static OSStatus mcl_VTDecompressionSessionDecodeFrame(VTDecompressionSessionRef ref, CMSampleBufferRef sample,
    VTDecodeFrameFlags flags, void *frame, VTDecodeInfoFlags *info) {
    MCLGuestVT *s = vt_session(ref);
    if (!s) return VTDecompressionSessionDecodeFrame(ref, sample, flags, frame, info);
    if (info) *info = 0;
    NSDictionary *data = sample ? vt_sample(sample) : nil;
    if (!data) return kVTParameterErr;
    return vt_request(s, @{@"op": @"decode", @"sample": data, @"flags": @(flags), @"frame": @((uintptr_t)frame)}, info, NULL);
}
static OSStatus mcl_VTDecompressionSessionWaitForAsynchronousFrames(VTDecompressionSessionRef ref) {
    MCLGuestVT *s = vt_session(ref);
    return s ? vt_request(s, @{@"op": @"wait"}, NULL, NULL) : VTDecompressionSessionWaitForAsynchronousFrames(ref);
}
static OSStatus mcl_VTDecompressionSessionFinishDelayedFrames(VTDecompressionSessionRef ref) {
    MCLGuestVT *s = vt_session(ref);
    return s ? vt_request(s, @{@"op": @"finish"}, NULL, NULL) : VTDecompressionSessionFinishDelayedFrames(ref);
}
static Boolean mcl_VTDecompressionSessionCanAcceptFormatDescription(VTDecompressionSessionRef ref, CMFormatDescriptionRef fd) {
    MCLGuestVT *s = vt_session(ref);
    if (!s) return VTDecompressionSessionCanAcceptFormatDescription(ref, fd);
    NSDictionary *d = vt_format(fd);
    if (!d || s->invalid) return false;
    return [vt_rpc(@{@"op": @"accept", @"handle": @(s->handle), @"format": d})[@"accepts"] boolValue];
}
static void vt_invalidate(MCLGuestVT *s) {
    if (!s->invalid) {
        s->invalid = YES;
        vt_unregister(s);
        vt_barrier(vt_rpc(@{@"op": @"destroy", @"handle": @(s->handle)}));
    }
}
static void mcl_VTCompressionSessionInvalidate(VTCompressionSessionRef ref) {
    MCLGuestVT *s = vt_session(ref); if (s) vt_invalidate(s); else VTCompressionSessionInvalidate(ref);
}
static void mcl_VTDecompressionSessionInvalidate(VTDecompressionSessionRef ref) {
    MCLGuestVT *s = vt_session(ref); if (s) vt_invalidate(s); else VTDecompressionSessionInvalidate(ref);
}
// A host CVPixelBufferPool cannot be exposed as a guest CF object.
static CVPixelBufferPoolRef mcl_VTCompressionSessionGetPixelBufferPool(VTCompressionSessionRef ref) {
    return vt_session(ref) ? NULL : VTCompressionSessionGetPixelBufferPool(ref);
}
#ifndef MCL_LOOPBACK
#define VT_INTERPOSE(name) {(const void *)mcl_##name, (const void *)name}
__attribute__((used)) static const struct { const void *replacement, *replacee; }
    mcl_vt_interposers[] __attribute__((section("__DATA,__interpose"))) = {
    VT_INTERPOSE(VTCompressionSessionCreate), VT_INTERPOSE(VTDecompressionSessionCreate),
    VT_INTERPOSE(VTSessionSetProperty), VT_INTERPOSE(VTSessionSetProperties), VT_INTERPOSE(VTSessionCopyProperty),
    VT_INTERPOSE(VTSessionCopySupportedPropertyDictionary), VT_INTERPOSE(VTSessionCopySerializableProperties),
    VT_INTERPOSE(VTCompressionSessionPrepareToEncodeFrames), VT_INTERPOSE(VTCompressionSessionEncodeFrame),
    VT_INTERPOSE(VTCompressionSessionCompleteFrames), VT_INTERPOSE(VTCompressionSessionInvalidate),
    VT_INTERPOSE(VTCompressionSessionGetPixelBufferPool), VT_INTERPOSE(VTDecompressionSessionDecodeFrame),
    VT_INTERPOSE(VTDecompressionSessionWaitForAsynchronousFrames), VT_INTERPOSE(VTDecompressionSessionFinishDelayedFrames),
    VT_INTERPOSE(VTDecompressionSessionCanAcceptFormatDescription), VT_INTERPOSE(VTDecompressionSessionInvalidate),
};
#undef VT_INTERPOSE
#endif
