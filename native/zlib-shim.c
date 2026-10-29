// arm64 guest shim: stateless zlib calls execute in the x86-64 host libz.
// Never transfer z_stream: its callbacks, allocator and internal state belong
// to the guest. All bridged APIs below have matching LP64 C layouts.
#include <stdint.h>
#include <zlib.h>
_Static_assert(sizeof(uLong) == 8 && sizeof(uLongf) == 8 && sizeof(uInt) == 4, "bridge requires Darwin LP64 zlib ABI");

static uint64_t hostcall(uint64_t op, uint64_t a, uint64_t b, uint64_t c, uint64_t d, uint64_t e) {
    register uint64_t x0 __asm__("x0") = op;
    register uint64_t x1 __asm__("x1") = a;
    register uint64_t x2 __asm__("x2") = b;
    register uint64_t x3 __asm__("x3") = c;
    register uint64_t x4 __asm__("x4") = d;
    register uint64_t x5 __asm__("x5") = e;
    register uint64_t x16 __asm__("x16") = 0x4154;
    __asm__ volatile("svc #0x80" : "+r"(x0), "+r"(x1) : "r"(x2), "r"(x3), "r"(x4), "r"(x5), "r"(x16) : "memory", "cc");
    return x0;
}
// Diagnostic counter is served by the host, proving that interposition reached
// native code rather than merely comparing two copies of guest execution.
__attribute__((visibility("default"))) uint64_t attesor_native_calls(void) {
    return hostcall(0, 0, 0, 0, 0, 0);
}
static uLong native_crc32(uLong crc, const Bytef *buf, uInt len) {
    return hostcall(1, crc, (uintptr_t)buf, len, 0, 0);
}
static uLong native_adler32(uLong adler, const Bytef *buf, uInt len) {
    return hostcall(2, adler, (uintptr_t)buf, len, 0, 0);
}
static int native_compress2(Bytef *dest, uLongf *destlen, const Bytef *source, uLong sourcelen, int level) {
    return (int)hostcall(3, (uintptr_t)dest, (uintptr_t)destlen, (uintptr_t)source, sourcelen, (uint64_t)(int64_t)level);
}
static int native_uncompress(Bytef *dest, uLongf *destlen, const Bytef *source, uLong sourcelen) {
    return (int)hostcall(4, (uintptr_t)dest, (uintptr_t)destlen, (uintptr_t)source, sourcelen, 0);
}
struct interpose { const void *replacement, *replacee; };
__attribute__((used)) static const struct interpose hooks[] __attribute__((section("__DATA,__interpose"))) = {
    {(const void *)native_crc32, (const void *)crc32},
    {(const void *)native_adler32, (const void *)adler32},
    {(const void *)native_compress2, (const void *)compress2},
    {(const void *)native_uncompress, (const void *)uncompress},
};
