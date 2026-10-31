# Experimental codegen100: exact MOV flag-fusion carrier (not selected)

Independently authored emitter experiment; not Apple-derived code. `movflags`
is opt-in, OFF wins. The previous funedge99 stays OFF. Selected experimental
profile remains full defaults plus the previously selected switches through
funstack98; production executable/original repository remain untouched.

## New evidence and implementation

Actual fib tail is CMP W20,#3; MOV X20,X0; B.HI. Source99 adjacent-only packing
hit just header CMP, twice for the native stack versions. The MOV is native
flag-preserving, but main emitter consumes then drops its incoming fusion kind.
Without fusion the packed B.HI needs TEST packedReg, SETZ AL, TEST AL, JCC.

This trial carries the EXISTING typed fusion kind through an exact W/X GP MOV
alias inside a fixed region, only when actual emitted lowering is native,
helper-free, unfused-with-other-source-words and every instruction has
`rflags_modified()==0`. Public/private entries keep their old resets; helpers,
unknown lowering, non-MOV words and modified/undefined host flags cannot extend
facts. This does NOT elide/defer guest NZCV materialization or recompute a CMP
from operands after MOV (which could overwrite them). All guest instructions,
raw register writes, stack bytes, markers, LR/RAS/epoch guards stay unchanged.

Nine focused release tests pass in0.20s runtime (compile18s, build14s; no broad
suite). New175,616 W/X all-condition full X/V/NZCV/PC/exit/whole-stack comparisons
exercise inserted W/X MOV, both edge directions, tags, Plain/Patchable and
funstack OFF/ON. Existing original fib now1024 whole-state/whole-stack cases
covers movflags OFF/ON along with funedge, packed flags, callret, LR tampering
and stack variants. Prior proof/epoch/deep tests remain green, not relabelled as
full-application/AOT/concurrency acceptance. One early TEST build failed only on
ambiguous test-vector integer type; fixed before any performance execution.

## One unchanged original bench screen, no repeat

| fib32 | ms |
|---|---:|
| native |5.103|
| control through funstack |13.319|
| +movflags |14.113|

Candidate **5.96% slower**,36.16% native. Static emission logs confirm fib's
1000011e0 MOV and four system-library sites; logs are NOT dynamic counts. All7
CHECK lines match. No causal CPU/layout claim follows from a single sample.
As requested, NO extra performance run follows this negative screen. Keep
`movflags` OFF. Release SHA256:
`bf07a78b7575c10fd7028527fa13c0671575c07081f67563dc0032eec5561085`.
No accumulated historical gain/full common-profile60% claim; goal incomplete.
