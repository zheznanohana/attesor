# Experimental codegen105: bounded wider native inline caller CFG (provisional)

The prior inlinegp104 leaf-local experiment is flat and stays OFF. This trial
changes ONLY the inline caller's private-start cap32->64 under `inlinewide`.
Keep the original256-word budget, maximum4 profiled calls, native whitelist,
reducibility, exact raw-target/epoch guards, full dirty-all join assignment and
canonical observers. No guest algorithm, source, ABI shortcut or flag-contract
change. Existing restricted background workers remain restricted.

Bounded private inspection of the supplied unprotected ARM sysroot identified
a reducible partition loop with162 words/56 starts, rejected by the old32 cap.
Interior public call-block roots could not capture its lower backedge. No Apple
bytes/full disassembly are redistributed. This is own compiler work motivated by
local CFG evidence, not a claim of recovering Apple's allocator.

## Focused checks / ONE screen plus authorized confirmations

One new release test actually executes384 full-state comparisons across OFF/ON,
all16 NZCV inputs, tags, raw X/V, nondefault FPCR/FPSR, loop phi values/counts and
normal/full-target-miss/pre-call-stale-epoch exits. An independently authored
many-diamond native caller (not Apple bytes) crosses32 starts; >64 still rejects.
Existing five related leafflags/source-dependency tests passed during104. No
new broad workspace/application/AOT/concurrency acceptance matrix.

Release SHA256:
`13555471ea8dcd0308e06f989d03e270a9fe59bc54bc3981026a620987280f2c`.
Unchanged original kernels, native/control/+inlinewide qsort4M milliseconds:

| Round | Native | Control | Candidate | Candidate time change |
|---|---:|---:|---:|---:|
| 1 |491.9|952.0|820.3|-13.83%|
| 2 (reversed order) |479.1|909.0|844.1|-7.14%|
| 3 |483.5|923.0|826.0|-10.51%|

Median923.0->826.0ms (-10.51%), **58.54% native, NOT >=60%**. Only two extra
rounds after the clear first gain. All5 checks match each native round. Actual
emission includes the targeted56-start/162-word caller and51-start/163-word
caller. Logs describe static regions, not dynamic execution counts.

ONE unchanged original bench screen for qsort1.5M: native168.912,
control323.920,candidate310.034ms (-4.29%,54.48% native); all7 checks match.
No extra small-sort rounds. The same profile uses all default flags plus
vloop,loopra2,vchain,funra,funflags,selectra,vloop2,vscalar,cfgflags,vpair,vtrace,
bliccache,leafinline,leafflags,fpflags,funstack,recleaf,inlinewide.

## Provisional research candidate, NOT global acceptance

Retain inlinewide as the sorting working candidate, not production/default
promotion. Broader companion observations are NOT all improvements: bench
matmul37.685->42.570ms is slower in this one screen and unexplained; kernels
branchy median428.8->440.7ms and nbody48.9->50.4ms also move adversely. No wide
FP/branchy optimization was emitted; this does NOT prove the adverse movements
are harmless or uncoupled (layout/load effects remain hypotheses). No additional
score matrix was run merely to smooth them away. Current common-profile native
ratios include fib40.40%,small sort54.48%,large sort58.54%,sieve62.82% single,
matmul49.86% single,nbody40.67%,branchy53.51%. All-seven >=60% goal remains unmet.
Production binary and original repo/fixtures are unchanged. Earlier rejected
modifiers, including inlinegp104 and bytestream103, remain OFF.

Next justified investigation: stable raw-SP frame accesses inside the now-wide
caller. Prove no architectural SP writer in caller OR captured callees, bound
all exact unsigned SP effective addresses, preserve tagged/native slow paths,
shared memory backend, all raw state and guard/canonical exits. Do not assume an
ABI stack bound or re-run rejected standalone leaf/flag/address trials unchanged.
