// Half conversion result/status oracle uses integer arithmetic only, never host fenv.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static uint16_t half_ref(uint32_t b, uint64_t fpcr) {
    unsigned sign = b >> 31, e = (b >> 23) & 255, f = b & 0x7fffff;
    unsigned mode = (fpcr >> 22) & 3, alt = (fpcr >> 26) & 1;
    unsigned result_sign = sign << 15;
    if (e == 255) {
        if (!f) return result_sign | (alt ? 0x7fff : 0x7c00);
        if (alt) return result_sign;
        if (fpcr & (1ull << 25)) return 0x7e00;
        return result_sign | 0x7e00 | ((f >> 13) & 0x1ff);
    }
    if (!e && (!f || (fpcr & (1ull << 24)))) return result_sign;
    int exponent = (int)e - 127;
    uint64_t mantissa = e ? (0x800000 | f) : f;
    int shift = exponent < -14 ? -exponent - 1 : 13;
    uint64_t q = shift >= 64 ? 0 : mantissa >> shift;
    uint64_t remainder = shift >= 64 ? mantissa : mantissa & ((1ull << shift) - 1);
    int increment = 0;
    if (remainder) {
        if (mode == 0 && shift < 64) {
            uint64_t halfway = 1ull << (shift - 1);
            increment = remainder > halfway || (remainder == halfway && (q & 1));
        } else if (mode == 1) increment = !sign;
        else if (mode == 2) increment = sign;
    }
    q += increment;
    int half_exp = exponent < -14 ? 0 : exponent + 15;
    if (q >= 2048) { q >>= 1; half_exp++; }
    if (half_exp > (alt ? 31 : 30)) {
        int infinity = !alt && (mode == 0 || (mode == 1 && !sign) || (mode == 2 && sign));
        return result_sign | (infinity ? 0x7c00 : alt ? 0x7fff : 0x7bff);
    }
    if (half_exp == 0) return result_sign | (uint16_t)q;
    return result_sign | (half_exp << 10) | (q & 1023);
}

// Re-expand the finite result as raw FP32 bits to detect inexactness, then
// classify overflow/underflow independently from the converter's discarded bits.
static uint32_t widen(uint16_t h, int alt) {
    unsigned sign = (unsigned)(h & 0x8000) << 16, e = (h >> 10) & 31, f = h & 1023;
    if (!e) {
        if (!f) return sign;
        int unbiased = -14;
        while (!(f & 1024)) { f <<= 1; unbiased--; }
        return sign | (unsigned)(unbiased + 127) << 23 | (f & 1023) << 13;
    }
    if (e == 31 && !alt) return sign | 0x7f800000 | f << 13;
    return sign | (e + 112) << 23 | f << 13;
}

static unsigned status_ref(uint32_t b, uint64_t fpcr) {
    uint32_t a = b & 0x7fffffff;
    unsigned alt = !!(fpcr & (1ull << 26));
    if (a > 0x7f800000) return alt || !(b & 0x400000) ? 1 : 0;
    if (a == 0x7f800000) return alt ? 1 : 0;
    if (a && a < 0x800000 && (fpcr & (1ull << 24))) return 128;
    uint16_t h = half_ref(b, fpcr);
    unsigned max = alt ? 0x7fff : 0x7bff;
    // Finite AHP saturation raises IOC, suppressing IXC.
    if (alt && a >= 0x48000000) return 1;
    // AHP can round a value just below 131072 above the available exponent.
    if (alt && (h & 0x7fff) == max && a > widen(max, 1)) {
        unsigned mode = (fpcr >> 22) & 3;
        if ((mode == 0 && a >= 0x47fff000) || (mode == 1 && !(b >> 31)) || (mode == 2 && (b >> 31))) return 1;
    }
    unsigned flags = widen(h, alt) != b ? 16 : 0;
    if (flags && a < 0x38800000) flags |= 8; // tininess before rounding
    if (!alt && (a >= 0x47800000 || (h & 0x7fff) == 0x7c00)) flags |= 20;
    return flags;
}

