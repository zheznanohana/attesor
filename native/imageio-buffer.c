// Host-local ImageIO objects and allocators only. Also compiled into the test
// probe for the same buffer operation in the guest (bridge disabled).
#include "imageio-buffer.h"
#include <CoreFoundation/CoreFoundation.h>
#include <CoreGraphics/CoreGraphics.h>
#include <ImageIO/ImageIO.h>
#include <limits.h>
#include <string.h>
_Static_assert(sizeof(attesor_image_info)==32, "LP64 buffer metadata required");
static int layout(uint64_t w,uint64_t h,uint64_t row) {
    return w && h && w<=SIZE_MAX/4 && row>=w*4 && h<=SIZE_MAX/row;
}
static int decode(const uint64_t *v) {
    const void *input=(const void*)(uintptr_t)v[0];size_t size=v[1];void *pixels=(void*)(uintptr_t)v[2];size_t capacity=v[3];
    attesor_image_info *info=(void*)(uintptr_t)v[4];
    if(!input||!size||size>LONG_MAX||!info)return -1;
    CFDataRef data=CFDataCreate(NULL,input,(CFIndex)size);if(!data)return -1;
    CGImageSourceRef source=CGImageSourceCreateWithData(data,NULL);CFRelease(data);if(!source)return -1;
    CFStringRef type=CGImageSourceGetType(source);
    if(!type||(!CFEqual(type,CFSTR("public.png"))&&!CFEqual(type,CFSTR("public.jpeg")))){CFRelease(source);return -1;}
    CGImageRef image=CGImageSourceCreateImageAtIndex(source,0,NULL);CFRelease(source);if(!image)return -1;
    uint64_t w=CGImageGetWidth(image),h=CGImageGetHeight(image);
    CGImageAlphaInfo alpha=CGImageGetAlphaInfo(image);
    if(!layout(w,h,w*4)||(alpha!=kCGImageAlphaNone && alpha!=kCGImageAlphaNoneSkipFirst && alpha!=kCGImageAlphaNoneSkipLast)){CGImageRelease(image);return -1;}
    *info=(attesor_image_info){w,h,w*4,w*h*4};
    if(!pixels||capacity<info->bytes){CGImageRelease(image);return 1;}
    CGColorSpaceRef color=CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
    CGContextRef context=color?CGBitmapContextCreate(pixels,w,h,8,w*4,color,kCGImageAlphaNoneSkipLast|kCGBitmapByteOrder32Big):NULL;
    if(color)CGColorSpaceRelease(color);
    if(!context){CGImageRelease(image);return -1;}
    CGContextSetBlendMode(context,kCGBlendModeCopy);CGContextDrawImage(context,CGRectMake(0,0,w,h),image);
    CGContextRelease(context);CGImageRelease(image);
    // CoreGraphics does not specify the skipped byte; our buffer API does.
    for(size_t i=3;i<info->bytes;i+=4)((uint8_t*)pixels)[i]=255;
    return 0;
}
static int encode(const uint64_t *v) {
    const void *pixels=(const void*)(uintptr_t)v[0];const attesor_image_info *info=(void*)(uintptr_t)v[1];unsigned type=(unsigned)v[2],quality=(unsigned)v[3];
    void *encoded=(void*)(uintptr_t)v[4];size_t capacity=v[5];size_t *written=(void*)(uintptr_t)v[6];
    if(!pixels||!info||!written||quality>1000||(type!=1&&type!=2)||!layout(info->width,info->height,info->row_bytes)||info->bytes<info->row_bytes*info->height)return -1;
    CGColorSpaceRef color=CGColorSpaceCreateWithName(kCGColorSpaceSRGB);if(!color)return -1;
    CGDataProviderRef provider=CGDataProviderCreateWithData(NULL,pixels,info->row_bytes*info->height,NULL);
    CGImageRef image=provider?CGImageCreate(info->width,info->height,8,32,info->row_bytes,color,kCGImageAlphaNoneSkipLast|kCGBitmapByteOrder32Big,provider,NULL,false,kCGRenderingIntentDefault):NULL;
    if(provider)CGDataProviderRelease(provider);CGColorSpaceRelease(color);if(!image)return -1;
    CFMutableDataRef data=CFDataCreateMutable(NULL,0);if(!data){CGImageRelease(image);return -1;}
    CGImageDestinationRef dest=CGImageDestinationCreateWithData(data,type==1?CFSTR("public.png"):CFSTR("public.jpeg"),1,NULL);
    float q=quality/1000.f;CFNumberRef number=CFNumberCreate(NULL,kCFNumberFloatType,&q);
    const void *key=kCGImageDestinationLossyCompressionQuality,*value=number;
    CFDictionaryRef options=number?CFDictionaryCreate(NULL,&key,&value,1,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks):NULL;
    int ok=0;if(dest&&options){CGImageDestinationAddImage(dest,image,options);ok=CGImageDestinationFinalize(dest);}
    if(options)CFRelease(options);if(number)CFRelease(number);if(dest)CFRelease(dest);CGImageRelease(image);
    if(!ok){CFRelease(data);return -1;}
    *written=(size_t)CFDataGetLength(data);
    if(!encoded||capacity<*written){CFRelease(data);return 1;}
    memcpy(encoded,CFDataGetBytePtr(data),*written);CFRelease(data);return 0;
}
// Slots have already had pointer tags stripped by the Rust boundary.
int attesor_buffer_imageio(unsigned op,const uint64_t *v) {
    return op==65?decode(v):op==66?encode(v):-1;
}
