#ifndef ATTESOR_IMAGEIO_BUFFER_H
#define ATTESOR_IMAGEIO_BUFFER_H
// Explicit buffer adapter, NOT transparent CGImage/CFData interposition.
// RGBX8, opaque sRGB, row stride supplied by caller; X is always 255 on decode.
// Status 0=success, 1=insufficient capacity (metadata/required bytes returned),
// -1=invalid/unsupported input or allocation/codec failure. No guest callbacks.
#include <stddef.h>
#include <stdint.h>
typedef struct { uint64_t width,height,row_bytes,bytes; } attesor_image_info;
// Decode PNG/JPEG only. With pixels=NULL/capacity=0 this queries dimensions.
int attesor_imageio_decode(const void *encoded,size_t size,void *pixels,size_t capacity,attesor_image_info *info);
// type: 1=PNG, 2=JPEG; JPEG quality integer 0..1000. Input must be opaque RGBX8.
// On short output, *written is the required size and output is not modified.
int attesor_imageio_encode(const void *pixels,const attesor_image_info *info,unsigned type,unsigned quality,
                          void *encoded,size_t capacity,size_t *written);

#endif
