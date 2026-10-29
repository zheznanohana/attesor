# Task brief: FP slow paths out of the hot path (A1) and an FP constant pool (A2)

Reviewer: Claude. Implementer: Codex. Work only in `~/attesor-fp` (branch `fpcold`). Never touch
`~/attesor`, `~/bin`, `~/attesor-launcher`, the user's apps or caches. Commit after each part; do not push.
Another branch is changing GPR register caching in the same file at the same time: keep your changes to
the FP helpers, the new cold-path / constant-pool mechanism and their call sites, to keep merges small.

## Why
FP kernels run at 18-24% of native. The nbody inner loop (23 ARM insns) translates to about 5.7 KB of
x86: almost all of it is NaN-exactness code inlined after every FP op (`fix_default_nan`,
`vfix_default_nan`, `vfix_binary_nan`, `select_nan`, `fma_arm` in `crates/attesor/src/jit/emit.rs`; see
docs/JIT-FP-NANS.md). It runs almost never but fills the instruction and uop caches. Constants are
built each time with `vconst` (movabs + movq + punpcklqdq).

Measure the loop with:
`ATTESOR_DUMP_BYTES=16384 target/release/attesor --jit-dump <words>` where words are the nbody body
`fc717ae1 fc707ae2 fc717963 1e623821 fc707962 1e623862 1e620843 1f410c23 9e670184 1e642863 1e61c064
1e640863 1e631803 1e630821 fc7079a4 9e6701c5 1f451021 fc3079a1 1e630841 fc7079e2 1f450821 fc3079e1 17ffffe5`
(extracted from `bench/native-ratio/kernels.c` built with `clang -O2 -arch arm64`, file offset 0xb80, 23 words:
`ldr d1,[x23,x17,lsl #3]` ... `b`). Disassemble by wrapping the hex as `.byte`
lines, `clang -c -arch x86_64`, `objdump -d`. Record bytes and instruction count before and after.

## A1: cold paths
- Hot path keeps only the cheap test and a conditional jump (`ucomisd/ptest` + `jp/jnz cold_N`), then
  `resume_N:`. The slow-path code goes out of line.
- Place cold code after the current block's final unconditional transfer (every block ends with a jmp:
  goto/stub, indirect, exit): queue closures in the `Unit` during the block, emit them right after the
  block's terminator in `emit_block` (also for the max-length cut and unreadable-pc endings). Each cold
  path ends with `jmp resume_N`.
- Cold code must leave register-cache state exactly as at the jump-out point: it may clobber only
  xmm0-xmm7 and rax/rcx/rdx/r11 (whatever the hot path's own contract already allows at that point), never
  CREGS, xmm8-xmm15 or other cache-owned registers.
- Operand sources the slow path reloads (`fld_scalar`, `vld` of rn/rm) consult the V-register cache
  (`vsrc`, `scalar_inputs`, `vc_dest`): resolve them at hot-emission time (which xmm slot or which memory
  offset) and capture that in the closure; do not query cache state while emitting cold code.
- Cover all FP NaN fix-up sites: scalar binary ops, FSQRT, FMADD family, vector three-same, by-element,
  FMLA, `fp_maxmin` (give it a fast path: when neither input lane is NaN and inputs differ, plain
  max/min is exact; everything else cold).

## A2: constant pool
- Replace `vconst` for 128-bit broadcast constants with RIP-relative memory operands into a per-Unit
  constant pool emitted once at the end of the Unit (16-byte aligned, deduplicated). Use memory-operand
  forms where the instruction allows (pand/por/pxor/pcmpeq/andpd...), or a single `movdqa xmm, [rip+c]`.
- AOT units are cached to disk: make sure the pool is inside the same assembled code buffer and
  addressed RIP-relatively so relocated AOT code still works (check `aot.rs` build/load).

## Tests before each commit (all must report mismatch=0)
```
cargo build --release -p attesor
cargo test --release --workspace -- --test-threads=1
for c in blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch; do target/release/attesor --selftest-jit $c 100000 | tail -1; done
ATTESOR_SELFTEST_LEN=1 target/release/attesor --selftest-jit fpblocks 200000 | tail -1
for m in 1E200800:1E200800 0E20D400:0E20D400 0F800000:0F800000 1F000000:1F000000 1E21C000:1E21C000 0E20F400:0E20F400; do target/release/attesor --selftest-jit $m 100000 | tail -1; done
```
(The last loop: scalar FP 2-source, vector FP three-same, by-element, FMADD family, FSQRT, FMAX/FMIN vector.
A pre-existing sNaN-vs-qNaN difference class is documented in docs/DEBUGGING.md; report any mismatch
with its instruction so Claude can classify it, do not hide it.) Also build and run
`tests/attesor/neon_edge.c` (arm64) under the JIT and with `--interp` and compare outputs.
Do not run long benchmarks; Claude measures performance.

## Report (last message)
Changed functions, before/after code size of the nbody loop (bytes and instruction count), exact test
lines, anything uncertain, commit hashes.

## Review round 1 (Claude)
Accepted: A1 cold paths (hot nbody block 5,731 B / 1,183 insns -> 925 B / 176 insns), captured `FpSource`,
the tests. Required changes before merge:
1. **No second assembly pass.** `finish_constants` assembles the whole Unit once more just to compute the
   pool alignment. Measured: FP-heavy translation (`--selftest-jit fpblocks 100000`, 3 alternating runs)
   46.3/49.5/50.9 s -> 49.9/57.3/55.5 s (+8..15%). Translation time is startup time. Remove the pass:
   do not require 16-byte alignment. Use VEX-encoded forms with memory operands (unaligned allowed) when
   the host has AVX (`has_fma()` implies it; add an `has_avx()` check), otherwise `movdqu xmm_scratch, [rip+c]`
   then the register form. Keep deduplication; the pool may be emitted unaligned with `dq`.
2. **Enforce the cold-path register contract.** Today no cold path writes r11, CREGS or xmm8-xmm15, but
   nothing guarantees it. In `cold_path`, record the instruction index range each cold emission produced
   and, in debug builds and in a unit test that translates every FP fixup site, check with iced's
   instruction info that none of them writes rbx/rbp/r12/r13/r14/r15/rsp, r11 while `tbi_mask_live`
   was true at the jump-out point, or xmm8-xmm15. (Add the iced `instr_info` feature if needed.)
3. Then re-run the full test list from this brief and report fpblocks-translation timing the same way
   (3 alternating runs vs `~/attesor/target/release/attesor`), plus the nbody hot/whole-unit sizes.
Commit on branch `fpcold` (git works now: first commit your A1 and A2 work as two commits from the saved
patches in target/fpcold/, then the fixes).
