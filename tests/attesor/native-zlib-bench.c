// Time only large pure-library calls. Print checksum for independent comparison.
#include <zlib.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1000.+t.tv_nsec/1e6;}
int main(void){size_t n=16<<20;Bytef *p=malloc(n),*out=malloc(compressBound(n)),*back=malloc(n);if(!p||!out||!back)return 1;
for(size_t i=0;i<n;i++)p[i]=(Bytef)(i*13+(i>>10));uLong c=0,a=1;double t=now();for(int i=0;i<32;i++)c=crc32(c,p,n);printf("native-crc %.3f ms\n",now()-t);
t=now();for(int i=0;i<32;i++)a=adler32(a,p,n);printf("native-adler %.3f ms\n",now()-t);
uLongf len=compressBound(n);t=now();if(compress2(out,&len,p,n,6)!=Z_OK)return 1;printf("native-deflate %.3f ms\n",now()-t);
uLongf size=n;t=now();if(uncompress(back,&size,out,len)!=Z_OK||size!=n||memcmp(p,back,n))return 1;printf("native-inflate %.3f ms\n",now()-t);
printf("checksum %08lx %08lx %08lx\n",c,a,crc32(0,back,n));free(p);free(out);free(back);return 0;}
