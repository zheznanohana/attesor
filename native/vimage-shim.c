// Only plain pixel buffers cross the boundary, never CG/CF/ObjC objects.
#include <Accelerate/Accelerate.h>
#include <stddef.h>
#include "hostcall.h"
#include "float-env.h"
_Static_assert(sizeof(vImage_Buffer)==32 && offsetof(vImage_Buffer,data)==0 &&
    offsetof(vImage_Buffer,height)==8 && offsetof(vImage_Buffer,width)==16 &&
    offsetof(vImage_Buffer,rowBytes)==24 && sizeof(vImage_Flags)==4 &&
    sizeof(vImage_Error)==8, "bridge requires Darwin LP64 vImage ABI");

__attribute__((visibility("default"))) uint64_t attesor_native_vimage_calls(void) {
    return hostcall(18,0,0,0,0,0);
}
static vImage_Error native_convolve(const vImage_Buffer *src,const vImage_Buffer *dst,void *scratch,
    vImagePixelCount x,vImagePixelCount y,const int16_t *kernel,uint32_t kh,uint32_t kw,int32_t divisor,Pixel_8 bg,vImage_Flags flags) {
    // Scratch sizes/layouts are implementation-private. Keep both queries and
    // caller-owned scratch on the original guest path. Calling the replacee
    // from this interposing image resolves to the original implementation.
    if(scratch || (flags & kvImageGetTempBufferSize))return vImageConvolve_Planar8(src,dst,scratch,x,y,kernel,kh,kw,divisor,bg,flags);
    // Fixed-width slots avoid struct padding and include arguments beyond x7.
    uint64_t args[]={(uintptr_t)src,(uintptr_t)dst,x,y,(uintptr_t)kernel,kh,kw,(uint64_t)(int64_t)divisor,bg,flags};
    return (vImage_Error)hostcall(17,(uintptr_t)args,0,0,0,0);
}
// Integer scaling remains guest-only: ARM/x86 can differ by whole pixels.
// Float scaling and Planar8 -> float conversion use the Round 2 error policy.
static vImage_Error scale_f(const vImage_Buffer *src,const vImage_Buffer *dst,void *scratch,vImage_Flags flags) {
    if(!attesor_default_fpcr() || scratch || (flags & kvImageGetTempBufferSize))return vImageScale_PlanarF(src,dst,scratch,flags);
    return (vImage_Error)hostcall(19,(uintptr_t)src,(uintptr_t)dst,flags,0,0);
}
static vImage_Error scale_argbf(const vImage_Buffer *src,const vImage_Buffer *dst,void *scratch,vImage_Flags flags) {
    if(!attesor_default_fpcr() || scratch || (flags & kvImageGetTempBufferSize))return vImageScale_ARGBFFFF(src,dst,scratch,flags);
    return (vImage_Error)hostcall(20,(uintptr_t)src,(uintptr_t)dst,flags,0,0);
}
static vImage_Error convert_f(const vImage_Buffer *src,const vImage_Buffer *dst,Pixel_F max,Pixel_F min,vImage_Flags flags) {
    if(!attesor_default_fpcr())return vImageConvert_Planar8toPlanarF(src,dst,max,min,flags);
    union {float f;uint32_t u;} hi={.f=max},lo={.f=min};
    return (vImage_Error)hostcall(21,(uintptr_t)src,(uintptr_t)dst,hi.u,lo.u,flags);
}
struct interpose { const void *replacement,*replacee; };
__attribute__((used)) static const struct interpose hooks[] __attribute__((section("__DATA,__interpose")))={
    {(const void*)native_convolve,(const void*)vImageConvolve_Planar8},
    {(const void*)scale_f,(const void*)vImageScale_PlanarF},
    {(const void*)scale_argbf,(const void*)vImageScale_ARGBFFFF},
    {(const void*)convert_f,(const void*)vImageConvert_Planar8toPlanarF},
};
