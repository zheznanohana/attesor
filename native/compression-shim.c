#include <compression.h>
#include "hostcall.h"
uint64_t attesor_native_compression_calls(void) { return hostcall(48,0,0,0,0,0); }
static int supported(compression_algorithm a) {
    return a==COMPRESSION_LZ4 || a==COMPRESSION_LZFSE || a==COMPRESSION_ZLIB;
}
#define BUFFER(name,op) \
static size_t bridge_##name(uint8_t *dst,size_t capacity,const uint8_t *src,size_t size,void *scratch,compression_algorithm alg) { \
    if(scratch || !supported(alg))return name(dst,capacity,src,size,scratch,alg); \
    return hostcall(op,(uintptr_t)dst,capacity,(uintptr_t)src,size,alg); \
}
BUFFER(compression_encode_buffer,49)
BUFFER(compression_decode_buffer,50)
struct interpose {const void *replacement,*replacee;};
__attribute__((used)) static const struct interpose hooks[] __attribute__((section("__DATA,__interpose")))={
    {(const void*)bridge_compression_encode_buffer,(const void*)compression_encode_buffer},
    {(const void*)bridge_compression_decode_buffer,(const void*)compression_decode_buffer},
};
