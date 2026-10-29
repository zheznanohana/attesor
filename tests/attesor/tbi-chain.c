// Dependent pointer loads with real AArch64 top-byte tags. The native reference
// explicitly removes tags because x86 has no TBI. Times only the dependent walk;
// its checksums are address-independent. Build -O2 for arm64 and x86_64.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

typedef struct { uintptr_t next; uint64_t id; } Node;
static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000.0 + t.tv_nsec / 1e6;
}
static uintptr_t step(uintptr_t p) {
#ifdef __aarch64__
    __asm__ volatile("ldr %0, [%0]" : "+r"(p) :: "memory");
    return p;
#else
    return *(const uintptr_t *)(p & UINT64_C(0x00ffffffffffffff));
#endif
}
__attribute__((noinline)) static uintptr_t walk(uintptr_t p, uint64_t count) {
    while (count >= 8) {
#ifdef __aarch64__
        __asm__ volatile("ldr %0, [%0]\nldr %0, [%0]\nldr %0, [%0]\nldr %0, [%0]\n"
                         "ldr %0, [%0]\nldr %0, [%0]\nldr %0, [%0]\nldr %0, [%0]"
                         : "+r"(p) :: "memory");
#else
        p=step(p); p=step(p); p=step(p); p=step(p);
        p=step(p); p=step(p); p=step(p); p=step(p);
#endif
        count -= 8;
    }
    while (count--) p=step(p);
    return p;
}
int main(int argc, char **argv) {
    uint64_t count = argc > 1 ? strtoull(argv[1], NULL, 10) : 10000000;
    if (!count) return 2;
    enum { N=1024, STRIDE=173 };
    Node *nodes = calloc(N, sizeof *nodes);
    if (!nodes) return 1;
    for (unsigned i=0;i<N;i++) {
        unsigned next=(i+STRIDE)&(N-1);
        nodes[i].next=(uintptr_t)&nodes[next] | (uint64_t)(1+(i%255))<<56;
        nodes[i].id=UINT64_C(0x9e3779b97f4a7c15)*(i+1);
    }
    uintptr_t p=(uintptr_t)nodes | UINT64_C(0xab00000000000000);
    double start=now();
    p=walk(p,count);
    double elapsed=now()-start;
    Node *end=(Node *)(p & UINT64_C(0x00ffffffffffffff));
    unsigned expected=(unsigned)((count & (N-1))*STRIDE)&(N-1);
    if (end != &nodes[expected] || end->id != UINT64_C(0x9e3779b97f4a7c15)*(expected+1)) return 1;
    printf("tbi-chain %.3f ms\nchecksum %016llx\n",elapsed,(unsigned long long)end->id);
    free(nodes);
    return 0;
}
