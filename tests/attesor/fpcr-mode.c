// Real guest MSR FPCR -> FSQRT sequence; independent ARM result-bit oracle.
#include <stdint.h>
#include <stdio.h>
__attribute__((noinline)) static uint64_t calc(uint64_t input, uint64_t mode) {
 uint64_t result;
 __asm__ volatile("mrs x9,fpcr\n fmov d1,%1\n msr fpcr,%2\n fsqrt d0,d1\n fmov %0,d0\n msr fpcr,x9"
                  : "=&r"(result) : "r"(input),"r"(mode) : "x9","v0","v1","memory");
 return result;
}
int main(void) {
 uint64_t inputs[]={0xbff0000000000000ULL,0xfff0000000000000ULL,0x8000000000000000ULL,0,
  0x7ff0000000000000ULL,0x4010000000000000ULL,0x7ff8000000001234ULL,0xfff8000000001234ULL,
  0x7ff0000000001234ULL,0xfff0000000001234ULL};
 uint64_t hash=0;unsigned count=0;
 for(unsigned n=0;n<10;n++)for(unsigned dn=0;dn<2;dn++) {
  uint64_t x=inputs[n],m=x&0x7fffffffffffffffULL,expected;
  if(m>0x7ff0000000000000ULL)expected=dn?0x7ff8000000000000ULL:x|0x0008000000000000ULL;
  else if((x>>63)&&m)expected=0x7ff8000000000000ULL;
  else if(m==0||m==0x7ff0000000000000ULL)expected=x;
  else expected=0x4000000000000000ULL;
  uint64_t actual=calc(x,(uint64_t)dn<<25);
  if(actual!=expected){printf("FAIL %016llx DN%u %016llx expected%016llx\n",(unsigned long long)x,dn,(unsigned long long)actual,(unsigned long long)expected);return 1;}
  hash=hash*1099511628211ULL^actual;count++;
 }
 printf("fpcr-boundary PASS cases=%u checksum=%016llx\n",count,(unsigned long long)hash);return 0;
}
