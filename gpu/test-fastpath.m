// CPU-only guest unit test: production plan builder/dispatcher/queue, a recording transport.
// Run the arm64 binary through attesor WITHOUT --gpu. No Metal or VT session is created.
#define MCL_LOOPBACK 1
#include "mclmetal.m"
#include <assert.h>
static unsigned batches, records, shared, creates, drops, vtCalls, vtDestroyed;
static NSCondition *fakeEventCondition;
static NSMutableArray *fakeEvents;
static uint64_t fakePushed;
static uint64_t expectedTarget;
static BOOL checkTarget;
static MclReply fakeReply = {MCL_T_VOID, 0, 0, 0, 0};
void *mcl_call(uint64_t op, const void *in, uint64_t n, uint64_t *out) {
    *out = 0;
    if (op == MCL_OP_CAPS) {
        static MclCaps caps = {MCL_VERSION, MCL_CAP_SHARED_UPLOAD};
        *out = sizeof caps; return &caps;
    }
    if (op == MCL_OP_EVENT) {
        [fakeEventCondition lock];
        while (!fakeEvents.count) [fakeEventCondition wait];
        NSDictionary *event = [[fakeEvents[0] retain] autorelease]; [fakeEvents removeObjectAtIndex:0];
        [fakeEventCondition unlock];
        NSData *data = [NSPropertyListSerialization dataWithPropertyList:event format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
        void *p = malloc(data.length); memcpy(p, data.bytes, data.length); *out = data.length; return p;
    }
    if (op == MCL_OP_FREE) { free((void *)in); return NULL; }
    if (op == MCL_OP_VT) {
        vtCalls++;
        NSData *d = [NSData dataWithBytes:in length:n];
        NSDictionary *r = [NSPropertyListSerialization propertyListWithData:d options:0 format:NULL error:NULL];
        NSDictionary *reply = @{@"status": @0};
        NSString *verb = r[@"op"];
        if ([verb hasPrefix:@"create-"]) reply = @{@"status": @0, @"handle": @99};
        if ([verb isEqual:@"copy"]) reply = @{@"status": @0, @"value": @YES};
        if ([verb isEqual:@"complete"]) {
            [fakeEventCondition lock];
            [fakeEvents addObject:@{@"vt": @99, @"event": @{@"status": @(kVTParameterErr),
                @"flags": @2, @"frame": @123, @"sample": @{}}}];
            reply = @{@"status": @0, @"barrier": @(++fakePushed)};
            [fakeEventCondition signal]; [fakeEventCondition unlock];
        }
        if ([verb isEqual:@"destroy"]) vtDestroyed++;
        NSData *outdata = [NSPropertyListSerialization dataWithPropertyList:reply format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
        void *p = malloc(outdata.length); memcpy(p, outdata.bytes, outdata.length); *out = outdata.length; return p;
    }
    if (op == MCL_OP_DEFSEL) return NULL;
    assert(op == MCL_OP_BATCH);
    const MclMsgHdr *h = in;
    assert(h->version == MCL_VERSION && h->cmdbytes + sizeof *h == n);
    batches++;
    const uint8_t *p = (const uint8_t *)in + sizeof *h, *end = (const uint8_t *)in + n;
    while (p < end) {
        const uint64_t *w = (const uint64_t *)p;
        uint32_t len = (uint32_t)w[0]; uint8_t kind = w[0] >> 32, nargs = w[0] >> 40;
        assert(len >= 32 && p + len <= end); records++;
        if (kind == MCL_R_CREATE) { creates++; expectedTarget = w[3]; }
        if (kind == MCL_R_DROP) { drops++; assert(w[2] == expectedTarget); }
        if (checkTarget && kind == MCL_R_MSG && !nargs) assert(w[2] == expectedTarget || w[2] == 1);
        const uint8_t *ap = p + 32;
        for (unsigned i = 0; i < nargs; i++) {
            uint64_t ah; memcpy(&ah, ap, 8); uint32_t alen = ah >> 32;
            if ((uint8_t)ah == MCL_A_SHARED) {
                MclSharedData d; assert(alen == sizeof d); memcpy(&d, ap + 8, sizeof d);
                assert(d.length == 65536);
                const uint8_t *bytes = (const uint8_t *)(uintptr_t)d.address;
                for (size_t k = 0; k < d.length; k++) assert(bytes[k] == (uint8_t)k);
                shared++;
            }
            ap += 8 + ((alen + 7) & ~7u);
        }
        p += len;
    }
    *out = sizeof fakeReply; return &fakeReply;
}
static void invoke(MCLProxy *p, SEL sel, uint64_t arg0, uint64_t arg1) {
    uint64_t r[16] = {(uintptr_t)p, (uintptr_t)sel, arg0, arg1};
    (void)mcl_fast_dispatch(r, NULL);
}
static void *other_thread(void *p) {
    invoke(p, @selector(endEncoding), 0, 0); return NULL;
}
static unsigned vtCallbacks;
static void encoded_test(void *ref, void *frame, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sb) {
    assert(ref && frame == (void *)123 && status == kVTParameterErr && flags == 2 && !sb);
    VTCompressionSessionRef session = *(VTCompressionSessionRef *)ref;
    CFTypeRef value = NULL;
    assert(!mcl_VTSessionCopyProperty(session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, NULL, &value));
    assert(value == kCFBooleanTrue); CFRelease(value); // Reentrant guest callback, not a host call into guest CF.
    vtCallbacks++;
}
int main(void) { @autoreleasepool {
    fakeEventCondition = [NSCondition new]; fakeEvents = [NSMutableArray new];
    mcl_init(); mcl_require_v2();
    sigEncodingFor(@selector(commandBuffer), "@@:", @"Test");
    fastNoteResultClass(@selector(commandBuffer), 1);
    sigEncodingFor(@selector(endEncoding), "v@:", @"Test");
    sigEncodingFor(@selector(commit), "v@:", @"Test");
    sigEncodingFor(@selector(setVertexBytes:length:atIndex:), "v@:r^vQQ", @"Test");
    sigEncodingFor(@selector(waitUntilCompleted), "v@:", @"Test");
    MCLProxy *root = [[MCLProxy alloc] initWithHandle:1 info:@{@"c": @[@"Test"], @"p": @[]}];
    uint64_t r[16] = {(uintptr_t)root, (uintptr_t)(@selector(commandBuffer))};
    MCLProxy *cb = (MCLProxy *)mcl_fast_dispatch(r, NULL);
    assert(cb && batches == 0); // Sensitivity: old sync=true flushed this CREATE immediately.
    checkTarget = YES;
    pthread_t thread; assert(!pthread_create(&thread, NULL, other_thread, cb)); pthread_join(thread, NULL);
    assert(batches == 0);
    uint8_t data[65536]; for (size_t k = 0; k < sizeof data; k++) data[k] = k;
    invoke(root, @selector(setVertexBytes:length:atIndex:), (uintptr_t)data, sizeof data);
    memset(data, 0xcd, sizeof data); // Caller owns/reuses its memory before the host sees the upload.
    assert(batches == 0);
    invoke(root, @selector(commit), 0, 0);
    assert(batches == 1 && records == 4 && creates == 1 && shared == 1 && G.uploads == NULL);
    tq_drop(expectedTarget);
    invoke(root, @selector(waitUntilCompleted), 0, 0);
    assert(batches == 3 && drops == 1);
    gNoAsync = YES;
    fakeReply.tag = MCL_T_NIL;
    r[0] = (uintptr_t)root; r[1] = (uintptr_t)(@selector(commandBuffer));
    assert(!mcl_fast_dispatch(r, NULL) && batches == 4);
    // NOBATCH must also disable asynchronous creates, not silently discard their sync reply.
    gNoAsync = NO; gNoBatch = YES;
    assert(!mcl_fast_dispatch(r, NULL) && batches == 5);
    // Large synchronous input is borrowed (no extra snapshot/copy), but still uses v2 shared records.
    for (size_t k = 0; k < sizeof data; k++) data[k] = k;
    invoke(root, @selector(setVertexBytes:length:atIndex:), (uintptr_t)data, sizeof data);
    assert(shared == 2 && G.uploads == NULL && gNUploadBorrowed == sizeof data);
    // Exercise the real guest VT wrapper/CF lifetime and callback reconstruction with a recording peer.
    gNoBatch = NO;
    setenv("ATTESOR_GPU_VT", "1", 1);
    VTCompressionSessionRef session = NULL;
    assert(!mcl_VTCompressionSessionCreate(NULL, 64, 32, kCMVideoCodecType_H264, NULL, NULL, NULL, encoded_test, &session, &session));
    assert(session && CFGetTypeID(session) == VTCompressionSessionGetTypeID());
    CFRetain(session); CFRelease(session);
    assert(!mcl_VTSessionSetProperty(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue));
    assert(!mcl_VTSessionSetProperty(session, kVTCompressionPropertyKey_RealTime, NULL));
    unsigned calls = vtCalls;
    assert(mcl_VTCompressionSessionEncodeFrame(session, NULL, kCMTimeZero, kCMTimeInvalid, NULL, NULL, NULL) == kVTParameterErr && vtCalls == calls);
    assert(!mcl_VTCompressionSessionCompleteFrames(session, kCMTimeInvalid) && vtCallbacks == 1);
    mcl_VTCompressionSessionInvalidate(session); mcl_VTCompressionSessionInvalidate(session);
    assert(vtDestroyed == 1);
    assert(mcl_VTCompressionSessionPrepareToEncodeFrames(session) == kVTInvalidSessionErr);
    CFRelease(session); assert(vtDestroyed == 1);
    // Release without Invalidate must tear down the host session, too.
    assert(!mcl_VTCompressionSessionCreate(NULL, 64, 32, kCMVideoCodecType_H264, NULL, NULL, NULL, NULL, NULL, &session));
    CFRelease(session); assert(vtDestroyed == 2);
    puts("PASS guest VT: opt-in, property/reset, CF type/lifetime, async pump/barrier/refcon/frame/error/flags/reentrancy, invalidate");
    puts("PASS production fastpath: async batching, cross-thread order, shared snapshot/reuse, drops, waits, toggles");
} return 0; }
