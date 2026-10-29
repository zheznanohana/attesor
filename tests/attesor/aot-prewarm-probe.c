#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
static volatile uint64_t seed = 0x12345678;
__attribute__((constructor)) static void marker(void) {
    const char *p = getenv("PREWARM_SENTINEL");
    if (p) { FILE *f = fopen(p, "a"); if (f) { fputs("constructor\n", f); fclose(f); } }
}
__attribute__((noinline)) uint64_t leaf(uint64_t x) {
    for (int i=0; i<1000; i++) { x ^= x << 7; x ^= x >> 9; x *= 0x100000001b3ULL; }
    return x;
}
int main(void) {
    uint64_t x = leaf(seed); x ^= 0xabcdef;
    printf("prewarm-checksum %016llx\n", (unsigned long long)x);
    return 0;
}
