// Own-source Rosetta probes. No Apple code, no network, no protected cache access.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sysctl.h>
#include <unistd.h>

__attribute__((noinline)) double accumulate(const double *p, unsigned n) {
    double sum = 0; for (unsigned i=0;i<n;i++) sum += p[i]*0.5; return sum;
}
__attribute__((noinline)) uint64_t diamond(uint64_t x, unsigned n) {
    for(unsigned i=0;i<n;i++) { if(x&1) x=(x>>1)^0x12345; else x+=i+1; } return x;
}
__attribute__((noinline)) uint64_t leaf(uint64_t x) { return (x*17) ^ (x>>3); }
__attribute__((noinline)) uint64_t calls(uint64_t x, unsigned n) {
    uint64_t (*volatile f)(uint64_t)=leaf;
    for(unsigned i=0;i<n;i++) x=f(x+i); return x;
}
__attribute__((noinline)) uint64_t recursive(unsigned n) {
    return n<2 ? n : recursive(n-1)+recursive(n-2);
}
static int translated(void) {
    int value=0; size_t size=sizeof(value);
    return sysctlbyname("sysctl.proc_translated",&value,&size,0,0)==0 ? value : 0;
}
static int jit(void) {
#if defined(__x86_64__)
    // Independently authored position-independent x86 recurrence; forces a JIT miss.
    const unsigned char code[]={0xb8,0x07,0,0,0,0x83,0xc0,0x09,0xc3};
    size_t size=(size_t)getpagesize();
    void *p=mmap(0,size,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANON,-1,0);
    if(p==MAP_FAILED) return -1;
    memcpy(p,code,sizeof(code));
    if(mprotect(p,size,PROT_READ|PROT_EXEC)) { munmap(p,size); return -2; }
    int result=((int(*)(void))p)(); munmap(p,size); return result;
#else
    return 16; // Native reference for the authored x86 instruction sequence.
#endif
}
int main(int argc,char **argv) {
    int t=translated(); printf("translated=%d\n",t);
    if(argc>1 && !strcmp(argv[1],"--require-rosetta") && t!=1) return 3;
    double v[128]; for(unsigned i=0;i<128;i++) v[i]=(int)(i%17)-8;
    printf("accumulate=%.17g\n",accumulate(v,128));
    printf("diamond=%llu\n",(unsigned long long)diamond(7,10000));
    printf("calls=%llu\n",(unsigned long long)calls(1,10000));
    printf("recursive=%llu\n",(unsigned long long)recursive(20));
    int result=jit(); printf("jit=%d\n",result); return result==16 ? 0 : 4;
}
