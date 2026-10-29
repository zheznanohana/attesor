// Explicit natural loops on ARM; independently compiled scalar C on x86.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
__attribute__((noinline)) static uint64_t loop(uint64_t a, uint64_t b, uint64_t c, uint64_t n) {
#ifdef __aarch64__
    __asm__ volatile("1: add %0,%0,%1\n eor %2,%2,%0\n add %1,%1,#7\n subs %3,%3,#1\n b.ne 1b"
                     : "+r"(a), "+r"(b), "+r"(c), "+r"(n) : : "cc");
#else
    do { a += b; c ^= a; b += 7; } while (--n);
#endif
    return a ^ b ^ c;
}
__attribute__((noinline)) static uint64_t pressure(uint64_t a, uint64_t n) {
    uint64_t b=a+1,c=a+2,d=a+3,e=a+4,f=a+5,g=a+6;
    do {
        a=(a^c)+f; b=(b^d)+g; c=(c^e)+a; d=(d^f)+b;
        e=(e^g)+c; f=(f^a)+d; g=(g^b)+e;
    } while (--n);
    return a^b^c^d^e^f^g;
}
int main(int argc, char **argv) {
    unsigned count=argc>1 ? (unsigned)atoi(argv[1]) : 257;
    uint64_t h=0,k=0,seed=0x123456789abcdefULL;
    for(unsigned i=0;i<count;i++) {
        seed^=seed<<13; seed^=seed>>7; seed^=seed<<17;
        h=(h*1099511628211ULL)^loop(seed,seed^0x5555555555555555ULL,~seed,(seed&127)+1);
        k=(k*1099511628211ULL)^pressure(seed,(seed&31)+1);
    }
    printf("checksum %016llx %016llx\n",(unsigned long long)h,(unsigned long long)k);
}
