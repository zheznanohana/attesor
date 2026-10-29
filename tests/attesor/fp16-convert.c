// Independent integer reference for FPCR-sensitive FCVTN/FCVTN2.
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static uint16_t reference(uint32_t b, uint64_t fpcr) {
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

#ifdef __aarch64__
#define CONVERT(NAME, INST, OUTREG) \
__attribute__((noinline)) static void NAME(const uint32_t *src, const uint16_t *old, uint16_t *out) { \
    __asm__ volatile("ldr q0, [%1]\nldr q1, [%0]\n" INST "\nstr " OUTREG ", [%2]" \
        :: "r"(src), "r"(old), "r"(out) : "v0", "v1", "memory"); \
}
CONVERT(low, "fcvtn v0.4h, v1.4s", "q0")
CONVERT(high, "fcvtn2 v0.8h, v1.4s", "q0")
CONVERT(low_alias, "fcvtn v1.4h, v1.4s", "q1")
CONVERT(high_alias, "fcvtn2 v1.8h, v1.4s", "q1")
CONVERT(scalar, "fcvt h0, s1", "q0")
CONVERT(scalar_alias, "fcvt h1, s1", "q1")
#endif

int main(int argc, char **argv) {
    static const uint32_t edges[] = {0,0x80000000,0x3f800000,0xbf800000,0x3f801000,0xbf801000,
        0x477fe000,0x477ff000,0xc77ff000,0x47800000,0x47fff000,0x48000000,
        0x33000000,0xb3000000,0x33800000,0x387fc000,0x38800000,1,0x80000001,
        0x7f7fffff,0xff7fffff,0x7f800000,0xff800000,0x7fc00000,0xffc01234,
        0x7f800001,0xff812345};
    const uint64_t flags[] = {0,1ull<<24,1ull<<19,1ull<<25,1ull<<26};
    uint64_t digest = 1469598103934665603ull, saved = 0;
    unsigned checked = 0;
#ifdef __aarch64__
    __asm__ volatile("mrs %0, fpcr" : "=r"(saved));
#endif
    uint32_t rng = 0x6913cafe;
    for (unsigned mode = 0; mode < 4; mode++) for (unsigned flag = 0; flag < (argc > 1 && !strcmp(argv[1], "--round-only") ? 1u : 5u); flag++) {
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
                memset(want, 0, sizeof want);
                if (upper) memcpy(want, alias ? (const void *)src : (const void *)old, 8);
                for (unsigned i = 0; i < (variant < 4 ? 4u : 1u); i++) want[i + (upper ? 4 : 0)] = reference(src[i], fpcr);
#ifdef __aarch64__
                void (*f[])(const uint32_t *, const uint16_t *, uint16_t *) = {low, high, low_alias, high_alias, scalar, scalar_alias};
                f[variant](src, old, got);
#else
                memcpy(got, want, sizeof got);
#endif
                if (memcmp(got, want, sizeof got)) {
                    printf("FAIL mode=%u flag=%u iteration=%u variant=%u\n", mode, flag, iteration, variant);
                    for (unsigned i = 0; i < 4; i++) printf("src%u=%08x ", i, src[i]);
                    printf("\n");
                    for (unsigned i = 0; i < 8; i++) printf("%04x/%04x ", got[i], want[i]);
                    printf("\n"); return 1;
                }
                for (unsigned i = 0; i < 8; i++) digest = (digest ^ got[i]) * 1099511628211ull;
                checked++;
            }
        }
    }
    // Explicit hand-computed FZ checks: directed rounding of FP32's smallest
    // subnormal yields a half subnormal only with FZ clear, and preserves -0.
    for (unsigned mode = 1; mode <= 2; mode++) for (unsigned fz = 0; fz < 2; fz++) {
        const uint32_t src[4] = {1, 0x80000001, 1, 0x80000001};
        const uint16_t old[8] = {0}; uint16_t got[8] = {0}, want[8] = {0};
        uint64_t fpcr = (uint64_t)mode << 22 | (uint64_t)fz << 24;
        want[0] = want[2] = !fz && mode == 1 ? 1 : 0;
        want[1] = want[3] = !fz && mode == 2 ? 0x8001 : 0x8000;
#ifdef __aarch64__
        __asm__ volatile("msr fpcr, %0" :: "r"(fpcr) : "memory");
        low(src, old, got);
#else
        memcpy(got, want, sizeof got);
#endif
        if (memcmp(got, want, sizeof got)) { printf("FAIL explicit tiny FZ mode=%u fz=%u\n", mode, fz); return 1; }
    }
#ifdef __aarch64__
    __asm__ volatile("msr fpcr, %0" :: "r"(saved) : "memory");
#endif
    puts("tiny FP32 FZ boundaries PASS");
    printf("fp16-convert PASS cases=%u digest=%016llx\n", checked, (unsigned long long)digest);
    return 0;
}
