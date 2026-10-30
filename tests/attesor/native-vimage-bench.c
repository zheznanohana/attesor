// Warm library-only A/B timing; startup/JIT warmup and checksum are not timed.
#include <Accelerate/Accelerate.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1000.+t.tv_nsec/1e6;}
int main(void){
    const size_t n=1024*1024;uint8_t *p=malloc(n),*out=malloc(n);if(!p||!out)return 1;
    for(size_t i=0;i<n;i++)p[i]=(uint8_t)(i*13+(i>>10));
    vImage_Buffer s={p,1024,1024,1024},d={out,1024,1024,1024};int16_t kernel[9]={1,1,1,1,1,1,1,1,1};
    if(vImageConvolve_Planar8(&s,&d,NULL,0,0,kernel,3,3,9,0,kvImageEdgeExtend))return 2;
    double t=now();for(int i=0;i<128;i++)if(vImageConvolve_Planar8(&s,&d,NULL,0,0,kernel,3,3,9,0,kvImageEdgeExtend))return 3;
    printf("vImage-convolve-128 %.6f ms\n",now()-t);uint64_t h=1469598103934665603ULL;
    for(size_t i=0;i<n;i++)h=(h^out[i])*1099511628211ULL;printf("bytes=%zu hash=%016llx\n",n,(unsigned long long)h);
    uint64_t (*calls)(void)=dlsym(RTLD_DEFAULT,"attesor_native_vimage_calls");fprintf(stderr,"bridge-calls=%llu\n",(unsigned long long)(calls?calls():0));
    free(p);free(out);return 0;
}
