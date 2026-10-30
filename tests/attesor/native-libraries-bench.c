// Headless, data-local phase-N survey. Build identically for arm64/x86_64.
#include <Accelerate/Accelerate.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreGraphics/CoreGraphics.h>
#include <ImageIO/ImageIO.h>
#include <CommonCrypto/CommonCryptor.h>
#include <CommonCrypto/CommonDigest.h>
#include <compression.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return t.tv_sec*1000.+t.tv_nsec/1e6; }
static uint64_t hash(const void *p,size_t n) { const uint8_t *s=p;uint64_t h=1469598103934665603ULL;while(n--)h=(h^*s++)*1099511628211ULL;return h; }
static void result(const char *name,double t,const void *p,size_t n) { double elapsed=now()-t;uint64_t digest=hash(p,n);printf("%s %.6f %016llx\n",name,elapsed,(unsigned long long)digest);fflush(stdout); }
static int images(const uint8_t *pixels) {
    CGColorSpaceRef cs=CGColorSpaceCreateDeviceRGB();
    CGDataProviderRef dp=CGDataProviderCreateWithData(NULL,pixels,256*256*4,NULL);
    CGImageRef im=CGImageCreate(256,256,8,32,256*4,cs,kCGImageAlphaNoneSkipLast,dp,NULL,false,kCGRenderingIntentDefault);
    const CFStringRef types[]={CFSTR("public.png"),CFSTR("public.jpeg"),CFSTR("public.heic")};
    const char *enc[]={"ImageIO-PNG-encode","ImageIO-JPEG-encode","ImageIO-HEIC-encode"};
    const char *dec[]={"ImageIO-PNG-decode","ImageIO-JPEG-decode","ImageIO-HEIC-decode"};
    for(int k=0;k<3;k++) {
        CFMutableDataRef data=NULL;double t=now();int ok=1;
        for(int i=0;i<2;i++) {
            if(data)CFRelease(data);data=CFDataCreateMutable(NULL,0);
            CGImageDestinationRef d=CGImageDestinationCreateWithData(data,types[k],1,NULL);
            if(!d){ok=0;break;}CGImageDestinationAddImage(d,im,NULL);ok=CGImageDestinationFinalize(d);CFRelease(d);if(!ok)break;
        }
        if(ok) {
            result(enc[k],t,CFDataGetBytePtr(data),CFDataGetLength(data));t=now();
            CFDataRef decoded=NULL;
            for(int i=0;i<2;i++) {
                CGImageSourceRef s=CGImageSourceCreateWithData(data,NULL);CGImageRef img=s?CGImageSourceCreateImageAtIndex(s,0,NULL):NULL;
                if(!img){ok=0;if(s)CFRelease(s);break;}
                if(decoded)CFRelease(decoded);decoded=CGDataProviderCopyData(CGImageGetDataProvider(img));CGImageRelease(img);CFRelease(s);
            }
            if(ok)result(dec[k],t,CFDataGetBytePtr(decoded),CFDataGetLength(decoded));else printf("%s SKIP decode-failed\n",dec[k]);
            if(decoded)CFRelease(decoded);
        } else printf("%s SKIP encoder-unavailable\n%s SKIP encoder-unavailable\n",enc[k],dec[k]);
        if(data)CFRelease(data);
    }
    CGImageRelease(im);CGDataProviderRelease(dp);CGColorSpaceRelease(cs);return 0;
}
int main(int argc,char **argv) {
    const size_t n=1<<20;uint8_t *p=malloc(n),*out=malloc(n*2),*back=malloc(n);if(!p||!out||!back)return 1;
    for(size_t i=0;i<n;i++)p[i]=(uint8_t)(i*13+(i>>10));
    // An optional API prefix makes individual slow/failing frameworks isolatable.
    const char *only=argc>1?argv[1]:"";
    if(!strcmp(only,"rejected-scale")) {
        // A directed counterexample to byte-identical ARM/x86 resampling.
        // Unlike the timing survey, this mode writes ALL bytes for cmp.
        uint32_t seed=1;for(size_t i=0;i<67*12;i++){seed^=seed<<13;seed^=seed>>17;seed^=seed<<5;p[i]=(uint8_t)seed;}
        memset(out,0xa5,24*27);vImage_Buffer s={p,12,60,67},d={out,27,13,24};
        if(vImageScale_Planar8(&s,&d,NULL,kvImageDoNotTile))return 9;
        if(fwrite(out,1,24*27,stdout)!=24*27)return 10;free(p);free(out);free(back);return 0;
    }
#define WANT(s) (!*only || !strncmp(s,only,strlen(only)))
    if(WANT("compression")) {
        const compression_algorithm alg[]={COMPRESSION_LZFSE,COMPRESSION_LZ4,COMPRESSION_ZLIB};
        const char *en[]={"compression-LZFSE-encode","compression-LZ4-encode","compression-zlib-encode"};
        const char *de[]={"compression-LZFSE-decode","compression-LZ4-decode","compression-zlib-decode"};
        for(int k=0;k<3;k++) { size_t z=0,m=0;double t=now();for(int i=0;i<4;i++)z=compression_encode_buffer(out,n*2,p,n,NULL,alg[k]);if(!z)return 2;result(en[k],t,out,z);
            t=now();for(int i=0;i<4;i++)m=compression_decode_buffer(back,n,out,z,NULL,alg[k]);if(m!=n||memcmp(p,back,n))return 3;result(de[k],t,back,m); }
    }
    if(WANT("CommonCrypto")) {
        uint8_t digest[CC_SHA256_DIGEST_LENGTH];double t=now();for(int i=0;i<16;i++)CC_SHA256(p,(CC_LONG)n,digest);result("CommonCrypto-SHA256",t,digest,sizeof digest);
        uint8_t key[32]={0},iv[16]={0};size_t m=0;t=now();for(int i=0;i<8;i++)if(CCCrypt(kCCEncrypt,kCCAlgorithmAES,0,key,sizeof key,iv,p,n,out,n*2,&m)!=kCCSuccess)return 4;result("CommonCrypto-AES-CBC",t,out,m);
    }
    if(WANT("vImage")) {
        vImage_Buffer src={p,1024,1024,1024},dst={out,1024,1024,1024},small={back,512,512,512};int16_t kernel[9]={1,1,1,1,1,1,1,1,1};double t=now();
        for(int i=0;i<4;i++)if(vImageConvolve_Planar8(&src,&dst,NULL,0,0,kernel,3,3,9,0,kvImageEdgeExtend))return 5;result("vImage-convolve",t,out,n);
        t=now();for(int i=0;i<4;i++)if(vImageScale_Planar8(&src,&small,NULL,kvImageNoFlags))return 6;result("vImage-scale",t,back,512*512);
    }
    if(WANT("vDSP")) {
        float *a=(float*)p,*b=(float*)out,r=0;for(size_t i=0;i<n/4;i++){a[i]=(float)(i%17)/16;b[i]=(float)(i%13)/16;}
        double t=now();for(int i=0;i<64;i++)vDSP_dotpr(a,1,b,1,&r,n/4);result("vDSP-dot",t,&r,sizeof r);
        const size_t count=4096;FFTSetup setup=vDSP_create_fftsetup(12,kFFTRadix2);if(!setup)return 7;
        DSPSplitComplex z={(float*)p,(float*)out};t=now();for(int i=0;i<16;i++){for(size_t j=0;j<count;j++){z.realp[j]=(float)(j%7);z.imagp[j]=0;}vDSP_fft_zip(setup,&z,1,12,FFT_FORWARD);}result("vDSP-FFT",t,p,count*4);vDSP_destroy_fftsetup(setup);
    }
    if(WANT("CoreFoundation")) {
        CFMutableDictionaryRef d=CFDictionaryCreateMutable(NULL,0,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);double t=now();
        for(int k=0;k<4;k++)for(int i=0;i<4096;i++){CFStringRef s=CFStringCreateWithFormat(NULL,NULL,CFSTR("key-%d"),i);CFDictionarySetValue(d,s,s);if(!CFDictionaryGetValue(d,s))return 8;CFRelease(s);}
        CFIndex count=CFDictionaryGetCount(d);result("CoreFoundation-dictionary-string",t,&count,sizeof count);CFRelease(d);
    }
    if(WANT("ImageIO")) {for(size_t i=0;i<n;i++)p[i]=(uint8_t)(i*13+(i>>10));images(p);}
    free(p);free(out);free(back);return 0;
}
