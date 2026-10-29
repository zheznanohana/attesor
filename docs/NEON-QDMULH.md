# NEON fixed-point high multiply

This batch adds native SSE4.1 translations of the **vector 32-bit** forms of
SQDMULH and SQRDMULH, including 2S and 4S. Existing 16-bit forms now set sticky
FPSR.QC when saturation occurs, as do the new 32-bit forms. Scalar and by-element
forms are outside this batch. `ATTESOR_JIT_OFF=qdmulh` retains the interpreter
path for comparison and diagnosis.

The workload class is fixed-point DSP/image arithmetic. No current application
fallback histogram was available when selecting it; this does not claim these
instructions dominate DeepSeek Harness or improve its startup.

## Algorithm and host requirements

For 32-bit inputs, SSE4.1 PMULDQ computes signed 32x32 products for even lanes.
Shifted copies compute the odd lanes. Add `2^30` for the rounding form, shift
products logically right by 31, then interleave the low dwords. Only those low
32 bits survive, so a logical qword shift has the same retained bits as an
arithmetic shift. All intermediates fit signed 64 bits.

The sole saturation case is `INT_MIN * INT_MIN`. Its wrapped result is INT_MIN;
no valid negative result can equal INT_MIN (the most negative valid result is
INT_MIN+1). Compare against that bit pattern, flip saturated lanes to INT_MAX,
and OR FPSR.QC if any active lane saturated. The 64-bit vector forms load with
zeroed upper lanes, so inactive lanes cannot set QC. The new path uses the
existing SSE4.1 baseline and needs no additional optional host ISA feature.

## Verification

Run:

```sh
cargo build --release -p attesor
python3 tests/attesor/neon-qdmulh-check.py target/release/attesor
```

The repeatable test builds its own arm64 and x86 executables in an isolated temp
directory. A native x86 implementation uses exact signed 64-bit arithmetic as
an independent reference, without NEON or the translator. It compares complete
result bits and FPSR state across 28,800 operations covering:

- 16- and 32-bit lanes, rounded and unrounded forms, 64- and 128-bit vectors;
- `rd=rn` and `rd=rm`, random signs, INT_MIN saturation in low and high lanes;
- cleared and already-set QC, preservation of other preset FPSR bits, and upper
  vector lane clearing;
- a separate hand-computed boundary: all four INT_MIN*INT_MIN lanes must produce
  INT_MAX and set QC.

Native x86, interpreted arm64 and JIT arm64 produce:

```
qdmulh digest 4d92fd0ec98359f5 qc_cases 12708 boundary PASS
```

Each fixed-width differential class also passes 20,000 attempts, with **zero
mismatches, zero skipped, and 20,000 native translations** per width. The generic
JIT self-test does not compare FPSR; the independent end-to-end probe explicitly
does, so zero mismatches alone is not used as proof of QC correctness.

[Recorded output](measurements/2026-10-29-neon-qdmulh.txt) includes the 257-iteration
throughput-workload checksum comparison between native, JIT and forced fallback.
No performance measurement was taken during parallel agent work. For serial
before/after timing, compile `neon-qdmulh.c` and run its arm64 output with
`--bench 1000000` through each translator; compare both checksum words. Keep AOT
disabled and run correctness tests separately from timing. The workload includes
call/dispatch/XOR overhead as well as the NEON operation, so report its measured
scope, not a pure instruction latency or general application speedup.
