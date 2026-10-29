// FPCR modes in a real program: rounding modes via fesetround() and flush-to-zero via FPCR.FZ.
// The JIT must give the interpreter's (i.e. ARM's) results; loops run long enough to be translated.
#include <fenv.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
static uint64_t bits(double d) { uint64_t u; memcpy(&u, &d, 8); return u; }
static uint32_t fbits(float f) { uint32_t u; memcpy(&u, &f, 4); return u; }
static double work(void) { volatile double s = 0, x = 1.0; for (int i = 1; i < 20000; i++) { s += x / i; x *= 1.0000001; } return s; }
static float tiny(void) { volatile float a = 1e-30f, s = 0; for (int i = 0; i < 1000; i++) s += a * 1e-10f; return s; }
int main(void) {
  const int modes[] = {FE_TONEAREST, FE_UPWARD, FE_DOWNWARD, FE_TOWARDZERO};
  for (int m = 0; m < 4; m++) { fesetround(modes[m]); printf("mode %d: %016llx\n", m, (unsigned long long)bits(work())); }
  fesetround(FE_TONEAREST);
  uint64_t fpcr; __asm__ volatile("mrs %0, fpcr" : "=r"(fpcr));
  printf("denormal sum, FZ off: %08x\n", fbits(tiny()));
  __asm__ volatile("msr fpcr, %0" :: "r"(fpcr | (1ull << 24)));
  printf("denormal sum, FZ on : %08x\n", fbits(tiny()));
  __asm__ volatile("msr fpcr, %0" :: "r"(fpcr));
  return 0;
}
