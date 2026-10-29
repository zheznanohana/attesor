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

## Round 2 (Claude): your AVX-512 detection is wrong — fix it and continue
macOS enables AVX-512 state on demand: XCR0 reads 0x7 until a thread executes its first AVX-512 instruction,
which traps once and the kernel promotes the thread (XCR0 then reads 0xe7). Verified on this host:
`XCR0 before: 0x7`, an EVEX op (xmm17, masked move) works and returns the right result, `XCR0 after: 0xe7`.
`sysctl hw.optional.avx512f/avx512bw/avx512vl/avx512dq` all report 1. So:
1. Detect with CPUID AVX-512 bits AND `sysctl hw.optional.avx512f` (and vl/bw/dq) instead of XCR0 on macOS.
2. Commit your step-1 work first (24-slot cache, EVEX encodings, AOT identity) as its own commit message file.
3. Then run the execution tests and sensitivity proof that were skipped, measure (fp-nbody, matmul, your
   avx512-kernels.c, raw numbers + uptime), and continue with the NaN fix-up stage (VFIXUPIMM, VRANGE, masks).
AVX-512 instructions need no sandbox permission; everything can run inside your sandbox.
