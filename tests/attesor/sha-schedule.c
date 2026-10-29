// SHA-256 schedule throughput and semantic checksum. Build:
// clang -O2 -arch arm64 -march=armv8-a+crypto sha-schedule.c -o build/sha-schedule
// clang -O2 -arch x86_64 sha-schedule.c -o build/sha-schedule-x86
// Both must print the same checksum; only elapsed time differs.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#ifdef __aarch64__
#include <arm_neon.h>
#endif
static volatile uint32_t seed = 0x12345678;
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
#ifndef __aarch64__
static uint32_t ror(uint32_t x, int n) { return (x >> n) | (x << (32 - n)); }
static uint32_t s0(uint32_t x) { return ror(x, 7) ^ ror(x, 18) ^ (x >> 3); }
static uint32_t s1(uint32_t x) { return ror(x, 17) ^ ror(x, 19) ^ (x >> 10); }
#endif
int main(int argc, char **argv) {
    int count = argc > 1 ? atoi(argv[1]) : 1000000;
    if (count < 1) return 2;
    uint32_t a[4] = {seed, 0xabcdeff0, 0xfedcba98, 0x1f83d9ab};
    uint32_t b[4] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a};
    uint32_t c[4] = {0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};
#ifdef __aarch64__
    uint32x4_t va = vld1q_u32(a), vb = vld1q_u32(b), vc = vld1q_u32(c);
#endif
    double start = now();
    for (int it = 0; it < count; it++) {
#ifdef __aarch64__
        va = vsha256su0q_u32(va, vb);
        va = vsha256su1q_u32(va, vb, vc);
        vb = vc; vc = va;
#else
        uint32_t t[4], u[4];
        t[0] = a[0] + s0(a[1]); t[1] = a[1] + s0(a[2]);
        t[2] = a[2] + s0(a[3]); t[3] = a[3] + s0(b[0]);
        u[0] = t[0] + b[1] + s1(c[2]); u[1] = t[1] + b[2] + s1(c[3]);
        u[2] = t[2] + b[3] + s1(u[0]); u[3] = t[3] + c[0] + s1(u[1]);
        memcpy(a, u, sizeof a); memcpy(b, c, sizeof b); memcpy(c, u, sizeof c);
#endif
    }
    double elapsed = now() - start;
#ifdef __aarch64__
    vst1q_u32(a, va); vst1q_u32(b, vb); vst1q_u32(c, vc);
#endif
    uint64_t hash = 1469598103934665603ull;
    for (int i = 0; i < 4; i++) { hash = (hash ^ a[i]) * 1099511628211ull; hash = (hash ^ b[i]) * 1099511628211ull; hash = (hash ^ c[i]) * 1099511628211ull; }
    printf("sha-sched %.3f ms\nchecksum %016llx\n", elapsed, (unsigned long long)hash);
    return 0;
}
