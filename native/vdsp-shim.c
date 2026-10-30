// Keep real guest setups for APIs not interposed here. Host companions are keyed
// by identity only; neither architecture ever dereferences the other's setup.
#include <Accelerate/Accelerate.h>
#include "hostcall.h"
#include "float-env.h"
_Static_assert(sizeof(vDSP_Length)==8 && sizeof(vDSP_Stride)==8 && sizeof(DSPSplitComplex)==16, "LP64 vDSP required");
uint64_t attesor_native_vdsp_calls(void) { return hostcall(32,0,0,0,0,0); }
static FFTSetup create(vDSP_Length n, FFTRadix radix) {
    FFTSetup s=vDSP_create_fftsetup(n,radix);
    if(s)hostcall(33,(uintptr_t)s,n,radix,0,0);
    return s;
}
static void destroy(FFTSetup s) {
    hostcall(34,(uintptr_t)s,0,0,0,0);
    vDSP_destroy_fftsetup(s);
}
#define FFT(name,op) \
static void bridge_##name(FFTSetup s,const DSPSplitComplex *z,vDSP_Stride stride,vDSP_Length n,FFTDirection direction) { \
    if(!attesor_default_fpcr() || !hostcall(op,(uintptr_t)s,(uintptr_t)z,(uint64_t)stride,n,(uint64_t)(int64_t)direction)) \
        name(s,z,stride,n,direction); \
}
FFT(vDSP_fft_zip,35)
FFT(vDSP_fft_zrip,36)
#define BINARY(name,op) \
static void bridge_##name(const float *a,vDSP_Stride sa,const float *b,vDSP_Stride sb,float *c,vDSP_Stride sc,vDSP_Length n) { \
    if(!attesor_default_fpcr()){name(a,sa,b,sb,c,sc,n);return;} \
    uint64_t v[]={(uintptr_t)a,(uint64_t)sa,(uintptr_t)b,(uint64_t)sb,(uintptr_t)c,(uint64_t)sc,n}; \
    hostcall(op,(uintptr_t)v,0,0,0,0); \
}
BINARY(vDSP_vadd,37)
BINARY(vDSP_vmul,38)
#define SCALAR(name,op) \
static void bridge_##name(const float *a,vDSP_Stride sa,const float *b,float *c,vDSP_Stride sc,vDSP_Length n) { \
    if(!attesor_default_fpcr()){name(a,sa,b,c,sc,n);return;} \
    uint64_t v[]={(uintptr_t)a,(uint64_t)sa,(uintptr_t)b,0,(uintptr_t)c,(uint64_t)sc,n}; \
    hostcall(op,(uintptr_t)v,0,0,0,0); \
}
SCALAR(vDSP_vsmul,39)
SCALAR(vDSP_vsadd,40)
static void dot(const float *a,vDSP_Stride sa,const float *b,vDSP_Stride sb,float *c,vDSP_Length n) {
    if(!attesor_default_fpcr()){vDSP_dotpr(a,sa,b,sb,c,n);return;}
    uint64_t v[]={(uintptr_t)a,(uint64_t)sa,(uintptr_t)b,(uint64_t)sb,(uintptr_t)c,0,n};
    hostcall(41,(uintptr_t)v,0,0,0,0);
}
static void sum(const float *a,vDSP_Stride sa,float *c,vDSP_Length n) {
    if(!attesor_default_fpcr()){vDSP_sve(a,sa,c,n);return;}
    uint64_t v[]={(uintptr_t)a,(uint64_t)sa,0,0,(uintptr_t)c,0,n};
    hostcall(42,(uintptr_t)v,0,0,0,0);
}
struct interpose {const void *replacement,*replacee;};
#define HOOK(a,b) {(const void*)a,(const void*)b}
__attribute__((used)) static const struct interpose hooks[] __attribute__((section("__DATA,__interpose")))={
    HOOK(create,vDSP_create_fftsetup),HOOK(destroy,vDSP_destroy_fftsetup),
    HOOK(bridge_vDSP_fft_zip,vDSP_fft_zip),HOOK(bridge_vDSP_fft_zrip,vDSP_fft_zrip),
    HOOK(bridge_vDSP_vadd,vDSP_vadd),HOOK(bridge_vDSP_vmul,vDSP_vmul),
    HOOK(bridge_vDSP_vsmul,vDSP_vsmul),HOOK(bridge_vDSP_vsadd,vDSP_vsadd),
    HOOK(dot,vDSP_dotpr),HOOK(sum,vDSP_sve),
};
