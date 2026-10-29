// Register-cache pressure: independent x86 scalar reference, ARM explicit 9-V
// recurrence (one more than the eight read-cache slots), and integer recurrence.
// clang -arch ARCH -O2 -ffp-contract=off nextuse-pressure.c -o pressure
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
static volatile uint64_t seed = 7;
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return t.tv_sec*1000.+t.tv_nsec/1e6; }
__attribute__((noinline)) static void fp(double *v) {
#ifdef __aarch64__
#define ROUND "fadd d0,d0,d1\n fadd d1,d1,d2\n fadd d2,d2,d3\n fadd d3,d3,d4\n fadd d4,d4,d5\n fadd d5,d5,d6\n fadd d6,d6,d7\n fadd d7,d7,d8\n fadd d8,d8,d0\n"
    __asm__ volatile(
        "ldr d0,[%0,#0]\n ldr d1,[%0,#8]\n ldr d2,[%0,#16]\n ldr d3,[%0,#24]\n ldr d4,[%0,#32]\n ldr d5,[%0,#40]\n ldr d6,[%0,#48]\n ldr d7,[%0,#56]\n ldr d8,[%0,#64]\n"
        ROUND ROUND ROUND ROUND ROUND ROUND ROUND ROUND
        ROUND ROUND ROUND ROUND ROUND ROUND ROUND ROUND
        "str d0,[%0,#0]\n str d1,[%0,#8]\n str d2,[%0,#16]\n str d3,[%0,#24]\n str d4,[%0,#32]\n str d5,[%0,#40]\n str d6,[%0,#48]\n str d7,[%0,#56]\n str d8,[%0,#64]\n"
        : : "r"(v) : "v0","v1","v2","v3","v4","v5","v6","v7","v8","memory");
#undef ROUND
#else
    for(int r=0;r<16;r++){for(int i=0;i<8;i++)v[i]+=v[i+1];v[8]+=v[0];}
#endif
}
__attribute__((noinline)) static uint64_t integer(uint64_t x) {
    uint64_t a=x+1,b=x+2,c=x+3,d=x+4,e=x+5,f=x+6,g=x+7,h=x+8;
    for(int i=0;i<20;i++) {
        a=(a^d)+g; b=(b^e)+h; c=(c^f)+a; d=(d^g)+b;
        e=(e^h)+c; f=(f^a)+d; g=(g^b)+e; h=(h^c)+f;
    }
    return a^b^c^d^e^f^g^h;
}
int main(int argc,char **argv) {
    int count=argc>1?atoi(argv[1]):200000;
    int exceptional=argc>2;
    static const uint64_t patterns[9]={0x7ff0000000000000ull,0,0x7ff8000000001234ull,0xfff8000000000000ull,0x7ff0000000001234ull,0xfff0000000005678ull,0x8000000000000000ull,0x3ff0000000000000ull,0xbff0000000000000ull};
    if(count<1)return 2;
    uint64_t h=0,k=0;
    double t=now();
    for(int j=0;j<count;j++) { double v[9];for(int i=0;i<9;i++){if(exceptional){uint64_t u=patterns[(i+j)%9];memcpy(&v[i],&u,8);}else v[i]=(double)(seed+i+(j&7));}fp(v);for(int i=0;i<9;i++){uint64_t u;memcpy(&u,&v[i],8);h=(h*1099511628211ull)^u;} }
    printf("nextuse-fp %.3f ms\n",now()-t);
    t=now();for(int j=0;j<count;j++)k^=integer(seed+(uint64_t)j);
    printf("nextuse-int %.3f ms\n",now()-t);
    printf("checksum %016llx %016llx\n",(unsigned long long)h,(unsigned long long)k);return 0;
}
