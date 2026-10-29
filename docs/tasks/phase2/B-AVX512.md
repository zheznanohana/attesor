# Track B — AVX-512 for SIMD&FP (this host: i7-11800H, AVX512F/DQ/BW/VL/VBMI/...)
Branch `avx512`, worktree `~/attesor-p2b`. Read COMMON.md.
Goal: use EVEX encodings to (a) cache many more guest V registers in xmm16-xmm31 (16 extra registers; the V
cache today uses xmm8-15), (b) replace ARM NaN/default-NaN/FZ fix-up sequences with AVX-512 forms where exact
(VFIXUPIMM, VRANGE, masked blends with k registers, VPTERNLOG), (c) three-operand forms to drop copies.
Only 128-bit (xmm) EVEX operations: no 512-bit ops, so no frequency licence drop. Runtime-detected
(`has_avx512()`); hosts without AVX-512 keep today's code; the AOT key must include the choice.
1. Measure first: instruction counts and timings of fp-nbody, matmul, and a NEON-heavy kernel (write one:
   e.g. image filter / dot products with NEON intrinsics) today.
2. Implement step by step (register file extension first, then NaN fix-ups), each with self-tests
   (fpblocks, fpnan, vector classes with masks like 0E20D400:0E20D400, 0F800000:0F800000, 1F000000:1F000000),
   seeds, sensitivity proofs, neon_edge JIT == interpreter.
