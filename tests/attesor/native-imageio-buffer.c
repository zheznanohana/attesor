#include "../../native/imageio-buffer.h"
#include <dlfcn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreGraphics/CoreGraphics.h>
#include <ImageIO/ImageIO.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define CHECK(x) do{if(!(x)){fprintf(stderr,"FAIL %d: %s\n",__LINE__,#x);exit(1);}}while(0)
extern int attesor_buffer_imageio(unsigned,const uint64_t*);
static void *tag(const void *p) {
#if defined(__arm64__)
    return p?(void*)((uintptr_t)p|0xab00000000000000ULL):NULL;
#else
    return (void*)p;
#endif
}
static int decode(const void *in,size_t n,void *out,size_t cap,attesor_image_info *info) {
    int(*f)(const void*,size_t,void*,size_t,attesor_image_info*)=dlsym(RTLD_DEFAULT,"attesor_imageio_decode");
    if(f)return f(tag(in),n,tag(out),cap,tag(info));
    uint64_t v[]={(uintptr_t)in,n,(uintptr_t)out,cap,(uintptr_t)info,0,0};return attesor_buffer_imageio(65,v);
}
static int encode(const void *in,const attesor_image_info *info,unsigned type,unsigned quality,void *out,size_t cap,size_t *n) {
    int(*f)(const void*,const attesor_image_info*,unsigned,unsigned,void*,size_t,size_t*)=dlsym(RTLD_DEFAULT,"attesor_imageio_encode");
    if(f)return f(tag(in),tag(info),type,quality,tag(out),cap,tag(n));
    uint64_t v[]={(uintptr_t)in,(uintptr_t)info,type,quality,(uintptr_t)out,cap,(uintptr_t)n};return attesor_buffer_imageio(66,v);
}
static void counters(void) {
    uint64_t(*f)(void)=dlsym(RTLD_DEFAULT,"attesor_native_imageio_calls");fprintf(stderr,"imageio-calls=%llu\n",(unsigned long long)(f?f():0));
}
static void save(const char *dir,const char *base,const void *p,size_t n) {
    char path[4096];CHECK(snprintf(path,sizeof path,"%s/%s",dir,base)<(int)sizeof path);FILE *f=fopen(path,"wb");CHECK(f);CHECK(fwrite(p,1,n,f)==n);CHECK(!fclose(f));
}
static void encode_all(const char *dir) {
    const size_t widths[]={1,17,256},heights[]={1,13,192};
    uint8_t *pixels=malloc(256*192*4),*encoded=malloc(256*192*8);CHECK(pixels&&encoded);
    for(int dim=0;dim<3;dim++)for(int pattern=0;pattern<3;pattern++) {
        size_t w=widths[dim],h=heights[dim],n=w*h*4;uint32_t seed=42;
        for(size_t y=0;y<h;y++)for(size_t x=0;x<w;x++)for(size_t c=0;c<4;c++) {
            seed^=seed<<13;seed^=seed>>17;seed^=seed<<5;
            pixels[(y*w+x)*4+c]=c==3?255:pattern==0?(uint8_t)((x*3+y*2+c*37)%256):pattern==1?(((x/8+y/8)%2)*200+23):(uint8_t)seed;
        }
        char name[64];snprintf(name,sizeof name,"%d-%d.raw",dim,pattern);save(dir,name,pixels,n);
        attesor_image_info info={w,h,w*4,n};
        for(unsigned type=1;type<=2;type++) {
            size_t needed=0;CHECK(encode(pixels,&info,type,900,NULL,0,&needed)==1);CHECK(needed>0&&needed<256*192*8);
            size_t written=0;uint8_t canary=0xa5;CHECK(encode(pixels,&info,type,900,&canary,1,&written)==1);CHECK(canary==0xa5&&written==needed);
            CHECK(encode(pixels,&info,type,900,encoded,256*192*8,&written)==0);CHECK(written==needed);
            snprintf(name,sizeof name,"%d-%d.%s",dim,pattern,type==1?"png":"jpg");save(dir,name,encoded,written);
        }
    }
    size_t written=999;attesor_image_info bad={0,1,4,4};CHECK(encode(pixels,&bad,1,900,encoded,100,&written)==-1);CHECK(written==999);
    bad=(attesor_image_info){1,1,4,4};CHECK(encode(pixels,&bad,3,900,encoded,100,&written)==-1);CHECK(encode(pixels,&bad,1,1001,encoded,100,&written)==-1);
    CHECK(encode(pixels,&bad,1,900,encoded,100,NULL)==-1);
    attesor_image_info info;const uint8_t invalid[]={1,2,3,4,5,6,7};CHECK(decode(invalid,sizeof invalid,pixels,100,&info)==-1);CHECK(decode(invalid,0,pixels,100,&info)==-1);
    // Padded input rows and ignored X bytes are part of the adapter contract.
    {uint8_t padded[32],back[24];memset(padded,0xa5,sizeof padded);
        for(size_t y=0;y<2;y++)for(size_t x=0;x<3;x++){for(size_t c=0;c<3;c++)padded[y*16+x*4+c]=(uint8_t)(y*29+x*13+c*17);padded[y*16+x*4+3]=17;}
        attesor_image_info in={3,2,16,sizeof padded},out;size_t size=0;
        CHECK(!encode(padded,&in,1,900,encoded,256*192*8,&size));CHECK(!decode(encoded,size,back,sizeof back,&out));
        CHECK(out.width==3&&out.height==2&&out.row_bytes==12);
        for(size_t y=0;y<2;y++)for(size_t x=0;x<3;x++){CHECK(!memcmp(back+y*12+x*4,padded+y*16+x*4,3));CHECK(back[y*12+x*4+3]==255);}}
    // Real alpha PNG is deliberately unsupported, not silently flattened.
    {uint8_t rgba[4]={128,0,0,128};CGColorSpaceRef cs=CGColorSpaceCreateWithName(kCGColorSpaceSRGB);CHECK(cs);
        CGContextRef context=CGBitmapContextCreate(rgba,1,1,8,4,cs,kCGImageAlphaPremultipliedLast|kCGBitmapByteOrder32Big);CHECK(context);
        CGImageRef image=CGBitmapContextCreateImage(context);CHECK(image);CFMutableDataRef data=CFDataCreateMutable(NULL,0);CHECK(data);
        CGImageDestinationRef dest=CGImageDestinationCreateWithData(data,CFSTR("public.png"),1,NULL);CHECK(dest);CGImageDestinationAddImage(dest,image,NULL);CHECK(CGImageDestinationFinalize(dest));
        attesor_image_info out;CHECK(decode(CFDataGetBytePtr(data),CFDataGetLength(data),pixels,256*192*4,&out)==-1);
        CFRelease(dest);CFRelease(data);CGImageRelease(image);CGContextRelease(context);CGColorSpaceRelease(cs);}
    free(pixels);free(encoded);
}
static void decode_all(const char *input,const char *output) {
    const size_t widths[]={1,17,256},heights[]={1,13,192};
    uint8_t *pixels=malloc(256*192*4+16),*encoded=malloc(256*192*8);CHECK(pixels&&encoded);
    for(int dim=0;dim<3;dim++)for(int pattern=0;pattern<3;pattern++)for(int type=1;type<=2;type++) {
        char base[64],path[4096];snprintf(base,sizeof base,"%d-%d.%s",dim,pattern,type==1?"png":"jpg");
        CHECK(snprintf(path,sizeof path,"%s/%s",input,base)<(int)sizeof path);FILE *f=fopen(path,"rb");CHECK(f);size_t size=fread(encoded,1,256*192*8,f);CHECK(feof(f));fclose(f);
        attesor_image_info info={0};CHECK(decode(encoded,size,NULL,0,&info)==1);CHECK(info.width==widths[dim]&&info.height==heights[dim]&&info.row_bytes==widths[dim]*4&&info.bytes==widths[dim]*heights[dim]*4);
        memset(pixels,0xa5,256*192*4+16);CHECK(decode(encoded,size,pixels,info.bytes-1,&info)==1);CHECK(pixels[0]==0xa5);
        CHECK(!decode(encoded,size,pixels,info.bytes,&info));for(size_t i=0;i<16;i++)CHECK(pixels[info.bytes+i]==0xa5);
        strcat(base,".raw");save(output,base,pixels,info.bytes);
    }
    free(pixels);free(encoded);
}
static void *worker(void *p) {
    (void)p;uint8_t in[17*13*4],out[8192],back[17*13*4];memset(in,128,sizeof in);attesor_image_info info={17,13,17*4,sizeof in},result;
    for(int i=0;i<4;i++){size_t n=0;CHECK(!encode(in,&info,1,900,out,sizeof out,&n));CHECK(!decode(out,n,back,sizeof back,&result));for(size_t j=0;j<sizeof in;j++)CHECK(back[j]==(j%4==3?255:128));}
    return NULL;
}
int main(int argc,char **argv) {
    CHECK(argc>=2);
    if(!strcmp(argv[1],"encode")){CHECK(argc==3);encode_all(argv[2]);}
    else if(!strcmp(argv[1],"decode")){CHECK(argc==4);decode_all(argv[2],argv[3]);}
    else if(!strcmp(argv[1],"threads")){pthread_t t[4];for(int i=0;i<4;i++)CHECK(!pthread_create(&t[i],NULL,worker,NULL));for(int i=0;i<4;i++)CHECK(!pthread_join(t[i],NULL));}
    else CHECK(0);
    counters();return 0;
}