#ifdef __aarch64__
#define CONVERT(NAME, INST, OUTREG) \
__attribute__((noinline)) static uint64_t NAME(const uint32_t *src, const uint16_t *old, uint16_t *out, uint64_t seed) { \
    uint64_t status; \
    __asm__ volatile("msr fpsr, %4\nldr q0, [%2]\nldr q1, [%1]\n" INST "\nstr " OUTREG ", [%3]\nmrs %0, fpsr" \
        : "=r"(status) : "r"(src), "r"(old), "r"(out), "r"(seed) : "v0", "v1", "memory"); \
    return status; \
}
CONVERT(low, "fcvtn v0.4h, v1.4s", "q0")
CONVERT(high, "fcvtn2 v0.8h, v1.4s", "q0")
CONVERT(low_alias, "fcvtn v1.4h, v1.4s", "q1")
CONVERT(high_alias, "fcvtn2 v1.8h, v1.4s", "q1")
CONVERT(scalar, "fcvt h0, s1", "q0")
CONVERT(scalar_alias, "fcvt h1, s1", "q1")
CONVERT(scalar64, "fcvt h0, d1", "q0")
__attribute__((noinline)) static uint64_t sticky_pair(const uint32_t *a, const uint32_t *b) {
    uint64_t status;
    __asm__ volatile("msr fpsr, xzr\nldr q1, [%1]\nfcvtn v0.4h, v1.4s\n"
                     "ldr q1, [%2]\nfcvtn v0.4h, v1.4s\nmrs %0, fpsr"
                     : "=r"(status) : "r"(a), "r"(b) : "v0", "v1", "memory");
    return status;
}
#endif

