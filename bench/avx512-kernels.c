// Track B: isolated original fp-nbody/matmul plus an explicitly NEON integer dot kernel.
// Build: clang -O2 -arch arm64 bench/avx512-kernels.c -o target/avx512/kernels
#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
static volatile uint64_t sink;
__attribute__((noinline)) static void nbody(void) {
  double px[64], py[64], vx[64] = {0}, vy[64] = {0};
  for (int i = 0; i < 64; i++) { px[i] = i * 1.7; py[i] = i * 0.3 + 1; }
  for (int s = 0; s < 2000; s++) for (int i = 0; i < 64; i++) for (int j = 0; j < 64; j++) if (i != j) {
    double dx = px[j] - px[i], dy = py[j] - py[i], d = dx * dx + dy * dy + 0.01, inv = 1.0 / (d * __builtin_sqrt(d));
    vx[i] += dx * inv * 1e-3; vy[i] += dy * inv * 1e-3;
  }
  for (int i = 0; i < 64; i++) { px[i] += vx[i]; py[i] += vy[i]; }
  sink = (uint64_t)(px[3] * 1e6);
}
__attribute__((noinline)) static void matmul(void) {
  enum { n = 300 };
  double *A = malloc(n*n*8), *B = malloc(n*n*8), *C = calloc(n*n,8);
  for (int i=0;i<n*n;i++) { A[i]=i%7; B[i]=i%5; }
  for (int i=0;i<n;i++) for (int j=0;j<n;j++) {
    double s=0; for (int k=0;k<n;k++) s += A[i*n+k]*B[k*n+j]; C[i*n+j]=s;
  }
  sink=(uint64_t)C[n+3]; free(A); free(B); free(C);
}
__attribute__((noinline)) static void neon_dot(void) {
  enum { N=4096 }; uint16_t a[N], b[N]; uint32x4_t sum[16];
  for (int i=0;i<N;i++) { a[i]=(i*37)&255; b[i]=(i*11+3)&255; }
  for (int j=0;j<16;j++) sum[j]=vdupq_n_u32(j);
  for (int r=0;r<2048;r++) for (int i=0;i<N;i+=64) {
    for (int j=0;j<16;j++) sum[j]=vmlal_u16(sum[j],vld1_u16(a+i+4*j),vld1_u16(b+i+4*j));
    // Prevent hoisting or collapsing repeated dot products without changing results.
    __asm__ volatile("" : "+m"(sum));
  }
  uint64_t h=0; for (int j=0;j<16;j++) h+=vaddvq_u32(sum[j]); sink=h;
}
int main(void) {
  double t=now(); nbody(); printf("fp-nbody %.3f ms checksum=%llu\n",now()-t,(unsigned long long)sink);
  t=now(); matmul(); printf("matmul %.3f ms checksum=%llu\n",now()-t,(unsigned long long)sink);
  t=now(); neon_dot(); printf("neon-dot %.3f ms checksum=%llu\n",now()-t,(unsigned long long)sink);
}
