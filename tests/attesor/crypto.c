// CommonCrypto workload: AES-128-CBC over 64 MB and SHA-256 of the result; prints digests and timings
// (compare native x86-64 with arm64 under attesor: digests must match).
#include <CommonCrypto/CommonCrypto.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/sysctl.h>
#include <time.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
int main(void) {
  size_t n = 64 << 20, out_len = 0; unsigned char *in = malloc(n), *out = malloc(n + 16), key[16], iv[16], dg[32];
  for (size_t i = 0; i < n; i++) in[i] = (unsigned char)(i * 131 + 7);
  for (int i = 0; i < 16; i++) { key[i] = (unsigned char)(i * 17 + 3); iv[i] = (unsigned char)(255 - i); }
  int aes = 0; size_t l = sizeof aes; sysctlbyname("hw.optional.arm.FEAT_AES", &aes, &l, NULL, 0);
  double t = now(); CCCrypt(kCCEncrypt, kCCAlgorithmAES, 0, key, 16, iv, in, n, out, n + 16, &out_len); double ta = now() - t;
  t = now(); CC_SHA256(out, (CC_LONG)out_len, dg); double ts = now() - t;
  printf("FEAT_AES=%d aes-cbc %.1f ms sha256 %.1f ms digest ", aes, ta, ts);
  for (int i = 0; i < 32; i++) printf("%02x", dg[i]);
  printf("\n"); return 0;
}
