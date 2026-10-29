// CPU kernels for comparing native x86-64 with arm64 under attesor; prints milliseconds per kernel.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
static volatile uint64_t sink;
static int cmp(const void *a, const void *b) { uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b; return (x > y) - (x < y); }
int main(void) {
  enum { N = 1 << 22 };
  uint32_t *a = malloc(N * 4); uint8_t *src = malloc(64 << 20), *dst = malloc(64 << 20); double t;
  // integer: FNV-1a over 256 MB
  t = now(); uint64_t h = 1469598103934665603ull; for (int r = 0; r < 4; r++) for (int i = 0; i < (64 << 20); i++) { h ^= (uint8_t)(i * 31 + r); h *= 1099511628211ull; } sink = h;
  printf("int-hash   %8.1f ms\n", now() - t);
  // qsort 4M uint32 (calls through a comparator: indirect branches)
  uint32_t x = 1; for (int i = 0; i < N; i++) { x ^= x << 13; x ^= x >> 17; x ^= x << 5; a[i] = x; }
  t = now(); qsort(a, N, 4, cmp); printf("qsort      %8.1f ms\n", now() - t); sink = a[N / 2];
  // floating point: n-body style
  t = now(); double px[64], py[64], vx[64] = {0}, vy[64] = {0};
  for (int i = 0; i < 64; i++) { px[i] = i * 1.7; py[i] = i * 0.3 + 1; }
  for (int s = 0; s < 2000; s++) for (int i = 0; i < 64; i++) for (int j = 0; j < 64; j++) if (i != j) {
    double dx = px[j] - px[i], dy = py[j] - py[i], d = dx * dx + dy * dy + 0.01, inv = 1.0 / (d * __builtin_sqrt(d));
    vx[i] += dx * inv * 1e-3; vy[i] += dy * inv * 1e-3; }
  for (int i = 0; i < 64; i++) { px[i] += vx[i]; py[i] += vy[i]; } sink = (uint64_t)(px[3] * 1e6);
  printf("fp-nbody   %8.1f ms\n", now() - t);
  // memory: memcpy 64 MB x 16
  memset(src, 7, 64 << 20); t = now(); for (int r = 0; r < 16; r++) memcpy(dst, src, 64 << 20); sink = dst[12345];
  printf("memcpy     %8.1f ms\n", now() - t);
  // branchy: byte classification state machine over 64 MB
  for (int i = 0; i < (64 << 20); i++) src[i] = (uint8_t)(i * 2654435761u >> 24);
  t = now(); uint64_t c = 0; int st = 0;
  for (int r = 0; r < 4; r++) for (int i = 0; i < (64 << 20); i++) { uint8_t v = src[i]; if (v < 64) st = (st + 1) & 7; else if (v < 128) st ^= 3; else if (v & 1) c += st; else c -= st; }
  sink = c; printf("branchy    %8.1f ms\n", now() - t);
  return 0;
}
