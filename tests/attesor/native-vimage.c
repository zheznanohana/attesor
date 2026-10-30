// Full-byte native/translated differential corpus (not hashes or timing).
#include <Accelerate/Accelerate.h>
#include <dlfcn.h>
#include <pthread.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
extern char **environ;
#define CHECK(x) do { if(!(x)){fprintf(stderr,"FAIL line %d: %s\n",__LINE__,#x);exit(1);} }while(0)
static uint64_t calls(void) { uint64_t (*f)(void)=dlsym(RTLD_DEFAULT,"attesor_native_vimage_calls");return f?f():0; }
static uint32_t next(uint32_t *s) {*s^=*s<<13;*s^=*s>>17;*s^=*s<<5;return *s;}
static void *tag(const void *p) {
#ifdef __aarch64__
    return (void*)((uintptr_t)p|(UINT64_C(0xab)<<56));
#else
    return (void*)p;
#endif
}
static void emit(FILE *f,int64_t r,const uint8_t *p,size_t n) { CHECK(fwrite(&r,sizeof r,1,f)==1);CHECK(fwrite(p,1,n,f)==n); }
static void corpus(FILE *f,uint32_t seed,int count) {
    for(int k=0;k<count;k++) {
        size_t w=9+next(&seed)%71,h=7+next(&seed)%53,sw=w+7,stride=w+13;
        uint8_t *src=malloc(sw*h),*dst=malloc(stride*h),*original=malloc(sw*h);CHECK(src&&dst&&original);
        for(size_t j=0;j<sw*h;j++)src[j]=(uint8_t)next(&seed);memcpy(original,src,sw*h);
        vImage_Buffer s={src,h,w,sw},d={dst,h,w,stride};
        uint32_t kh=1+2*(k%4),kw=1+2*((k/4)%4);int16_t kernel[49];
        for(unsigned j=0;j<kh*kw;j++)kernel[j]=(k%4==2)?1+(int16_t)(next(&seed)%3):(int16_t)(next(&seed)%7)-3;
        if(k%8==0)for(unsigned j=0;j<kh*kw;j++)kernel[j]=(j&1)?INT16_MIN:INT16_MAX;
        const vImage_Flags edges[]={kvImageBackgroundColorFill,kvImageEdgeExtend,kvImageTruncateKernel,kvImageCopyInPlace};
        vImage_Flags flags=edges[k%4];if(k&2)flags|=kvImageDoNotTile;
        int32_t divisor=(k%3==0)?0:(k&1)?9:-9;memset(dst,0xa5,stride*h);
        int64_t r=vImageConvolve_Planar8(&s,&d,NULL,0,0,kernel,kh,kw,divisor,37,flags);CHECK(r==0);emit(f,r,dst,stride*h);
        // Private scratch queries and caller scratch must never reach the host.
        uint64_t before=calls();vImage_Error size=vImageConvolve_Planar8(&s,&d,NULL,0,0,kernel,kh,kw,divisor,37,flags|kvImageGetTempBufferSize);CHECK(size>=0);
        void *scratch=malloc((size_t)size+1);CHECK(scratch);memset(dst,0xa5,stride*h);
        r=vImageConvolve_Planar8(&s,&d,scratch,0,0,kernel,kh,kw,divisor,37,flags);CHECK(r==0);if(count==32)CHECK(calls()==before);emit(f,r,dst,stride*h);free(scratch);
        // Nonzero source ROI, and 11-argument marshalling with tagged inputs.
        d.width=w-4;d.height=h-2;memset(dst,0xa5,stride*h);s.data=tag(src);d.data=tag(dst);
        r=vImageConvolve_Planar8(tag(&s),tag(&d),NULL,2,1,tag(kernel),kh,kw,9,219,kvImageBackgroundColorFill);CHECK(r==0);emit(f,r,dst,stride*h);
        CHECK(!memcmp(src,original,sw*h));free(src);free(original);free(dst);
    }
}
static void errors(FILE *f) {
    uint8_t src[256],dst[256];memset(src,3,sizeof src);int16_t kernel[9]={1,1,1,1,1,1,1,1,1};
    vImage_Buffer s={src,16,16,16},d={dst,16,16,16};int64_t r;
    memset(dst,0xa5,sizeof dst);r=vImageConvolve_Planar8(&s,&d,NULL,0,0,kernel,2,3,9,0,kvImageEdgeExtend);CHECK(r<0);emit(f,r,dst,sizeof dst);
    memset(dst,0xa5,sizeof dst);r=vImageConvolve_Planar8(&s,&d,NULL,20,0,kernel,3,3,9,0,kvImageEdgeExtend);CHECK(r<0);emit(f,r,dst,sizeof dst);
    memset(dst,0xa5,sizeof dst);r=vImageConvolve_Planar8(&s,&d,NULL,0,20,kernel,3,3,9,0,kvImageEdgeExtend);CHECK(r<0);emit(f,r,dst,sizeof dst);
    memset(dst,0xa5,sizeof dst);r=vImageConvolve_Planar8(&s,&d,NULL,0,0,NULL,3,3,9,0,kvImageEdgeExtend);CHECK(r<0);emit(f,r,dst,sizeof dst);
    memset(dst,0xa5,sizeof dst);r=vImageConvolve_Planar8(&s,&d,NULL,0,0,kernel,3,3,9,0,kvImageNoFlags);CHECK(r<0);emit(f,r,dst,sizeof dst);
    memset(dst,0xa5,sizeof dst);r=vImageConvolve_Planar8(&s,&d,NULL,0,0,kernel,3,3,9,0,kvImageBackgroundColorFill|kvImageEdgeExtend);CHECK(r<0);emit(f,r,dst,sizeof dst);
    d.width=17;memset(dst,0xa5,sizeof dst);r=vImageConvolve_Planar8(&s,&d,NULL,0,0,kernel,3,3,9,0,kvImageEdgeExtend);CHECK(r<0);emit(f,r,dst,sizeof dst);
}
static void large(FILE *f) {
    // Exercise the internally tiled implementation, not just tiny scalar tails.
    size_t n=1024*1024;uint8_t *src=malloc(n),*dst=malloc(n);CHECK(src&&dst);
    for(size_t i=0;i<n;i++)src[i]=(uint8_t)(i*13+(i>>10));vImage_Buffer s={src,1024,1024,1024},d={dst,1024,1024,1024};
    int16_t kernel[9]={1,1,1,1,1,1,1,1,1};int64_t r=vImageConvolve_Planar8(&s,&d,NULL,0,0,kernel,3,3,9,0,kvImageEdgeExtend);CHECK(r==0);emit(f,r,dst,n);
    free(src);free(dst);
}
struct worker {uint32_t seed;char *data;size_t n;};
static void *work(void *p) {struct worker *w=p;FILE *f=open_memstream(&w->data,&w->n);CHECK(f);corpus(f,w->seed,8);CHECK(!fclose(f));return NULL;}
int main(int argc,char **argv) {
    const char *mode=argc>1?argv[1]:"--off";
    if(!strcmp(mode,"--chain")||!strcmp(mode,"--chain-off")) {
        int off=!strcmp(mode,"--chain-off");if(off)setenv("ATTESOR_NATIVE_LIBS","0",1);
        char *av[]={argv[2],argv[0],off?"--off":"--bridge",NULL};pid_t p;int st;
        CHECK(!posix_spawn(&p,argv[2],NULL,NULL,av,environ));CHECK(waitpid(p,&st,0)==p);CHECK(WIFEXITED(st)&&WEXITSTATUS(st)==0);return 0;
    }
    uint32_t seed=argc>2?(uint32_t)strtoul(argv[2],NULL,0):1;corpus(stdout,seed,32);errors(stdout);large(stdout);
    pthread_t t[4];struct worker w[4]={0};for(int i=0;i<4;i++){w[i].seed=seed+(uint32_t)i+100;CHECK(!pthread_create(&t[i],NULL,work,&w[i]));}
    for(int i=0;i<4;i++){CHECK(!pthread_join(t[i],NULL));CHECK(fwrite(w[i].data,1,w[i].n,stdout)==w[i].n);free(w[i].data);}
    uint64_t n=calls();if(!strcmp(mode,"--bridge"))CHECK(n==136);else CHECK(n==0);
    fprintf(stderr,"native-vimage: PASS seed=%u bridge-calls=%llu\n",seed,(unsigned long long)n);return 0;
}
