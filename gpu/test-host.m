// CPU-only production host dispatch tests; fake NSObject targets, no Metal device/VT sessions.
#include "mclbridge.m"
#include <assert.h>
@interface TestTarget : NSObject { @public uint64_t value; }
- (id)commandBuffer;
- (void)setBytes:(const void *)bytes length:(NSUInteger)n;
- (uint64_t)value;
@end
@implementation TestTarget
- (id)commandBuffer { return self; }
- (void)setBytes:(const void *)bytes length:(NSUInteger)n {
    value = 0; for (NSUInteger i = 0; i < n; i++) value += ((const uint8_t *)bytes)[i];
}
- (uint64_t)value { return value; }
@end
int main(void) { @autoreleasepool {
    ensureInit(); uint64_t outlen;
    MclCaps *caps = mcl_call(MCL_OP_CAPS, NULL, 0, &outlen);
    assert(outlen == sizeof *caps && caps->version == 2 && (caps->capabilities & MCL_CAP_VT));
    TestTarget *target = [TestTarget new]; uint64_t h = handleFor(target, NO), asyncH = MCL_GUEST_HANDLE_BASE;
    defSel(1, "commandBuffer"); defSel(2, "setBytes:length:"); defSel(3, "value");
    uint8_t data[65536]; uint64_t sum = 0;
    for (size_t i = 0; i < sizeof data; i++) { data[i] = i; sum += data[i]; }
    // CREATE, shared upload, synchronous getter: one batch, including cross-handle dependency.
    uint64_t wire[] = {
        0,0,
        32 | ((uint64_t)MCL_R_CREATE << 32), 1, h, asyncH,
        72 | ((uint64_t)MCL_R_MSG << 32) | (2ull << 40), 2, asyncH, 0,
        MCL_A_SHARED | (16ull << 32), (uintptr_t)data, sizeof data,
        MCL_A_SCALAR | (8ull << 32), sizeof data,
        32 | ((uint64_t)MCL_R_SYNC << 32), 3, asyncH, 0,
    };
    MclMsgHdr *head = (MclMsgHdr *)wire;
    *head = (MclMsgHdr){MCL_MAGIC, sizeof wire - 16, 0, MCL_VERSION};
    assert(mcl_batch_valid(wire, sizeof wire));
    MclReply *reply = mcl_call(MCL_OP_BATCH, wire, sizeof wire, &outlen);
    assert(reply && outlen == sizeof *reply && reply->tag == MCL_T_SCALAR && reply->value == sum);
    assert(target->value == sum);
    // Reject bad versions/lengths/argument tags BEFORE executing the first record.
    uint64_t original = wire[6]; target->value = 99;
    head->version = 99; assert(!mcl_batch_valid(wire, sizeof wire)); head->version = MCL_VERSION;
    wire[6] = (wire[6] & ~0xffffffffull) | 40; assert(!mcl_batch_valid(wire, sizeof wire)); wire[6] = original;
    uint64_t arg = wire[10]; wire[10] = MCL_A_SHARED | (8ull << 32);
    assert(!mcl_call(MCL_OP_BATCH, wire, sizeof wire, &outlen) && target->value == 99); wire[10] = arg;
    head->version = 0; assert(!mcl_batch_valid(wire, sizeof wire)); head->version = MCL_VERSION;
    for (size_t n = 0; n < sizeof wire; n++) assert(!mcl_batch_valid(wire, n));
    // Opt-in must be exact; even a valid request cannot accidentally create a VT session.
    unsetenv("ATTESOR_GPU_VT");
    assert([mcl_vt_handle(@{@"op": @"create-enc"})[@"status"] intValue] == kVTInvalidSessionErr);
    setenv("ATTESOR_GPU_VT", "0", 1);
    assert([mcl_vt_handle(@{@"op": @"create-dec"})[@"status"] intValue] == kVTInvalidSessionErr);
    setenv("ATTESOR_GPU_VT", "1", 1);
    assert([mcl_vt_handle(@{@"op": @"wait", @"handle": @123})[@"status"] intValue] == kVTInvalidSessionErr);
    dropHandle(asyncH, 1); dropHandle(h, 1);
    puts("PASS production host: v2 negotiation, batched handles/order, shared upload, malformed envelopes, VT opt-in");
} return 0; }
