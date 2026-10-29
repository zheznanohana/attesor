// Small deterministic integer workloads, also compiled natively for reference.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#ifndef KERNEL
#define KERNEL 0
#endif
__attribute__((noinline)) static uint64_t kernel(void) {
#if KERNEL == 0
    uint64_t h = 1469598103934665603ull;
    for (unsigned i = 0; i < 12000; i++) { h ^= (uint8_t)(i * 31); h *= 1099511628211ull; }
    return h;
#elif KERNEL == 1
    uint32_t tab[256], c = ~0u; uint8_t b[4096];
    for (unsigned i = 0; i < 256; i++) {
        uint32_t t = i;
        for (unsigned k = 0; k < 8; k++) t = (t >> 1) ^ ((t & 1) ? 0xedb88320u : 0);
        tab[i] = t;
    }
    for (unsigned i = 0; i < sizeof b; i++) b[i] = (uint8_t)(i * 31);
    for (unsigned i = 0; i < sizeof b; i++) c = tab[(c ^ b[i]) & 255] ^ (c >> 8);
    return c;
#elif KERNEL == 2
    unsigned char c[8192] = {0}; uint64_t count = 0;
    for (long i = 2; i < sizeof c; i++) if (!c[i]) {
        count++;
        for (long j = i * i; j < sizeof c; j += i) c[j] = 1;
    }
    return count;
#elif KERNEL == 3
    uint8_t src[4096]; uint64_t c = 0; unsigned st = 0;
    for (unsigned i = 0; i < sizeof src; i++) src[i] = (uint8_t)(i * 2654435761u >> 24);
    for (unsigned i = 0; i < sizeof src; i++) {
        unsigned v = src[i];
        if (v < 64) st = (st + 1) & 7;
        else if (v < 128) st ^= 3;
        else if (v & 1) c += st;
        else c -= st;
    }
    return c;
#elif KERNEL == 4
    struct node { struct node *next; uint64_t v; } nodes[257];
    for (unsigned i = 0; i < 257; i++) { nodes[i].v = i; nodes[i].next = &nodes[(i * 19 + 1) % 257]; }
    struct node *p = nodes; uint64_t s = 0;
    for (unsigned i = 0; i < 12000; i++) { s += p->v; p = p->next; }
    return s;
#else
    uint64_t a[258], b[258] = {0}, sum = 0;
    for (unsigned i = 0; i < 258; i++) a[i] = i * 7919u + 17;
    uint64_t *src = a, *dst = b;
    unsigned n = 128;
#if defined(__aarch64__) && KERNEL == 5
    // Both pre- and post-index writeback, with a register killed before its first read.
    __asm__ volatile("1: ldr x8, [%[s]], #8\n"
                     "ldr x9, [%[s], #8]!\n"
                     "str x8, [%[d]], #8\n"
                     "str x9, [%[d], #8]!\n"
                     "subs %w[n], %w[n], #1\n"
                     "b.ne 1b"
                     : [s] "+r"(src), [d] "+r"(dst), [n] "+r"(n) : : "x8", "x9", "cc", "memory");
#elif defined(__aarch64__) && KERNEL == 6
    __asm__ volatile("1: ldp x8, x9, [%[s]], #16\n"
                     "stp x8, x9, [%[d]], #16\n"
                     "subs %w[n], %w[n], #1\n"
                     "b.ne 1b"
                     : [s] "+r"(src), [d] "+r"(dst), [n] "+r"(n) : : "x8", "x9", "cc", "memory");
#elif KERNEL == 5
    for (unsigned i = 0; i < 128; i++) { *dst++ = *src++; src++; dst++; *dst = *src; }
#else
    for (unsigned i = 0; i < 256; i += 2) { dst[i] = src[i]; dst[i + 1] = src[i + 1]; }
#endif
    for (unsigned i = 0; i < 256; i++) sum += b[i];
    return sum;
#endif
}
int main(void) { printf("checksum %016llx\n", (unsigned long long)kernel()); return 0; }
