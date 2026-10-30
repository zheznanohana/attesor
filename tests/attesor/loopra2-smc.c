// Runtime-generated, multi-block loop whose body lies on a different 16-KiB
// guest page, beyond the old header+MAX_BLOCK_INSNS invalidation range.
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <libkern/OSCacheControl.h>
int main(void) {
    unsigned char *p = mmap(0, 32768, PROT_READ|PROT_WRITE|PROT_EXEC, MAP_ANON|MAP_PRIVATE, -1, 0);
    if (p == MAP_FAILED) return 2;
    uint32_t *h = (uint32_t *)(p+0x3000), *b = (uint32_t *)(p+0x4800);
    h[0] = 0x91000400; // add x0,x0,#1
    h[1] = 0x14000000 | ((uint32_t)((char *)b-(char *)(h+1))/4);
    b[0] = 0x91000400; // add x0,x0,#1 (rewritten below)
    b[1] = 0xf1000421; // subs x1,x1,#1
    b[2] = 0x54000001 | (((uint32_t)(((char *)h-(char *)(b+2))/4)&0x7ffff)<<5);
    b[3] = 0xd65f03c0; // ret
    sys_icache_invalidate(p,32768);
    uint64_t (*loop)(uint64_t,uint64_t) = (void *)h;
    uint64_t sum = 0;
    for (int k=0;k<1000;k++) { uint64_t r=loop(0,4); if(r!=8) return 3; sum+=r; }
    b[0] = 0x91000800; // add x0,x0,#2; invalidate ONLY the second page
    sys_icache_invalidate(b,4);
    for (int k=0;k<1000;k++) { uint64_t r=loop(0,4); if(r!=12) { printf("stale %llu expected 12\n",r); return 4; } sum+=r; }
    munmap(p,32768);
    printf("loopra2 SMC PASS checksum=%llu\n",sum);
    return 0;
}
