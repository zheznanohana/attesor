#include <stdint.h>
#include <stdlib.h>
static volatile uint64_t sink;
int main(void) {
  uint8_t *src = malloc(64 << 20); for (int i = 0; i < (64 << 20); i++) src[i] = (uint8_t)(i * 2654435761u >> 24);
  uint64_t c = 0; int st = 0;
  for (int r = 0; r < 8; r++) for (int i = 0; i < (64 << 20); i++) { uint8_t v = src[i]; if (v < 64) st = (st + 1) & 7; else if (v < 128) st ^= 3; else if (v & 1) c += st; else c -= st; }
  sink = c; return 0;
}
