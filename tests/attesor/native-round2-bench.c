#include <Accelerate/Accelerate.h>
#include <CommonCrypto/CommonCryptor.h>
#include <compression.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "../../native/imageio-buffer.h"
extern int attesor_buffer_imageio(unsigned,const uint64_t*);
#define CHECK(x) do{if(!(x)){fprintf(stderr,"FAIL %d: %s\n",__LINE__,#x);exit(1);}}while(0)
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}
static void result(const char *name,double start){printf("%s %.6f ms\n",name,now()-start);fflush(stdout);}
static int encode(const void *in,const attesor_image_info *info,unsigned type,void *out,size_t cap,size_t *n) {
    int(*f)(const void*,const attesor_image_info*,unsigned,unsigned,void*,size_t,size_t*)=dlsym(RTLD_DEFAULT,"attesor_imageio_encode");
    if(f)return f(in,info,type,900,out,cap,n);
    uint64_t v[]={(uintptr_t)in,(uintptr_t)info,type,900,(uintptr_t)out,cap,(uintptr_t)n};return attesor_buffer_imageio(66,v);
}
static int decode(const void *in,size_t n,void *out,size_t cap,attesor_image_info *info) {
    int(*f)(const void*,size_t,void*,size_t,attesor_image_info*)=dlsym(RTLD_DEFAULT,"attesor_imageio_decode");
    if(f)return f(in,n,out,cap,info);
    uint64_t v[]={(uintptr_t)in,n,(uintptr_t)out,cap,(uintptr_t)info,0,0};return attesor_buffer_imageio(65,v);
}
int main(void) {
    const size_t n=1<<20;uint8_t *p=malloc(n*4),*out=malloc(n*8),*back=malloc(n*4);CHECK(p&&out&&back);
    float *a=(float*)p,*b=(float*)out,*c=(float*)back;double t;
    for(size_t i=0;i<n/4;i++){a[i]=(i%17)/16.f;b[i]=(i%13)/16.f;}
    float r=0;vDSP_dotpr(a,1,b,1,&r,n/4);t=now();for(int i=0;i<64;i++)vDSP_dotpr(a,1,b,1,&r,n/4);result("vDSP-dot-64",t);CHECK(r>0);
    vDSP_vadd(a,1,b,1,c,1,n/4);t=now();for(int i=0;i<64;i++)vDSP_vadd(a,1,b,1,c,1,n/4);result("vDSP-add-64",t);CHECK(c[0]==0);
    FFTSetup setup=vDSP_create_fftsetup(12,kFFTRadix2);CHECK(setup);DSPSplitComplex z={a,b};
    for(int real=0;real<=1;real++){
        for(int pass=0;pass<2;pass++){
            t=now();for(int i=0;i<(pass?64:1);i++){
                memset(a,0,4096*4);memset(b,0,4096*4);a[0]=1;
                if(real)vDSP_fft_zrip(setup,&z,1,12,FFT_FORWARD);else vDSP_fft_zip(setup,&z,1,12,FFT_FORWARD);
            }
            if(pass)result(real?"vDSP-rFFT-64":"vDSP-FFT-64",t);
        }
        CHECK(a[0]==(real?2:1));
    }
    vDSP_destroy_fftsetup(setup);
    for(size_t i=0;i<1024*1024;i++)a[i]=(i%251)/250.f;
    vImage_Buffer s={a,1024,1024,1024*4},d={b,512,512,512*4};
    CHECK(!vImageScale_PlanarF(&s,&d,NULL,kvImageNoFlags));t=now();for(int i=0;i<16;i++)CHECK(!vImageScale_PlanarF(&s,&d,NULL,kvImageNoFlags));result("vImage-scaleF-16",t);
    for(size_t i=0;i<n;i++)p[i]=(uint8_t)(i*13+(i>>10));s=(vImage_Buffer){p,1024,1024,1024};d=(vImage_Buffer){out,1024,1024,1024*4};
    CHECK(!vImageConvert_Planar8toPlanarF(&s,&d,1,0,0));t=now();for(int i=0;i<16;i++)CHECK(!vImageConvert_Planar8toPlanarF(&s,&d,1,0,0));result("vImage-convertF-16",t);
    const compression_algorithm alg[]={COMPRESSION_LZ4,COMPRESSION_LZFSE,COMPRESSION_ZLIB};
    const char *en[]={"compression-LZ4-encode-16","compression-LZFSE-encode-16","compression-zlib-encode-16"};
    const char *de[]={"compression-LZ4-decode-16","compression-LZFSE-decode-16","compression-zlib-decode-16"};
    for(int k=0;k<3;k++){
        size_t size=compression_encode_buffer(out,n*2,p,n,NULL,alg[k]);CHECK(size);t=now();for(int i=0;i<16;i++)size=compression_encode_buffer(out,n*2,p,n,NULL,alg[k]);result(en[k],t);CHECK(size);
        CHECK(compression_decode_buffer(back,n,out,size,NULL,alg[k])==n);t=now();for(int i=0;i<16;i++)CHECK(compression_decode_buffer(back,n,out,size,NULL,alg[k])==n);result(de[k],t);CHECK(!memcmp(p,back,n));
    }
    uint8_t key[32]={0},iv[16]={0};size_t written=0;CHECK(!CCCrypt(0,0,0,key,32,iv,p,n,out,n*2,&written));
    t=now();for(int i=0;i<16;i++)CHECK(!CCCrypt(0,0,0,key,32,iv,p,n,out,n*2,&written));result("AES-CBC-16",t);size_t moved=0;CHECK(!CCCrypt(1,0,0,key,32,iv,out,written,back,n,&moved));CHECK(moved==n&&!memcmp(p,back,n));
    for(size_t i=0;i<256*256;i++)p[i*4+3]=255;
    attesor_image_info info={256,256,256*4,256*256*4},decoded;
    for(unsigned type=1;type<=2;type++){
        size_t size=0;CHECK(!encode(p,&info,type,out,n*2,&size));t=now();for(int i=0;i<8;i++)CHECK(!encode(p,&info,type,out,n*2,&size));result(type==1?"ImageIO-buffer-PNG-encode-8":"ImageIO-buffer-JPEG-encode-8",t);
        CHECK(!decode(out,size,back,n,&decoded));t=now();for(int i=0;i<8;i++)CHECK(!decode(out,size,back,n,&decoded));result(type==1?"ImageIO-buffer-PNG-decode-8":"ImageIO-buffer-JPEG-decode-8",t);CHECK(decoded.width==256&&decoded.height==256);if(type==1)CHECK(!memcmp(p,back,info.bytes));
    }
    const char *names[]={"vdsp","vimage","compression","crypto","imageio"};
    for(int i=0;i<5;i++){char sym[64];snprintf(sym,sizeof sym,"attesor_native_%s_calls",names[i]);uint64_t(*f)(void)=dlsym(RTLD_DEFAULT,sym);fprintf(stderr,"%s=%llu\n",names[i],(unsigned long long)(f?f():0));}
    free(p);free(out);free(back);return 0;
}
