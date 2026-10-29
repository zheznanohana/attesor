// Directed ABI/semantic probes. Binary records are compared by the Python driver.
#include <Accelerate/Accelerate.h>
#include <CommonCrypto/CommonCryptor.h>
#include <compression.h>
#include <dlfcn.h>
#include <float.h>
#include <math.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
#define CHECK(x) do { if(!(x)){fprintf(stderr,"FAIL line %d: %s\n",__LINE__,#x);exit(1);} } while(0)
static uint32_t seed=1;
static uint32_t rnd(void) { seed^=seed<<13;seed^=seed>>17;seed^=seed<<5;return seed; }
static void *tag(void *p) {
#if defined(__arm64__)
    return (void*)((uintptr_t)p|0xab00000000000000ULL);
#else
    return p;
#endif
}
static void record(const char *name,int type,const void *p,uint32_t n) {
    char label[32]={0};snprintf(label,sizeof label,"%s",name);
    CHECK(fwrite(label,1,32,stdout)==32);uint32_t h[]={type,n};CHECK(fwrite(h,4,2,stdout)==2);CHECK(fwrite(p,1,n,stdout)==n);
}
static void floats(const char *name,const float *p,size_t n) { record(name,1,p,(uint32_t)n*4); }
static void bytes(const char *name,const void *p,size_t n) { record(name,2,p,(uint32_t)n); }
static uint64_t counter(const char *name) { uint64_t(*f)(void)=dlsym(RTLD_DEFAULT,name);return f?f():0; }
static const char *symbols[]={"attesor_native_vdsp_calls","attesor_native_vimage_calls","attesor_native_compression_calls","attesor_native_crypto_calls"};
static void counters(void) {for(int i=0;i<4;i++)fprintf(stderr,"%s=%llu\n",symbols[i],(unsigned long long)counter(symbols[i]));}
static void nondefault_fpcr(void) {
#if defined(__arm64__)
    uint64_t original,value;__asm__ volatile("mrs %0, fpcr":"=r"(original));
    value=original|(1ULL<<24);__asm__ volatile("msr fpcr, %0"::"r"(value));
    float a[16]={1},b[16]={2},c[16];uint64_t dsp=counter(symbols[0]),image=counter(symbols[1]);
    vDSP_vadd(a,1,b,1,c,1,16);vDSP_vmul(a,1,b,1,c,1,16);vDSP_vsmul(a,1,b,c,1,16);vDSP_vsadd(a,1,b,c,1,16);vDSP_dotpr(a,1,b,1,c,16);vDSP_sve(a,1,c,16);
    FFTSetup setup=vDSP_create_fftsetup(4,kFFTRadix2);CHECK(setup);DSPSplitComplex z={a,b};vDSP_fft_zip(setup,&z,1,4,FFT_FORWARD);vDSP_fft_zrip(setup,&z,1,4,FFT_FORWARD);vDSP_destroy_fftsetup(setup);
    vImage_Buffer s={a,4,4,16},d={c,4,4,16};CHECK(!vImageScale_PlanarF(&s,&d,NULL,0));
    CHECK(!vImageScale_ARGBFFFF(&(vImage_Buffer){a,1,4,64},&(vImage_Buffer){c,1,4,64},NULL,0));
    uint8_t p[16]={0};CHECK(!vImageConvert_Planar8toPlanarF(&(vImage_Buffer){p,4,4,4},&d,1,0,0));
    CHECK(counter(symbols[0])==dsp&&counter(symbols[1])==image);
    __asm__ volatile("msr fpcr, %0"::"r"(original));
#endif
}
static void numeric(void) {
    nondefault_fpcr();
    float a[1024],b[1024],out[2048],scalar=-.3125f;
    // Positive, cancellation-heavy, extreme-scale and denormal vectors. Odd
    // lengths/strides, negative strides, in-place and untouched padding.
    for(int kind=0;kind<5;kind++) {
        for(int i=0;i<1024;i++) {
            float x=(int32_t)rnd()/(float)INT32_MAX;
            a[i]=kind==0?fabsf(x):kind==1?x:kind==2?x*1e12f:kind==3?x*1e-35f:x*FLT_TRUE_MIN*32;
            b[i]=(int32_t)rnd()/(float)INT32_MAX;
        }
        for(int stride=1;stride<=3;stride+=2) {
            size_t n=257;memset(out,0xa5,sizeof out);vDSP_vadd(tag(a),stride,tag(b),stride,tag(out),2,n);floats("vadd",out,2*n);
            vDSP_vmul(a,stride,b,stride,out,2,n);floats("vmul",out,2*n);
            vDSP_vsmul(a,stride,tag(&scalar),out,2,n);floats("vsmul",out,2*n);
            vDSP_vsadd(a,stride,tag(&scalar),out,2,n);floats("vsadd",out,2*n);
            vDSP_dotpr(a,stride,b,stride,out,n);floats("dot",out,1);
            vDSP_sve(a,stride,out,n);floats("sum",out,1);
            vDSP_vadd(a+768,-stride,b+768,-stride,out,1,n);floats("negative-stride",out,n);
        }
        vDSP_vmul(a,1,b,1,a,1,1024);floats("inplace",a,1024);
    }
    float special[]={0,-0.0f,FLT_TRUE_MIN,-FLT_TRUE_MIN,INFINITY,-INFINITY,NAN,1,-1};
    float one[9];for(int i=0;i<9;i++)one[i]=1;
    vDSP_vadd(special,1,one,1,out,1,9);floats("special-add",out,9);
    vDSP_vmul(special,1,one,1,out,1,9);floats("special-mul",out,9);
    // Larger setup reused for smaller transforms, forward/inverse, split-real
    // packing, nested tagged pointers and non-unit strides.
    FFTSetup setup=vDSP_create_fftsetup(10,kFFTRadix2);CHECK(setup);
    for(int lg=3;lg<=9;lg+=2)for(int stride=1;stride<=2;stride++)for(int kind=0;kind<5;kind++) {
        size_t n=1UL<<lg;float re[2048],im[2048];
        memset(re,0xa5,sizeof re);memset(im,0xa5,sizeof im);
        for(size_t j=0;j<n;j++) {
            re[j*stride]=kind==0?(j==0):kind==1?sinf((float)j*.17f):kind==2?(int32_t)rnd()/1e9f:kind==3?1e-30f*(int32_t)rnd()/1e9f:FLT_TRUE_MIN*(int)(j%7);
            im[j*stride]=kind==2?(int32_t)rnd()/1e9f:0;
        }
        floats("fft-input-real",re,n*stride);floats("fft-input-imag",im,n*stride);
        DSPSplitComplex z={tag(re),tag(im)};
        vDSP_fft_zip(setup,tag(&z),stride,lg,FFT_FORWARD);floats("fft-real",re,n*stride);floats("fft-imag",im,n*stride);
        vDSP_fft_zip(setup,&z,stride,lg,FFT_INVERSE);floats("ifft-real",re,n*stride);floats("ifft-imag",im,n*stride);
        vDSP_fft_zrip(setup,&z,stride,lg,FFT_FORWARD);floats("rfft-real",re,(n/2)*stride);floats("rfft-imag",im,(n/2)*stride);
        vDSP_fft_zrip(setup,&z,stride,lg,FFT_INVERSE);floats("irfft-real",re,(n/2)*stride);floats("irfft-imag",im,(n/2)*stride);
    }
    // A guest-only FFT API uses the actual guest setup: it must not see a proxy.
    {float re[16]={1},im[16]={0},ore[16],oim[16];DSPSplitComplex z={re,im},o={ore,oim};
        vDSP_fft_zop(setup,&z,1,&o,1,4,FFT_FORWARD);floats("guest-zop",ore,16);}
    vDSP_destroy_fftsetup(setup);
    for(int i=0;i<32;i++) {setup=vDSP_create_fftsetup(4,kFFTRadix2);CHECK(setup);float re[16]={1},im[16]={0};DSPSplitComplex z={re,im};vDSP_fft_zip(setup,&z,1,4,FFT_FORWARD);vDSP_destroy_fftsetup(setup);}
    // Float images: nontrivial dimensions/row padding, up/down scale and high
    // quality filtering. Integer scaling is deliberately NOT interposed.
    for(int channels=1;channels<=4;channels+=3)for(int kind=0;kind<4;kind++)for(int flags=0;flags<2;flags++) {
        float src[71*23*4],dst[37*31*4];size_t sw=71*channels,dw=37*channels;
        for(size_t i=0;i<sw*23;i++)src[i]=kind==0?(int32_t)rnd()/1e9f:kind==1?1e-32f*(int32_t)rnd()/1e9f:kind==2?FLT_TRUE_MIN*(int)(i%11):.75f;
        memset(dst,0xa5,sizeof dst);vImage_Buffer s={tag(src),23,63,sw*4},d={tag(dst),31,29,dw*4};
        vImage_Flags f=kvImageDoNotTile|(flags?kvImageHighQualityResampling:0);
        vImage_Error r=channels==1?vImageScale_PlanarF(tag(&s),tag(&d),NULL,f):vImageScale_ARGBFFFF(tag(&s),tag(&d),NULL,f);CHECK(r==0);floats("scale-f",dst,dw*31);
        vImage_Error size=channels==1?vImageScale_PlanarF(&s,&d,NULL,f|kvImageGetTempBufferSize):vImageScale_ARGBFFFF(&s,&d,NULL,f|kvImageGetTempBufferSize);CHECK(size>=0);
        void *scratch=malloc(size?size:1);CHECK(scratch);uint64_t before=counter(symbols[1]);
        r=channels==1?vImageScale_PlanarF(&s,&d,scratch,f):vImageScale_ARGBFFFF(&s,&d,scratch,f);CHECK(!r);CHECK(counter(symbols[1])==before);floats("scratch-scale",dst,dw*31);free(scratch);
    }
    {uint8_t src[73*19];float dst[71*19];for(size_t i=0;i<sizeof src;i++)src[i]=(uint8_t)rnd();
        vImage_Buffer s={tag(src),19,67,73},d={tag(dst),19,67,71*4};memset(dst,0xa5,sizeof dst);
        CHECK(!vImageConvert_Planar8toPlanarF(tag(&s),tag(&d),1,-.5f,kvImageDoNotTile));floats("convert-f",dst,71*19);
        uint8_t isrc[67*12],idst[24*27];for(size_t i=0;i<sizeof isrc;i++)isrc[i]=(uint8_t)rnd();memset(idst,0xa5,sizeof idst);
        s=(vImage_Buffer){isrc,12,60,67};d=(vImage_Buffer){idst,27,13,24};uint64_t before=counter(symbols[1]);
        CHECK(!vImageScale_Planar8(&s,&d,NULL,kvImageDoNotTile));CHECK(before==counter(symbols[1]));bytes("guest-scale8",idst,sizeof idst);
    }
    // Zero-sized destination: compare the returned status, without inventing
    // an error contract (these OS versions accept the no-op).
    {float src[64]={0},dst[64]={0};vImage_Buffer s={src,8,8,32},d={dst,8,0,32};
        int64_t e=vImageScale_PlanarF(&s,&d,NULL,kvImageDoNotTile);bytes("scale-zero",&e,8);}
}
static compression_algorithm algs[]={COMPRESSION_LZ4,COMPRESSION_LZFSE,COMPRESSION_ZLIB};
static void encode(const char *path) {
    FILE *f=fopen(path,"wb");CHECK(f);uint8_t src[65536],dst[131072],back[65536];
    const uint32_t sizes[]={1,7,31,4096,65536};
    for(int a=0;a<3;a++)for(int kind=0;kind<3;kind++)for(int j=0;j<5;j++) {
        uint32_t n=sizes[j];for(uint32_t i=0;i<n;i++)src[i]=kind==0?0:kind==1?(uint8_t)(i*13+(i>>10)):(uint8_t)rnd();
        size_t z=compression_encode_buffer(tag(dst),sizeof dst,tag(src),n,NULL,algs[a]);CHECK(z>0);
        uint32_t h[]={algs[a],n,(uint32_t)z};CHECK(fwrite(h,4,3,f)==3);CHECK(fwrite(src,1,n,f)==n);CHECK(fwrite(dst,1,z,f)==z);
        CHECK(compression_decode_buffer(back,n,dst,z,NULL,algs[a])==n);CHECK(!memcmp(src,back,n));
        size_t sn=compression_encode_scratch_buffer_size(algs[a]);void *scratch=malloc(sn?sn:1);CHECK(scratch);
        uint64_t before=counter(symbols[2]);CHECK(compression_encode_buffer(dst,sizeof dst,src,n,scratch,algs[a])>0);CHECK(counter(symbols[2])==before);free(scratch);
        // Encoder failure: output bytes are unspecified, so check status only.
        CHECK(compression_encode_buffer(dst,0,src,n,NULL,algs[a])==0);
    }
    for(int a=0;a<3;a++) {
        size_t z=compression_encode_buffer(dst,sizeof dst,src,0,NULL,algs[a]);
        if(z)CHECK(compression_decode_buffer(back,sizeof back,dst,z,NULL,algs[a])==0);
        CHECK(compression_decode_buffer(back,sizeof back,src,0,NULL,algs[a])==0);
    }
    uint8_t bad[16];memset(bad,0xff,sizeof bad);
    for(int a=0;a<3;a++)CHECK(compression_decode_buffer(back,sizeof back,bad,sizeof bad,NULL,algs[a])==0);
    uint64_t before=counter(symbols[2]);CHECK(compression_encode_buffer(dst,sizeof dst,src,12,NULL,(compression_algorithm)0)==0);CHECK(counter(symbols[2])==before);
    fclose(f);
}
static void decode(const char *path) {
    FILE *f=fopen(path,"rb");CHECK(f);uint32_t h[3];uint8_t src[65536],encoded[131072],dst[65536];
    int count=0;
    while(fread(h,4,3,f)==3) {
        CHECK(h[1]<=sizeof src&&h[2]<=sizeof encoded);CHECK(fread(src,1,h[1],f)==h[1]);CHECK(fread(encoded,1,h[2],f)==h[2]);
        memset(dst,0xa5,sizeof dst);CHECK(compression_decode_buffer(tag(dst),h[1],tag(encoded),h[2],NULL,h[0])==h[1]);CHECK(!memcmp(src,dst,h[1]));
        // Full and prefix output contracts; independently check canary bytes.
        if(h[1]>1){memset(dst,0xa5,sizeof dst);CHECK(compression_decode_buffer(dst,h[1]-1,encoded,h[2],NULL,h[0])==h[1]-1);CHECK(!memcmp(src,dst,h[1]-1));CHECK(dst[h[1]-1]==0xa5);}
        size_t sn=compression_decode_scratch_buffer_size(h[0]);void *scratch=malloc(sn?sn:1);CHECK(scratch);uint64_t before=counter(symbols[2]);
        CHECK(compression_decode_buffer(dst,h[1],encoded,h[2],scratch,h[0])==h[1]);CHECK(!memcmp(src,dst,h[1]));CHECK(counter(symbols[2])==before);free(scratch);count++;
    }
    CHECK(count==45);CHECK(feof(f));fclose(f);
}
static void crypto(void) {
    uint8_t key[32],iv[16],src[1024],dst[1056],back[1056];for(int i=0;i<32;i++)key[i]=(uint8_t)rnd();for(int i=0;i<16;i++)iv[i]=(uint8_t)rnd();for(int i=0;i<1024;i++)src[i]=(uint8_t)rnd();
    const size_t sizes[]={0,1,15,16,17,63,64,1024};
    for(size_t kl=16;kl<=32;kl+=8)for(unsigned opt=0;opt<4;opt++)for(int i=0;i<8;i++) {
        memset(dst,0xa5,sizeof dst);size_t written=999;
        int32_t status=CCCrypt(kCCEncrypt,kCCAlgorithmAES,opt,tag(key),kl,tag(iv),tag(src),sizes[i],tag(dst),sizeof dst,tag(&written));
        bytes("aes-status",&status,4);bytes("aes-written",&written,8);
        if(status==kCCSuccess) {bytes("aes",dst,sizeof dst);size_t n=0;CHECK(CCCrypt(kCCDecrypt,kCCAlgorithmAES,opt,key,kl,iv,dst,written,back,sizeof back,&n)==0);CHECK(n==sizes[i]&&!memcmp(src,back,n));}
        else CHECK(status==kCCAlignmentError);
    }
    // NIST SP800-38A AES128 CBC first block.
    const uint8_t k[]={0x2b,0x7e,0x15,0x16,0x28,0xae,0xd2,0xa6,0xab,0xf7,0x15,0x88,0x09,0xcf,0x4f,0x3c};
    const uint8_t in[]={0x6b,0xc1,0xbe,0xe2,0x2e,0x40,0x9f,0x96,0xe9,0x3d,0x7e,0x11,0x73,0x93,0x17,0x2a};
    const uint8_t expected[]={0x76,0x49,0xab,0xac,0x81,0x19,0xb2,0x46,0xce,0xe9,0x8e,0x9b,0x12,0xe9,0x19,0x7d};
    for(int i=0;i<16;i++)iv[i]=i;size_t n=0;CHECK(!CCCrypt(0,0,0,k,16,iv,in,16,dst,sizeof dst,&n));CHECK(n==16&&!memcmp(dst,expected,16));
    for(int kind=0;kind<4;kind++) {
        n=999;memset(dst,0xa5,sizeof dst);
        int32_t e=CCCrypt(0,0,0,key,kind==0?0:kind==1?13:16,kind==3?NULL:iv,src,16,dst,kind==2?1:sizeof dst,&n);
        CHECK(kind==3?e==0:e!=0);bytes("aes-error-status",&e,4);bytes("aes-error-size",&n,8);
        if(!e)bytes("aes-nulliv",dst,n);
    }
    // Invalid PKCS7 padding: encrypt a block ending in 17, then ask for
    // padded decryption. Never pass invalid operation enums (host OS crashes).
    {uint8_t zero[16]={0};zero[15]=17;CHECK(!CCCrypt(0,0,0,key,16,iv,zero,16,dst,sizeof dst,&n));
        size_t moved=999;int32_t e=CCCrypt(1,0,1,key,16,iv,dst,n,back,sizeof back,&moved);
        CHECK(e==0||e==kCCDecodeError);bytes("aes-padding-status",&e,4);bytes("aes-padding-size",&moved,8);}
    {uint64_t before=counter(symbols[3]);size_t moved=0;
        CHECK(!CCCrypt(kCCEncrypt,kCCAlgorithm3DES,0,key,24,iv,src,16,dst,sizeof dst,&moved));
        CHECK(counter(symbols[3])==before);bytes("guest-3des",dst,moved);}
    // In-place encryption/decryption with valid padding.
    memcpy(dst,src,63);CHECK(!CCCrypt(0,0,1,key,32,iv,dst,63,dst,sizeof dst,&n));size_t m=0;CHECK(!CCCrypt(1,0,1,key,32,iv,dst,n,dst,sizeof dst,&m));CHECK(m==63&&!memcmp(dst,src,63));
}
static FFTSetup shared;
static void *worker(void *arg) {
    (void)arg;
    for(int k=0;k<16;k++) {
        float re[64]={1},im[64]={0};DSPSplitComplex z={re,im};vDSP_fft_zip(shared,&z,1,6,FFT_FORWARD);for(int i=0;i<64;i++)CHECK(re[i]==1&&im[i]==0);
        float out[64];vDSP_vadd(re,1,re,1,out,1,64);for(int i=0;i<64;i++)CHECK(out[i]==2);
        uint8_t src[4096]={1,2,3},encoded[8192],decoded[4096];
        for(int a=0;a<3;a++){size_t n=compression_encode_buffer(encoded,sizeof encoded,src,sizeof src,NULL,algs[a]);CHECK(n);CHECK(compression_decode_buffer(decoded,sizeof decoded,encoded,n,NULL,algs[a])==sizeof src);CHECK(!memcmp(src,decoded,sizeof src));}
        uint8_t key[16]={0},dst[4112];size_t n=0;CHECK(!CCCrypt(0,0,0,key,16,NULL,src,sizeof src,dst,sizeof dst,&n));CHECK(n==sizeof src);
        float pixels[64]={1},scaled[256];vImage_Buffer s={pixels,8,8,32},d={scaled,16,16,64};CHECK(!vImageScale_PlanarF(&s,&d,NULL,kvImageDoNotTile));
    }
    return NULL;
}
static void concurrent(void) {
    shared=vDSP_create_fftsetup(6,kFFTRadix2);CHECK(shared);pthread_t t[4];for(int i=0;i<4;i++)CHECK(!pthread_create(&t[i],NULL,worker,NULL));for(int i=0;i<4;i++)CHECK(!pthread_join(t[i],NULL));vDSP_destroy_fftsetup(shared);
}
static void fork_setup(void) {
    FFTSetup s=vDSP_create_fftsetup(4,kFFTRadix2);CHECK(s);
    float re[16]={1},im[16]={0};DSPSplitComplex z={re,im};vDSP_fft_zip(s,&z,1,4,FFT_FORWARD);
    pid_t pid=fork();CHECK(pid>=0);
    if(!pid){memset(re,0,sizeof re);memset(im,0,sizeof im);re[0]=1;vDSP_fft_zip(s,&z,1,4,FFT_FORWARD);for(int i=0;i<16;i++)if(re[i]!=1||im[i]!=0)_exit(3);vDSP_destroy_fftsetup(s);_exit(0);}
    int status=0;CHECK(waitpid(pid,&status,0)==pid);CHECK(WIFEXITED(status)&&WEXITSTATUS(status)==0);vDSP_destroy_fftsetup(s);
}
int main(int argc,char **argv) {
    CHECK(argc>=2);seed=argc>3?(uint32_t)strtoul(argv[3],NULL,0):1;
    if(!strcmp(argv[1],"chain")||!strcmp(argv[1],"chain-off")) {
        CHECK(argc>2);if(!strcmp(argv[1],"chain-off"))CHECK(!setenv("ATTESOR_NATIVE_LIBS","0",1));
        execl(argv[2],argv[2],argv[0],"threads",NULL);CHECK(0);
    }
    if(!strcmp(argv[1],"numeric"))numeric();else if(!strcmp(argv[1],"crypto"))crypto();else if(!strcmp(argv[1],"encode")){CHECK(argc>2);encode(argv[2]);}
    else if(!strcmp(argv[1],"decode")){CHECK(argc>2);decode(argv[2]);}else if(!strcmp(argv[1],"threads"))concurrent();else if(!strcmp(argv[1],"fork"))fork_setup();else CHECK(0);
    counters();return 0;
}
