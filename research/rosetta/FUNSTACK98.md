# Experimental codegen 98: locally proved stack blocks

This is independently authored AArch64-to-x86 compiler work, not a recovered
Rosetta allocator, Apple implementation, or an inverse opcode table.

## Mechanism and bounds

`ATTESOR_JIT_ON=...existing profile...,funstack` is opt-in (OFF wins).
Only an existing closed GP-only `funra` scope, with SP already assigned, can
qualify. Snapshot each exact basic block, ending at an existing label, the
instruction limit or a known branch/return. Admit only validated 64-bit GP
SP pairs, native narrow arithmetic/logical/move-wide templates and affine
64-bit SP updates. Reject arbitrary/WSP writes, SIMD/non-SP memory, systems,
unknown/helper templates and duplicate load destinations. Observe complete
16-byte pair spans and SP-update extrema relative to the *block's actual SP*.

Every affected block checks unsigned raw SP against its complete low56 range.
FAST folds the original two 8-byte accesses into x86 base+displacement addressing;
SLOW remains the original native TBI implementation. Both use the same fixed
assignment and shared GPR memory backend, with identical widths/access order and
writeback AFTER both accesses. No stack stores, saved values, raw tags or guest
computation are elided. Compile-time caches/notbi/flags are reset between bodies.

This deliberately supersedes the earlier proposed once-per-activation CFG proof:
there is **no** callee/ABI/balanced-return assumption. Each private recursive
entry and each epilogue/access block checks its own current SP, including after
nonlocal return or native RAS saturation. Guards precede guest effects. Source
snapshots stay inside the existing function invalidation range. Canonical
writeback, public zero markers, native-frame limits, LR matching/scanning and
post-call epoch guards are unchanged. Faults retain the existing fatal-only
contract; no resumable guest-fault claim. No commpage/helper scope qualifies.

## Focused validation, not a broad acceptance matrix

Seven release tests pass: five `funra_` tests, existing cross-thread open-call
epoch rejection, and existing helper canonicalization/clobber/reload/status.
The recursive fixture now compares the FULL guest stack buffer after resetting
it between interpreter and JIT, plus all X/V/NZCV/PC/exit state, optimization
OFF/ON, Plain/Patchable, packed flags, callret and LR-tamper modes (256 cases).
Additional tests execute 66 range-only native guards near bit56/underflow/tags
without dereferencing fake data, reject unsupported source shapes, and compare
eight full-state/full-stack cases covering linear depth300 (RAS limit256) and
source-driven epoch change inside an open recursive call, tagged/untagged SP.
These linear-recursion probes are tests, NOT benchmark substitutions.

Early development checks caught an over-broad system terminator admission;
this now explicitly rejects systems. Deep test initially did not optimize:
current `cache_access` omits pair usage, leaving SP unassigned in that tiny
source. Adding a real SP read to the probe exercises the actual mapped-SP
precondition; production allocation was NOT broadened to force the test.
Whole workspace/apps/real-AOT loading/full concurrency suite were not rerun.

## One screen, then ONLY two extra rounds after a clear positive

Unchanged original `bench` fixture, no warmup, same release binary; full previous
selected profile as control, candidate adds only `funstack`. Alternate ordering
in round2. Each execution matches all seven exact CHECK lines against native.

| round | native fib32 ms | control ms | funstack ms |
|---|---:|---:|---:|
| 1 | 5.816 | 18.350 | 13.869 |
| 2 | 6.351 | 17.480 | 13.429 |
| 3 | 5.586 | 18.261 | 13.955 |

Median18.261 ->13.869ms: **24.05% less elapsed time**, 41.94% of same-machine
native throughput. All paired improvements23.18–24.42%. Only fib's two source
blocks report static emission; logs do NOT count dynamic executions. Incidental
qsort1.5M median+1.63%, matmul+1.34%; desktop noise/layout effects not isolated.
No separate qsort4M, nbody or branchy run this iteration. No mean score, sum of
historical gains or common-config >=60% claim.

Select `funstack` for the EXPERIMENTAL profile; all unrelated rejected trials
remain OFF. Release SHA256:
`ce32b87903bc5bd2535f948f759b22f8b579b2f9a6001c527ab2bdd6d4086e3e`.
Production executable/original repository unchanged. Goal remains incomplete.
