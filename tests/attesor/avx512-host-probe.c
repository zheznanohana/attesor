#include <cpuid.h>
#include <stdint.h>
#include <stdio.h>
static uint64_t xcr0(void) { unsigned a,d; __asm__ volatile("xgetbv":"=a"(a),"=d"(d):"c"(0)); return ((uint64_t)d<<32)|a; }
int main(void) {
    unsigned a,b,c,d; __cpuid_count(7,0,a,b,c,d);
    unsigned required=(1u<<16)|(1u<<17)|(1u<<30)|(1u<<31);
    if ((b&required)!=required) return 77;
    uint64_t in[2]={0x123456789abcdef0ULL,0xfedcba9876543210ULL},out[2]={0};
    printf("XCR0 before: 0x%llx\n",(unsigned long long)xcr0());
    __asm__ volatile("mov $3, %%eax; kmovw %%eax, %%k1; vmovdqu64 (%0), %%xmm17%{%%k1%}%{z%}; vmovdqu64 %%xmm17, (%1)"::"r"(in),"r"(out):"eax","k1","xmm17","memory");
    printf("XCR0 after: 0x%llx; correct=%d\n",(unsigned long long)xcr0(),in[0]==out[0]&&in[1]==out[1]);
    return in[0]!=out[0]||in[1]!=out[1];
}
