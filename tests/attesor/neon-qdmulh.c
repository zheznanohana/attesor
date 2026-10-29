// Fixed-point high multiply correctness/QC probe and optional throughput workload.
// Build for both architectures with clang -O2 -ffp-contract=off -arch ARCH.
// Compare stdout (default). --bench N performs N dependent Q32 operations and
// prints a checksum, leaving timing to the serial acceptance coordinator.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>
typedef uint64_t V __attribute__((vector_size(16)));
typedef union { V v; int16_t h[8]; int32_t s[4]; uint64_t d[2]; } Vec;

static uint32_t status;
static void set_status(uint32_t v) {
#ifdef __aarch64__
    uint64_t x = v; __asm__ volatile("msr fpsr, %0" :: "r"(x) : "memory");
#else
    status = v;
#endif
}
static uint32_t get_status(void) {
#ifdef __aarch64__
    uint64_t v; __asm__ volatile("mrs %0, fpsr" : "=r"(v) :: "memory"); return (uint32_t)v;
#else
    return status;
#endif
}

// mode bits: bit0 round, bit1 32-bit lanes, bit2 128-bit vector.
__attribute__((noinline)) static V operation(V a, V b, unsigned mode, unsigned alias) {
#ifdef __aarch64__
    V r;
#define OP(NAME, LANES) do { \
    if (alias == 1) { r = a; __asm__ volatile(NAME " %0." LANES ", %0." LANES ", %1." LANES : "+w"(r) : "w"(b)); } \
    else if (alias == 2) { r = b; __asm__ volatile(NAME " %0." LANES ", %1." LANES ", %0." LANES : "+w"(r) : "w"(a)); } \
    else { __asm__ volatile(NAME " %0." LANES ", %1." LANES ", %2." LANES : "=w"(r) : "w"(a), "w"(b)); } \
} while (0)
    switch (mode) {
    case 0: OP("sqdmulh", "4h"); break;
    case 1: OP("sqrdmulh", "4h"); break;
    case 2: OP("sqdmulh", "2s"); break;
    case 3: OP("sqrdmulh", "2s"); break;
    case 4: OP("sqdmulh", "8h"); break;
    case 5: OP("sqrdmulh", "8h"); break;
    case 6: OP("sqdmulh", "4s"); break;
    default: OP("sqrdmulh", "4s"); break;
    }
#undef OP
    return r;
#else
    (void)alias;
    Vec x = {.v=a}, y = {.v=b}, r = {.d={0,0}};
    unsigned bits = mode & 2 ? 32 : 16, lanes = (mode & 4 ? 128 : 64) / bits;
    int64_t top = ((int64_t)1 << (bits - 1)) - 1;
    for (unsigned i = 0; i < lanes; i++) {
        int64_t p = bits == 32 ? (int64_t)x.s[i] * y.s[i] : (int64_t)x.h[i] * y.h[i];
        int64_t v = (p + (mode & 1 ? (int64_t)1 << (bits - 2) : 0)) >> (bits - 1);
        if (v > top) { v = top; status |= 1u << 27; }
        if (bits == 32) r.s[i] = (int32_t)v; else r.h[i] = (int16_t)v;
    }
    return r.v;
#endif
}

static uint64_t rng = 0x123456789abcdefULL;
static uint32_t random32(void) { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; return (uint32_t)rng; }
int main(int argc, char **argv) {
    uint32_t original = get_status();
    Vec a = {.s={INT_MIN, INT_MAX, -123456789, 123456789}}, b = {.s={INT_MIN, INT_MIN, INT_MAX, -987654321}}, r;
    if (argc == 3 && !strcmp(argv[1], "--bench")) {
        unsigned long count = strtoul(argv[2], NULL, 10);
        for (unsigned long i = 0; i < count; i++) {
            a.v = operation(a.v, b.v, 7, 1);
            // Keep values varying, avoid a degenerate all-zero recurrence.
            a.d[0] ^= 0x13579bdf2468ace0ULL; a.d[1] ^= 0xfedcba9876543210ULL;
        }
        printf("checksum %016llx %016llx\n", (unsigned long long)a.d[0], (unsigned long long)a.d[1]);
        set_status(original); return 0;
    }
    uint64_t digest = 1469598103934665603ULL;
    unsigned qc_cases = 0;
    for (unsigned mode = 0; mode < 8; mode++) for (unsigned alias = 0; alias < 3; alias++) {
        for (unsigned it = 0; it < 1200; it++) {
            for (unsigned i = 0; i < 4; i++) { a.s[i] = (int32_t)random32(); b.s[i] = (int32_t)random32(); }
            if (it % 4 == 0) {
                if (mode & 2) { a.s[(it / 4) % 4] = INT_MIN; b.s[(it / 4) % 4] = INT_MIN; }
                else { a.h[(it / 4) % 8] = INT16_MIN; b.h[(it / 4) % 8] = INT16_MIN; }
            }
            if (it % 7 == 0) { a.d[0] = a.d[1] = b.d[0] = b.d[1] = UINT64_MAX; }
            uint32_t initial = it % 3 == 0 ? (1u << 27) | 0x12 : 0x12;
            set_status(initial);
            r.v = operation(a.v, b.v, mode, alias);
            uint32_t qc = get_status();
            if (qc & (1u << 27)) qc_cases++;
            digest = (digest ^ r.d[0]) * 1099511628211ULL;
            digest = (digest ^ r.d[1]) * 1099511628211ULL;
            digest = (digest ^ qc) * 1099511628211ULL;
        }
    }
    // Hand-computed overflow case: (-2^31)*(-2^31)*2 / 2^32
    // is 2^31 before saturation, therefore INT_MAX and sticky QC=1.
    a = (Vec){.s={INT_MIN, INT_MIN, INT_MIN, INT_MIN}}; b = a;
    set_status(0x12); r.v = operation(a.v,b.v,6,0);
    uint32_t qc = get_status();
    int fail = qc != ((1u << 27) | 0x12);
    for (int i=0;i<4;i++) fail |= r.s[i] != INT_MAX;
    printf("qdmulh digest %016llx qc_cases %u boundary %s\n", (unsigned long long)digest, qc_cases, fail ? "FAIL" : "PASS");
    set_status(original);
    return fail ? 1 : 0;
}
