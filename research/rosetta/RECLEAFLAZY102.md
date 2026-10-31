# Experimental codegen102: private inline CMP recipe (not selected)

Owner-authored compiler experiment, not recovered Apple code. Opt-in modifier
`recleaflazy` requires recleaf and an existing dedicated funflags GP slot.
Production unchanged; selected experimental profile remains through recleaf101
(provisional). All earlier rejected flag/batch/address trials remain OFF.

## Captured proof, not skipped observers

For CMP-immediate only, a call-free cloned leaf must contain no additional NZCV
reader/writer. Its exact private caller continuation must overwrite incoming
NZCV before EVERY reader/helper/return/public/missing/cyclic path. Exact self
calls count as kills only with their separately captured native entry-prefix
witness. Proof words remain in the original function invalidation snapshot and
actual emitted lowering is checked native/no-helper (original self CALL allowed).
No flag slot, register CMP or live/unknown continuation retains eager101.

At the pinned cloned CMP, save the ORIGINAL W/X operand in the otherwise-unused
flag slot instead of eagerly executing CMP+four packed-flag instructions. This
slot is a compiler-private recipe, NOT a guest GPR. No guest data computation,
stack access/order/width, raw tag or source arithmetic is substituted.
Inline return STILL checks raw LR and epoch before continuation. Stale epoch
recomputes exact CMP from the saved operand then canonicalizes NZCV; raw-LR miss
first saves RAW target in RDX, repairs flags, then dispatches the unmodified raw
PC. Both cold entries independently expect operand representation. Normal private
return may leave the recipe only until its proven first kill; no canonical
writeback/helper/RET can observe it. RAS-full/nonleaf retain original eager calls.

## Focused checks

Fifteen release tests pass (14 funra + existing cross-thread open-call epoch).
Original fib expands to4096 full-state/whole-stack cases. New29,568 normal,
stale-epoch and raw-return observer comparisons cover all14 conditions, W/X,
edge operands, all16 incoming NV values, tags, nondefault RMode/FZ FPCR/FPSR,
Plain/Patchable, eager/lazy. The leaf changes X0 AFTER CMP, requiring repair from
the saved operand rather than its current value. Multiple-callsite tests require
one site lazy and its RET-before-kill sibling eager; deep300 explicitly rejects
inline/recipe at full RAS. Unsupported readers/writers, missing/cycles/uncertain
memory and register-CMP recipes reject. Prior related groups stay green.
No broad workspace/application/real-AOT acceptance matrix was run.

## ONE unchanged original bench screen, no repeat

Corrected release SHA256:
`3ddddec9e7008a6abcd1cefb8a597e0647e6c234e9156f8a61ad043428dd0fa0`.
Native fib32=5.208ms, existing recleaf control=12.864ms, +recleaflazy=13.274ms:
**3.19% slower**,39.23% native. All7 CHECK lines match, static site emitted.
NEGATIVE: keep recleaflazy OFF and do NOT repeat to manufacture a gain. Source
instruction-count reduction is not demonstrated runtime improvement.

## One separate hotspot diagnostic (NOT another performance screen)

Ran original kernels once with sampling+loopcensus+stats using the selected
profile, NOT lazy. Its five exact checks match prior same-fixture native.
Instrumentation invalidates timing comparisons. Also, inspection confirms
loopcensus explicitly DISABLES background translation; this run describes the
foreground diagnostic path, NOT normal bgtrans adoption or its timing variance.

Of1852 execution samples, libc's sorting routine accounts907 (48.97%). Four own
byte-classification loop PCs account446 (24.08%). Main qsort sampled PCs include
6003ba6d0(281),6003ba7f8(170),6003ba804(115). Own branchy PCs500000e3c/e8c/ed8/f20
have122/101/111/112 samples. Static translation has two helper sites, not a claim
of two dynamic helper invocations. Startup/translation/syscall samples are NOT
kernel-time acceptance scores. No Apple binaries/full decompilations published.

Own Mach-O disassembly confirms each branchy loop has17 source instructions:
one post-index LDRB #1, GP arithmetic/TST/CSEL, terminal SUBS counter,#1;B.NE.
Existing ldst forms RAX from mapped pointer, TBI masks, byte-loads, then updates
raw pointer. Next justified direction is a generic counted byte-stream full-EA
versioning proof, or actual qsort caller analysis, not more fib flag microtrials.
Code cache allocator is16-byte aligned; code-layout sensitivity is a hypothesis,
not a demonstrated cause of the earlier incidental matmul result. Full all-seven
common-config >=60% goal remains incomplete.
