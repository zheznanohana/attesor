#include <stdio.h>
#include <stdint.h>
static volatile uint64_t sink;
int main(void) {
  double px[64], py[64], vx[64] = {0}, vy[64] = {0};
  for (int i = 0; i < 64; i++) { px[i] = i * 1.7; py[i] = i * 0.3 + 1; }
  for (int s = 0; s < 20000; s++) for (int i = 0; i < 64; i++) for (int j = 0; j < 64; j++) if (i != j) {
    double dx = px[j] - px[i], dy = py[j] - py[i], d = dx * dx + dy * dy + 0.01, inv = 1.0 / (d * __builtin_sqrt(d));
    vx[i] += dx * inv * 1e-3; vy[i] += dy * inv * 1e-3; }
  sink = (uint64_t)(vx[3] * 1e9); return 0;
}
