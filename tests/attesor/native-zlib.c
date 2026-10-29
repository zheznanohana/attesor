// Stateless bridge semantics and directed error cases, independently compared
// with the native x86 libz. No timings here; see native-zlib-bench.c.
#include <zlib.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <spawn.h>
#include <pthread.h>
#include <sys/wait.h>
extern char **environ;
static int fail;
#define CHECK(x) do { if (!(x)) { fprintf(stderr,"FAIL line %d: %s\n",__LINE__,#x); fail=1; } } while(0)
static uint64_t calls(void) { uint64_t (*f)(void) = dlsym(RTLD_DEFAULT,"attesor_native_calls"); return f ? f() : 0; }
static void *thread_work(void *arg) {
  Bytef input[1024], output[2048], back[1024]; memset(input, (int)(uintptr_t)arg, sizeof input);
  for(int i=0;i<16;i++) {
    uLongf n=sizeof output,m=sizeof back;
    if(compress2(output,&n,input,sizeof input,6)!=Z_OK || uncompress(back,&m,output,n)!=Z_OK || m!=sizeof input || memcmp(back,input,m)) return (void*)1;
    if(crc32(0,(const Bytef*)"123456789",9)!=0xcbf43926UL) return (void*)1;
  }
  return NULL;
}
static uintptr_t tagged(const void *p) {
#ifdef __aarch64__
  return (uintptr_t)p | (UINT64_C(0xab)<<56);
#else
  return (uintptr_t)p;
#endif
}
int main(int argc,char **argv) {
  if(argc>1 && (!strcmp(argv[1],"--chain") || !strcmp(argv[1],"--chain-off"))) {
    int off=!strcmp(argv[1],"--chain-off");if(off)setenv("ATTESOR_NATIVE_LIBS","0",1);
    char *av[]={argv[2],argv[0],off?"--off":"--bridge-child",NULL}; pid_t p; int st;
    int r=posix_spawn(&p,argv[2],NULL,NULL,av,environ); CHECK(!r);
    if(r) return 1; CHECK(waitpid(p,&st,0)==p); CHECK(WIFEXITED(st)&&WEXITSTATUS(st)==0);
    puts("native-chain: PASS"); return fail;
  }
  CHECK(crc32(0,(const Bytef*)"123456789",9)==0xcbf43926UL);
  CHECK(adler32(1,(const Bytef*)"123456789",9)==0x091e01deUL);
  CHECK(crc32(123,NULL,0)==0); CHECK(adler32(123,NULL,0)==1);
  const Bytef empty=0; CHECK(crc32(0x12345678,&empty,0)==0x12345678);
  CHECK(adler32(0x12345678,&empty,0)==0x12345678);
  size_t n=4*1024*1024; Bytef *src=malloc(n),*dst=malloc(compressBound(n)),*back=malloc(n);
  CHECK(src && dst && back); if(!src||!dst||!back)return 1;
  for(int pattern=0;pattern<2;pattern++) {
    uint32_t x=1; for(size_t i=0;i<n;i++) { x^=x<<13;x^=x>>17;x^=x<<5;src[i]=pattern?(Bytef)x:(Bytef)(i%17); }
    uLong crc=crc32(0,src,(uInt)n),adler=adler32(1,src,(uInt)n);
    uLong c=0,a=1;for(size_t p=0;p<n;p+=4096){c=crc32(c,src+p,4096);a=adler32(a,src+p,4096);}
    CHECK(c==crc && a==adler);
    for(int level=0;level<=9;level+=3){
      uLongf cap=compressBound(n);CHECK(compress2(dst,&cap,src,n,level)==Z_OK);
      uLongf len=n;CHECK(uncompress(back,&len,dst,cap)==Z_OK);CHECK(len==n&&!memcmp(src,back,n));
      len=7;int r=uncompress(back,&len,dst,cap);CHECK(r==Z_BUF_ERROR);CHECK(len==7);
      printf("pattern%d level%d crc%08lx adler%08lx short=%d/%lu\n",pattern,level,crc,adler,r,len);
    }
  }
  uLongf len=16;int r=compress2(dst,&len,src,n,99);CHECK(r==Z_STREAM_ERROR);printf("invalid-level=%d/%lu\n",r,len);
  len=1;r=compress2(dst,&len,src,n,6);CHECK(r==Z_BUF_ERROR);printf("compress-short=%d/%lu\n",r,len);
  const Bytef invalid[]={0x78,0x9c,0xff,0xff,0xff,0xff};len=n;r=uncompress(back,&len,invalid,sizeof invalid);CHECK(r==Z_DATA_ERROR);printf("invalid-data=%d/%lu\n",r,len);
  static const Bytef abc[]={0x78,0x9c,0x4b,0x4c,0x4a,0x06,0x00,0x02,0x4d,0x01,0x27};
  len=n;CHECK(uncompress(back,&len,abc,sizeof abc)==Z_OK);CHECK(len==3&&!memcmp(back,"abc",3));
  len=compressBound(0);CHECK(compress2(dst,&len,&empty,0,6)==Z_OK);uLongf zero=0;CHECK(uncompress(back,&zero,dst,len)==Z_OK);CHECK(zero==0);
  const Bytef *tag=(const Bytef*)tagged("123456789");
  CHECK(crc32(0,tag,9)==0xcbf43926UL); CHECK(adler32(1,tag,9)==0x091e01deUL);
  len=compressBound(9);CHECK(compress2((Bytef*)tagged(dst),(uLongf*)tagged(&len),tag,9,6)==Z_OK);
  uLongf taggedlen=9;CHECK(uncompress((Bytef*)tagged(back),(uLongf*)tagged(&taggedlen),(const Bytef*)tagged(dst),len)==Z_OK);CHECK(taggedlen==9&&!memcmp(back,"123456789",9));
  if(!getenv("NATIVE_ZLIB_SINGLE_THREAD")) {
    pthread_t threads[4]; for(int i=0;i<4;i++) CHECK(!pthread_create(&threads[i],NULL,thread_work,(void*)(uintptr_t)i));
    for(int i=0;i<4;i++){void *r=(void*)1;CHECK(!pthread_join(threads[i],&r));CHECK(!r);}
  }
  uint64_t count=calls();
  if(argc>1&&(!strcmp(argv[1],"--bridge")||!strcmp(argv[1],"--bridge-child"))) CHECK(count>0);
  if(argc>1&&!strcmp(argv[1],"--off"))CHECK(count==0);
  fprintf(stderr,"bridge-calls=%llu\n",(unsigned long long)count);
  puts(fail?"native-zlib: FAIL":"native-zlib: PASS");free(src);free(dst);free(back);return fail;
}
