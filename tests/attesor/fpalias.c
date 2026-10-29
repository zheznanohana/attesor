// Compare the complete result bits with the native x86 build. Keep contraction
// disabled on both architectures; this exercises destructive scalar FP operands
// and mixed single/double widths without permitting reassociation.
// clang -arch arm64 -O2 -ffp-contract=off fpalias.c -o build/fpalias
// clang -arch x86_64 -O2 -ffp-contract=off fpalias.c -o build/fpalias-x86
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static volatile double input = 1.25;

int main(void) {
    double a = input, b = 0.375, c = 2.0;
    float x = (float)input, y = 0.125f;
    for (int i = 0; i < 10000; i++) {
        a = a * 0.999 + b;
        b = sqrt(a + c) / 16.0;
        c = (c + b) / 1.125;
        x = x * 0.9375f + y;
        y = sqrtf(x + 0.5f) / 16.0f;
    }
    uint64_t da, db, dc;
    uint32_t sx, sy;
    memcpy(&da, &a, sizeof da);
    memcpy(&db, &b, sizeof db);
    memcpy(&dc, &c, sizeof dc);
    memcpy(&sx, &x, sizeof sx);
    memcpy(&sy, &y, sizeof sy);
    printf("%016llx %016llx %016llx %08x %08x\n",
           (unsigned long long)da, (unsigned long long)db,
           (unsigned long long)dc, sx, sy);
    return 0;
}
