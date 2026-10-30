#include "imageio-buffer.h"
#include "hostcall.h"
uint64_t attesor_native_imageio_calls(void) { return hostcall(64,0,0,0,0,0); }
int attesor_imageio_decode(const void *encoded,size_t size,void *pixels,size_t capacity,attesor_image_info *info) {
    uint64_t v[]={(uintptr_t)encoded,size,(uintptr_t)pixels,capacity,(uintptr_t)info,0,0};
    return (int)hostcall(65,(uintptr_t)v,0,0,0,0);
}
int attesor_imageio_encode(const void *pixels,const attesor_image_info *info,unsigned type,unsigned quality,void *encoded,size_t capacity,size_t *written) {
    uint64_t v[]={(uintptr_t)pixels,(uintptr_t)info,type,quality,(uintptr_t)encoded,capacity,(uintptr_t)written};
    return (int)hostcall(66,(uintptr_t)v,0,0,0,0);
}
