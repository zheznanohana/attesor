// V-loop pressure/width regression: independently authored, no Apple code.
#include <stdint.h>
#include <stdio.h>
#include <string.h>
int main(void) {
#ifdef SINGLE
  typedef float real;
#define REG "s"
#else
  typedef double real;
#define REG "d"
#endif
  real source=1.25, result=0; uint64_t count=1000;
#ifdef __aarch64__
  __asm__ volatile (
    "fmov " REG "0,#0.5\n"
    "fmov " REG "1,#1.0\n"
    "fmov " REG "2,#1.0\n"
    "fmov " REG "3,#1.0\n"
    "fmov " REG "4,#1.0\n"
    "fmov " REG "5,#1.0\n"
    "fmov " REG "6,#1.0\n"
    "fmov " REG "7,#1.0\n"
    "fmov " REG "8,#1.0\n"
    "fmov " REG "9,#1.0\n"
    "fmov " REG "10,#1.0\n"
    "fmov " REG "11,#1.0\n"
    "fmov " REG "12,#1.0\n"
    "fmov " REG "13,#1.0\n"
    "fmov " REG "14,#1.0\n"
    "fmov " REG "15,#1.0\n"
    "fmov " REG "16,#1.0\n"
    "fmov " REG "17,#1.0\n"
    "fmov " REG "18,#1.0\n"
    "fmov " REG "19,#1.0\n"
    "fmov " REG "20,#1.0\n"
    "fmov " REG "21,#1.0\n"
    "fmov " REG "22,#1.0\n"
    "fmov " REG "23,#1.0\n"
    "1:\n"
    "ldr " REG "9,[%[src]]\n"
    "fadd " REG "0," REG "0," REG "1\n"
    "fadd " REG "0," REG "0," REG "2\n"
    "fadd " REG "0," REG "0," REG "3\n"
    "fadd " REG "0," REG "0," REG "4\n"
    "fadd " REG "0," REG "0," REG "5\n"
    "fadd " REG "0," REG "0," REG "6\n"
    "fadd " REG "0," REG "0," REG "7\n"
    "fadd " REG "0," REG "0," REG "8\n"
#ifdef VCHAIN
    // Separately rounded MUL -> ADD, with a destructive high-bank source.
    // V10 is exactly 1, so the independent x86 reference remains unchanged.
    "fmul " REG "9," REG "9," REG "10\n"
#endif
    "fadd " REG "0," REG "0," REG "9\n"
    "fadd " REG "0," REG "0," REG "10\n"
    "fadd " REG "0," REG "0," REG "11\n"
    "fadd " REG "0," REG "0," REG "12\n"
    "fadd " REG "0," REG "0," REG "13\n"
    "fadd " REG "0," REG "0," REG "14\n"
    "fadd " REG "0," REG "0," REG "15\n"
    "fadd " REG "0," REG "0," REG "16\n"
    "fadd " REG "0," REG "0," REG "17\n"
    "fadd " REG "0," REG "0," REG "18\n"
    "fadd " REG "0," REG "0," REG "19\n"
    "fadd " REG "0," REG "0," REG "20\n"
    "fadd " REG "0," REG "0," REG "21\n"
    "fadd " REG "0," REG "0," REG "22\n"
    "fadd " REG "0," REG "0," REG "23\n"
    "subs %[count],%[count],#1\n"
    "b.ne 1b\n"
    "str " REG "0,[%[out]]\n"
    : [count] "+&r"(count)
    : [src] "r"(&source),[out] "r"(&result)
    : "cc","memory","v0","v1","v2","v3","v4","v5","v6","v7","v8","v9","v10","v11","v12","v13","v14","v15","v16","v17","v18","v19","v20","v21","v22","v23");
#else
  result=0.5; for(uint64_t i=0;i<count;i++) result+=23.25;
#endif
  uint64_t bits=0;memcpy(&bits,&result,sizeof(result));
  printf("%016llx\n",(unsigned long long)bits);
  return result==(real)23250.5 ? 0 : 1;
}
