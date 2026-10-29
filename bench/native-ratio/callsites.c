// Call/return to many sites: one small function called from 64 places in turn (like objc_msgSend,
// malloc or memcpy in real programs). Prints milliseconds.
#include <stdint.h>
#include <stdio.h>
#include <time.h>
__attribute__((noinline)) static uint64_t mix(uint64_t s, uint64_t v) { s ^= v; s *= 0x9E3779B97F4A7C15ull; return s ^ (s >> 29); }
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
int main(void) {
  volatile uint64_t out; uint64_t s = 1; double t = now();
  for (uint64_t i = 0; i < 300000; i++) {
    s = mix(s, i + 0);
    s = mix(s, i + 1);
    s = mix(s, i + 2);
    s = mix(s, i + 3);
    s = mix(s, i + 4);
    s = mix(s, i + 5);
    s = mix(s, i + 6);
    s = mix(s, i + 7);
    s = mix(s, i + 8);
    s = mix(s, i + 9);
    s = mix(s, i + 10);
    s = mix(s, i + 11);
    s = mix(s, i + 12);
    s = mix(s, i + 13);
    s = mix(s, i + 14);
    s = mix(s, i + 15);
    s = mix(s, i + 16);
    s = mix(s, i + 17);
    s = mix(s, i + 18);
    s = mix(s, i + 19);
    s = mix(s, i + 20);
    s = mix(s, i + 21);
    s = mix(s, i + 22);
    s = mix(s, i + 23);
    s = mix(s, i + 24);
    s = mix(s, i + 25);
    s = mix(s, i + 26);
    s = mix(s, i + 27);
    s = mix(s, i + 28);
    s = mix(s, i + 29);
    s = mix(s, i + 30);
    s = mix(s, i + 31);
    s = mix(s, i + 32);
    s = mix(s, i + 33);
    s = mix(s, i + 34);
    s = mix(s, i + 35);
    s = mix(s, i + 36);
    s = mix(s, i + 37);
    s = mix(s, i + 38);
    s = mix(s, i + 39);
    s = mix(s, i + 40);
    s = mix(s, i + 41);
    s = mix(s, i + 42);
    s = mix(s, i + 43);
    s = mix(s, i + 44);
    s = mix(s, i + 45);
    s = mix(s, i + 46);
    s = mix(s, i + 47);
    s = mix(s, i + 48);
    s = mix(s, i + 49);
    s = mix(s, i + 50);
    s = mix(s, i + 51);
    s = mix(s, i + 52);
    s = mix(s, i + 53);
    s = mix(s, i + 54);
    s = mix(s, i + 55);
    s = mix(s, i + 56);
    s = mix(s, i + 57);
    s = mix(s, i + 58);
    s = mix(s, i + 59);
    s = mix(s, i + 60);
    s = mix(s, i + 61);
    s = mix(s, i + 62);
    s = mix(s, i + 63);
  }
  out = s; printf("callsites  %8.1f ms\n", now() - t); return 0;
}
