// Independent x86 reference for AArch64 compare/test-and-branch decisions and
// exact NZCV preservation. --bench N isolates repeated CBZ/CBNZ or TBZ/TBNZ.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static uint64_t digest = 1469598103934665603ULL;
static int check(uint64_t got, uint64_t flags, uint64_t expected, uint64_t wanted) {
    if (got != expected || flags != wanted) {
        fprintf(stderr, "decision=%llu expected=%llu flags=%llx wanted=%llx\n",
                (unsigned long long)got, (unsigned long long)expected,
                (unsigned long long)flags, (unsigned long long)wanted);
        return 0;
    }
    digest = (digest ^ got ^ flags) * 1099511628211ULL;
    return 1;
}
#ifdef __aarch64__
#define CHECK_BRANCH(instruction, condition) do { \
    uint64_t got, flags; \
    __asm__ volatile("msr nzcv, %[wanted]\n" instruction " 1f\n" \
                     "mov %[got], #0\nb 2f\n1: mov %[got], #1\n2: mrs %[flags], nzcv" \
                     : [got] "=&r"(got), [flags] "=r"(flags) \
                     : [v] "r"(v), [wanted] "r"(wanted) : "cc"); \
    if (!check(got, flags, (condition), wanted)) return 1; \
} while (0)
#else
#define CHECK_BRANCH(instruction, condition) do { \
    if (!check((condition), wanted, (condition), wanted)) return 1; \
} while (0)
#endif

static double now(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000.0 + t.tv_nsec / 1e6;
}
static uint64_t work(uint64_t n, int tb) {
    uint64_t x = 0x123456789abcdefULL, s = 7;
#ifdef __aarch64__
    uint64_t temporary;
    if (!tb) {
        __asm__ volatile(
            "0: and %[t], %[x], #7\ncbz %[t], 1f\nadd %[s], %[s], %[x]\nb 2f\n"
            "1: eor %[s], %[s], %[x]\n2: and %[t], %[x], #3\ncbnz %w[t], 3f\n"
            "add %[s], %[s], #17\n3: eor %[x], %[x], %[x], lsl #13\n"
            "eor %[x], %[x], %[x], lsr #7\neor %[x], %[x], %[x], lsl #17\n"
            "subs %[n], %[n], #1\nb.ne 0b"
            : [x] "+r"(x), [s] "+r"(s), [n] "+r"(n), [t] "=&r"(temporary) :: "cc");
    } else {
        __asm__ volatile(
            "0: tbz %[x], #31, 1f\nadd %[s], %[s], %[x]\nb 2f\n"
            "1: eor %[s], %[s], %[x]\n2: tbnz %[x], #63, 3f\n"
            "add %[s], %[s], #29\n3: eor %[x], %[x], %[x], lsl #13\n"
            "eor %[x], %[x], %[x], lsr #7\neor %[x], %[x], %[x], lsl #17\n"
            "subs %[n], %[n], #1\nb.ne 0b"
            : [x] "+r"(x), [s] "+r"(s), [n] "+r"(n) :: "cc");
    }
#else
    while (n--) {
        if ((tb ? (x & (1ULL << 31)) : (x & 7)) != 0) s += x; else s ^= x;
        if ((tb ? (x & (1ULL << 63)) : (x & 3)) == 0) s += tb ? 29 : 17;
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
    }
#endif
    return s ^ x;
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--bench")) {
        uint64_t n = argc > 2 ? strtoull(argv[2], 0, 10) : 20000000;
        if (!n) return 2;
        double t = now(); uint64_t a = work(n, 0);
        printf("cbz-chain %.3f ms\n", now() - t);
        t = now(); uint64_t b = work(n, 1);
        printf("tbz-chain %.3f ms\nchecksum %016llx %016llx\n", now() - t,
               (unsigned long long)a, (unsigned long long)b);
        return 0;
    }
    const uint64_t values[] = {0, 1, UINT64_MAX, 1ULL << 31, 1ULL << 32,
                              1ULL << 63, 0xffffffff00000000ULL, 0x55555555aaaaaaaaULL};
    for (int repeat = 0; repeat < 128; repeat++) for (int i = 0; i < 8; i++) {
        uint64_t v = values[i];
        for (uint64_t f = 0; f < 16; f++) {
            uint64_t wanted = f << 28;
            CHECK_BRANCH("cbz %w[v],", (uint32_t)v == 0);
            CHECK_BRANCH("cbnz %w[v],", (uint32_t)v != 0);
            CHECK_BRANCH("cbz %[v],", v == 0);
            CHECK_BRANCH("cbnz %[v],", v != 0);
            CHECK_BRANCH("cbz xzr,", 1);
            CHECK_BRANCH("cbnz xzr,", 0);
            CHECK_BRANCH("tbz %[v], #0,", !(v & 1));
            CHECK_BRANCH("tbnz %[v], #31,", !!(v & (1ULL << 31)));
            CHECK_BRANCH("tbz %[v], #32,", !(v & (1ULL << 32)));
            CHECK_BRANCH("tbnz %[v], #63,", !!(v & (1ULL << 63)));
            CHECK_BRANCH("tbz xzr, #63,", 1);
            CHECK_BRANCH("tbnz xzr, #0,", 0);
        }
    }
    printf("direct-branch PASS cases=196608 checksum %016llx\n", (unsigned long long)digest);
    return 0;
}
