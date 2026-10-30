// Private arm64 shim ABI. The host never invokes guest callbacks.
#include <stdint.h>
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