int main(int argc, char **argv) {
    const uint32_t edges[] = {0,0x80000000,0x3f800000,0xbf800000,0x3f801000,0xbf801000,
        0x477fe000,0x477ff000,0xc77ff000,0x47800000,0x47fff000,0x48000000,
        0x33000000,0xb3000000,0x33800000,0x387fe000,0x387fffff,0x38800000,1,0x80000001,
        0x7f7fffff,0xff7fffff,0x7f800000,0xff800000,0x7fc00000,0xffc01234,0x7f800001,0xff812345};
    const uint64_t flags[] = {0,1ull<<24,1ull<<19,1ull<<25,1ull<<26};
    uint64_t digest = 1469598103934665603ull, saved = 0;
    unsigned checked = 0; uint32_t rng = 0x6913cafe;
#ifdef __aarch64__
    __asm__ volatile("mrs %0, fpcr" : "=r"(saved));
#endif
    if (argc > 1 && !strcmp(argv[1], "--bench")) {
        unsigned loops = argc > 2 ? (unsigned)strtoul(argv[2], NULL, 10) : 1000000;
        uint32_t src[4] = {0x3f800001,0x3f801000,0x38800001,0x33800000};
        uint16_t old[8] = {0}, got[8]; uint64_t accum = 0, result_hash = 1469598103934665603ull;
        struct timespec t0, t1;
#ifdef __aarch64__
        uint64_t zero = 0;
        __asm__ volatile("msr fpcr, %0" :: "r"(zero) : "memory");
#endif
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (unsigned i = 0; i < loops; i++) {
#ifdef __aarch64__
            accum ^= low(src, old, got, 0);
#else
            for (unsigned j = 0; j < 4; j++) got[j] = half_ref(src[j], 0);
            accum ^= status_ref(src[0],0) | status_ref(src[1],0) | status_ref(src[2],0) | status_ref(src[3],0);
#endif
            accum *= 1099511628211ull;
            result_hash = (result_hash * 1099511628211ull) ^ got[i & 3];
        }
        clock_gettime(CLOCK_MONOTONIC, &t1);
#ifdef __aarch64__
        __asm__ volatile("msr fpcr, %0" :: "r"(saved) : "memory");
#endif
        double ms = (t1.tv_sec - t0.tv_sec) * 1000.0 + (t1.tv_nsec - t0.tv_nsec) / 1e6;
        printf("half-status %.3f ms\nresult-checksum %016llx status-checksum %016llx\n", ms,
               (unsigned long long)result_hash, (unsigned long long)accum); return 0;
    }
    for (unsigned mode = 0; mode < 4; mode++) for (unsigned flag = 0; flag < 5; flag++) {
        uint64_t fpcr = (uint64_t)mode << 22 | flags[flag];
#ifdef __aarch64__
        __asm__ volatile("msr fpcr, %0" :: "r"(fpcr) : "memory");
#endif
        for (unsigned iteration = 0; iteration < 500; iteration++) {
            uint32_t src[4]; uint16_t old[8], want[8], got[8];
            for (unsigned i = 0; i < 4; i++) {
                rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5;
                src[i] = iteration < sizeof edges / sizeof *edges ? edges[(iteration + i * 7) % (sizeof edges / sizeof *edges)] : rng;
            }
            for (unsigned i = 0; i < 8; i++) old[i] = (uint16_t)(0x1234 + i * 0x1111);
            for (unsigned variant = 0; variant < 6; variant++) {
                int upper = variant < 4 && (variant & 1), alias = variant & 2;
                uint64_t seed = iteration & 1 ? (1ull << 27) | 2 : 0;
                uint64_t want_status = seed, got_status;
                memset(want, 0, sizeof want);
                if (upper) memcpy(want, alias ? (const void *)src : (const void *)old, 8);
                for (unsigned i = 0; i < (variant < 4 ? 4u : 1u); i++) {
                    want[i + (upper ? 4 : 0)] = half_ref(src[i], fpcr);
                    want_status |= status_ref(src[i], fpcr);
                }
#ifdef __aarch64__
                uint64_t (*f[])(const uint32_t *, const uint16_t *, uint16_t *, uint64_t) = {low, high, low_alias, high_alias, scalar, scalar_alias};
                got_status = f[variant](src, old, got, seed);
#else
                memcpy(got, want, sizeof got); got_status = want_status;
#endif
                if (memcmp(got, want, sizeof got) || got_status != want_status) {
                    printf("FAIL mode=%u flag=%u iteration=%u variant=%u status=%llx/%llx\n", mode, flag, iteration, variant,
                           (unsigned long long)got_status, (unsigned long long)want_status);
                    for (unsigned i = 0; i < 4; i++) printf("src%u=%08x ", i, src[i]); puts(""); return 1;
                }
                for (unsigned i = 0; i < 8; i++) digest = (digest ^ got[i]) * 1099511628211ull;
                digest = (digest ^ got_status) * 1099511628211ull; checked++;
            }
        }
    }
#ifdef __aarch64__
    // Double-source checks use hand-written result/status tables rather than
    // passing through FP32 (which would lose the smallest double subnormal).
    const struct { uint64_t bits; uint16_t result[4]; unsigned status[4]; } doubles[] = {
        {0x3ff0000000000000ull, {0x3c00,0x3c00,0x3c00,0x3c00}, {0,0,0,0}},
        {0x3ff0020000000000ull, {0x3c00,0x3c01,0x3c00,0x3c00}, {16,16,16,16}},
        {0x40effe0000000000ull, {0x7c00,0x7c00,0x7bff,0x7bff}, {20,20,16,16}},
        {0x7ff8000000001234ull, {0x7e00,0x7e00,0x7e00,0x7e00}, {0,0,0,0}},
        {0x7ff0000000001234ull, {0x7e00,0x7e00,0x7e00,0x7e00}, {1,1,1,1}},
        {1, {0,1,0,0}, {24,24,24,24}},
    };
    for (unsigned mode = 0; mode < 4; mode++) for (unsigned fz = 0; fz < 2; fz++) {
        uint64_t fpcr = (uint64_t)mode << 22 | (uint64_t)fz << 24;
        __asm__ volatile("msr fpcr, %0" :: "r"(fpcr) : "memory");
        for (unsigned i = 0; i < sizeof doubles / sizeof *doubles; i++) {
            uint64_t src[2] = {doubles[i].bits, 0}; uint16_t old[8] = {0}, out[8];
            uint16_t want = doubles[i].result[mode]; unsigned status = doubles[i].status[mode];
            if (doubles[i].bits == 1 && fz) { want = 0; status = 128; }
            uint64_t got = scalar64((const uint32_t *)src, old, out, 1ull << 27);
            if (out[0] != want || got != ((1ull << 27) | status)) {
                printf("FAIL scalar64 case=%u mode=%u fz=%u result=%x/%x status=%llx/%x\n", i, mode, fz,
                       out[0], want, (unsigned long long)got, status); return 1;
            }
            for (unsigned lane = 1; lane < 8; lane++) if (out[lane]) { puts("FAIL scalar64 upper clear"); return 1; }
        }
    }
#endif
#ifdef __aarch64__
    // Conversion leaves host sticky flags behind; clearing guest FPSR must not
    // re-import them into a later exact conversion.
    uint32_t bad[4] = {0x7f800001,0x7f7fffff,0x33000000,0x3f801000};
    uint32_t exact[4] = {0x3f800000,0x40000000,0x40400000,0x40800000};
    uint16_t old[8] = {0}, out[8]; uint64_t zero = 0;
    __asm__ volatile("msr fpcr, %0" :: "r"(zero) : "memory");
    for (unsigned i = 0; i < 100; i++) {
        uint64_t bad_status = low(bad, old, out, 0);
        uint64_t clean = low(exact, old, out, 0);
        uint64_t sticky = sticky_pair(bad, exact);
        if (sticky != 29) { printf("FAIL cumulative status %llx\n", (unsigned long long)sticky); return 1; }
        if (bad_status != 29 || clean != 0) { printf("FAIL clear isolation %llx/%llx\n", (unsigned long long)bad_status, (unsigned long long)clean); return 1; }
    }
    __asm__ volatile("msr fpcr, %0" :: "r"(saved) : "memory");
#endif
    printf("fp16-status PASS cases=%u digest=%016llx clear-isolation=PASS double-boundaries=PASS\n", checked, (unsigned long long)digest);
    return 0;
}
