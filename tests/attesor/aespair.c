// AESE+AESMC / AESD+AESIMC round pairs (as compilers emit them): JIT result must equal the interpreter's.
#include <arm_neon.h>
#include <stdio.h>
int main(void) {
  uint8_t init[16]; for (int i = 0; i < 16; i++) init[i] = (uint8_t)(i * 37 + 11);
  uint8x16_t s = vld1q_u8(init), k = vrev64q_u8(s), d = veorq_u8(s, vdupq_n_u8(0xa5));
  for (int i = 0; i < 1000; i++) {
    s = vaesmcq_u8(vaeseq_u8(s, k)); k = vaddq_u8(k, vdupq_n_u8(1));
    d = vaesimcq_u8(vaesdq_u8(d, k));
  }
  uint8_t a[16], b[16]; vst1q_u8(a, s); vst1q_u8(b, d);
  for (int i = 0; i < 16; i++) printf("%02x", a[i]); printf(" ");
  for (int i = 0; i < 16; i++) printf("%02x", b[i]); printf("\n"); return 0;
}
