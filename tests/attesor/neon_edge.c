// Directed NEON edge cases; run under attesor with the JIT and with --interp and compare the output.
// Each kernel runs 100 times so it is translated (JIT threshold 24) before the printed iteration.
#include <arm_neon.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
static void dump(const char *n, const void *p, int len) {
  const uint8_t *b = p; printf("%-10s", n); for (int i = 0; i < len; i++) printf("%02x", b[i]); printf("\n");
}
int main(void) {
  int16_t a16[8] = {-32768, -32768, 32767, -32768, 1, -1, 16384, -16384};
  int16_t b16[8] = {-32768, 32767, 32767, 1, -32768, -32768, 16384, 16384};
  uint8_t ua[16], ub[16]; int8_t sa[16];
  for (int i = 0; i < 16; i++) { ua[i] = (uint8_t)(i * 37 + 255 * (i & 1)); ub[i] = (uint8_t)(255 - i * 11); sa[i] = (int8_t)(i & 1 ? -128 : 127 - i); }
  int16x8_t r1, r2; uint16x8_t r3; int16x8_t r4; uint8x16_t r5; uint32x4_t r6; int64x2_t r7; uint8x16_t r8; uint16x8_t r9;
  for (int it = 0; it < 100; it++) {
    int16x8_t x = vld1q_s16(a16), y = vld1q_s16(b16);
    r1 = vqdmulhq_s16(x, y);
    r2 = vqrdmulhq_s16(x, y);
    r3 = vpaddlq_u8(vld1q_u8(ua));
    r4 = vpaddlq_s8(vld1q_s8(sa));
    r5 = vabdq_u8(vld1q_u8(ua), vld1q_u8(ub));
    r6 = vpaddlq_u16(vreinterpretq_u16_u8(vld1q_u8(ua)));
    r7 = vpaddlq_s32(vreinterpretq_s32_s16(x));
    r8 = vextq_u8(vld1q_u8(ua), vld1q_u8(ub), 5);
    r9 = vaddw_high_u8(vreinterpretq_u16_u8(vld1q_u8(ub)), vld1q_u8(ua));
  }
  dump("sqdmulh", &r1, 16); dump("sqrdmulh", &r2, 16); dump("uaddlp8", &r3, 16); dump("saddlp8", &r4, 16);
  dump("uabd8", &r5, 16); dump("uaddlp16", &r6, 16); dump("saddlp32", &r7, 16); dump("ext5", &r8, 16); dump("uaddw2", &r9, 16);
  return 0;
}
