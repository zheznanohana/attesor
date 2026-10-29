// Cross-block overwritten stores must disappear only in the offline blob.
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#ifdef __aarch64__
__asm__(
    ".text\n.p2align 2\n.globl _offline_probe\n_offline_probe:\n"
    "mov x2,#7\nb 1f\n1: mov x2,#9\n"
    "add x3,x0,x1\nadd x3,x3,x2\neor x4,x3,x0\n"
    "add w4,w4,#3\nmov x0,x4\nret\n");
extern uint64_t offline_probe(uint64_t, uint64_t);
#else
static uint64_t offline_probe(uint64_t a, uint64_t b) {
    return (uint32_t)(((a + b + 9) ^ a) + 3);
}
#endif
int main(void) {
    uint64_t state = 0x91ff010203040506ULL, hash = 0;
    for (int i = 0; i < 97; ++i) {
        state ^= state << 13; state ^= state >> 7; state ^= state << 17;
        hash = (hash << 3 | hash >> 61) ^ offline_probe(state, ~state + i);
    }
    printf("offline checksum %016" PRIx64 "\n", hash);
    return 0;
}
