# Experimental codegen101: guarded recursive call-free paths

Independently authored AArch64-to-x86 optimization, not recovered Apple code or
an algorithm recognizer. `recleaf` is opt-in; OFF wins. Production unchanged.

## Proof and execution

In an existing closed GP recursive scope (same fixed mapping), capture a bounded
native prefix ending CMP+B.cond, and a finite call-free GP successor CFG. Prefix
GPR writes must not alter either predicate input. All original prefix/leaf words
remain in the owning source invalidation snapshot. Only validated native GP
arithmetic, SP pairs, ordinary unsigned GP memory and bounded branches qualify;
unknown/system/FP/helper, missing nodes/cycles, unsupported return shapes and
excessive cloning reject the optimization. No name/benchmark identity test.

After original BL LR update, a private GPR-only predicate probe chooses the inline
path. This probe rereads NO guest data memory and changes no guest NV/register.
A separate native RAS depth guard is retained; full depth and nonleaf use original
host_call. Inline executes the ORIGINAL prefix, computation, loads/stores and
leaf CFG, including full guest frame bytes, exact access widths/order and raw SP.
The existing per-block funstack bounds/native TBI slow paths are reused.
The prefix conditional is specialized only after its operand-invariance proof.

Inline RET checks RAW actual LR against this call site's exact continuation; it
never consumes an outer host frame with an identical guest return PC. Match still
checks epoch BEFORE any caller guest continuation (even callret option OFF).
Mismatch preserves RAW target PC and canonically dispatches; no tag stripping.
Stale epoch also canonicalizes full callee state. The original public markers,
nonleaf return scan, RAS protection and invalidation lifetime protocol remain.
Compile-time caches/labels/analysis are separately restored across inline/miss.
All cloned actual lowering is checked native/no-CALL. No ABI save elision.

## Focused checks and a real release-only bug

Twelve directed release tests pass:11 funra tests plus existing cross-thread
open-call epoch rejection. Original fib now2048 full X/V/NZCV/PC/exit/whole-stack
cases spans recleaf OFF/ON, stack variants, packed flags, other experimental flag
modes, callret, signed/extreme n and LR tampering. Test-only counters prove actual
inline execution.32 raw-return cases require preserving tagged target PCs,
including a tag whose LOW bits equal a parent continuation.32 deep300/epoch
cases check full stack/state plus nondefault FPCR/FPSR; full native RAS explicitly
rejects inlining, and epoch bailout requires NEW callee NV=6. Source proof rejects
modified predicate inputs, reserved/unknown/template/system/cycle/missing paths.
Existing two175,616-condition groups and range checks remain green. No broad
workspace/application/real-AOT acceptance matrix was run.

An initial real release screen aborted before any candidate checksum/timing:
removing a test-only diagnostic left two CodeAssembler labels on one instruction.
This was NOT performance evidence. Added an unconditional cold jump separating
the labels independent of test configuration, reran the directed group, rebuilt,
and screened the corrected binary. The failed raw artifact/hash is preserved
separately. No failed score or earlier control sample enters the final cohort.

## One successful screen + only two extra rounds after clear gain

Unchanged original bench, full prior selected profile through funstack as control,
candidate adds only recleaf; no warmups, reverse order in round2, all7 CHECK lines
match each execution. Corrected release SHA256:
`38b9bd3b7426e83b45abbb6f25c8f7d5fa92eea03e9e7394fca6b43c9e888775`.

| round | native fib32 ms | control ms | +recleaf ms |
|---|---:|---:|---:|
|1|5.241|15.046|12.638|
|2|5.360|14.354|13.333|
|3|5.250|14.409|14.316|

Median14.409->13.333ms, **7.47% less time**,39.38% native. Paired gains16.00%,
7.11%,0.65%: direction positive, size variable and final round nearly flat.
Retain as a **provisional experimental candidate**, NOT a stable16% gain or a
production promotion. No further score repetition. Funedge/movflags and earlier
rejected trials stay OFF; no cross-batch gain sum.

Incidental matmul candidate/native observations were60.76%,60.20%,60.10% in this
cohort (medians21.474/35.564ms). Only fib's call site reports recleaf emission;
there is NO demonstrated causal matmul optimization. The margin over60 is tiny,
not an all-workload/common-profile acceptance claim. Qsort4M/nbody/branchy were
not rerun here; fib is still far below60. Full goal remains incomplete.
