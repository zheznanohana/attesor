# Experimental codegen103: counted byte-stream native versioning (not selected)

Owner-authored compiler optimization, not recovered Apple code. `bytestream`
is opt-in, OFF by default and not added to the selected experimental profile.
Production binary and original benchmark/source repository are unchanged.

## Proof and lowering

A standalone GPR-only, single-basic-block countdown loop must have exactly one
post-index LDRB #1, terminal SUBS Xcount,Xcount,#1; B.NE to its own header, and
otherwise fully validated native GP instructions. Reject aliases, extra memory,
unknown/reserved/system/FP/helper instructions, interior cuts, or any other
pointer/counter writer. Both pointer and counter must have fixed GP locations.
No function/leaf/CFG contexts or spill/vector mappings are admitted.

The PUBLIC selector proves actual remaining n>0 and rawptr+(n-1)<=MASK56,
without overflow (independent u128 model in the tests). It does not assume the
source's initial trip count: a predecessor may already have run an iteration.
FAST uses the shared exact-width GP memory backend with the mapped pointer and
full64 LEA post-update, instead of per-iteration address copying/masking. SLOW
retains original native TBI. Tags and unusual spans select SLOW; even a final
pointer increment crossing bit56 retains its full raw architectural result.
There is exactly one byte access per source load, no widened/reread access.
All original ALU/TST/CSEL instructions use the EXISTING block emitter. Source
words are captured; no algorithm/benchmark identity matching or substitution.

Two private native backedges bypass the selector, with identical assignment
and a shared canonical exit. Each version resets emitter facts/caches; incoming
and final guest flags, raw X/V values and FPCR/FPSR are preserved. Actual lowering
asserts no fallback/helper call. FP checks are inapplicable only because this
scope proves no FP instructions. Existing fatal-guest-fault contract is required.
Source dependencies stay in the original block's invalidation range. Background
restricted units are not silently opted into internal jumps.

## Focused validation only

Three release tests pass. Four exact loop bodies from OUR unchanged kernels
fixture exercise 512 full-state executions: both native paths, all16 incoming
NZCV, short remaining counts, unaligned byte buffers, arbitrary raw X/V state,
nondefault FPCR/FPSR, Plain/Patchable and internal/public exits. Whole readonly
input buffers are unchanged. Actual assembled range guards match an independent
u128 model for64 cases including n=0, huge n, tags and bit56 boundaries, WITHOUT
dereferencing synthetic addresses. Scope tests reject13 invalid source mutations,
interior cuts and flag OFF. No unrelated workspace/application/AOT matrix.

## ONE unchanged original kernels screen; no repeats

Release SHA256:
`624a0abe26e3b7580a82da7518a31c4741352536ef9efc34778293e5f8bc5c2d`.
Native/control/candidate branchy = **235.9 / 446.3 / 432.3 ms**.
Candidate time is **3.14% lower** than control; **54.57% of native performance**.
All five CHECK lines match native. Four static sites emitted (not a dynamic
path counter). Same full selected profile, candidate adds ONLY bytestream.

This is a small single observation, NOT a confirmed gain or >=60% acceptance.
Per user's fast-progress policy: no extra rounds, retain OFF, move to sorting
hotspots. Qsort happened to move930.5->963.5ms; no optimization to sorting was
emitted and no causal improvement/regression is inferred from that one sample.
Selected profile remains through provisional recleaf101; no historic gains are
summed. Full seven-item/common-profile >=60% goal remains incomplete.
