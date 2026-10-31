# Experimental codegen104: local GP assignment in guarded inline leaves (OFF)

Owner-authored compiler experiment, not recovered Apple code. Existing selected
profile remains through provisional recleaf101. `inlinegp` is opt-in/unselected;
bytestream103 and all earlier rejected modifiers remain OFF. Production unchanged.

Read-only, bounded inspection of the already supplied, unprotected guest ARM
sysroot locates the sampled sorting PCs at two comparator calls. No protected
system cache was accessed. Apple bytes/full disassembly are NOT published.
The comparator itself is our original unchanged fixture's six-instruction leaf.

At an already guarded, proven inline leaf with NO enclosing fixed/loop-cache
assignment, allocate only the leaf's exact used GPRs. Reload exact live-ins,
retain all original source loads/CMP/selects/MOVs, then write back EVERY modified
architectural GPR before the post-return epoch check or continuation. Native
actual-lowering audit rejects helpers/fallbacks. The temporary assignment cannot
escape. RET-only LR is not loaded; a source body reading LR still receives the
exact newly established link. Guest state/flags, full target and epoch guards,
original memory widths/order and source invalidation dependencies are unchanged.
This differs from the failed standalone leafra: there is no new call/RET tier.

## Focused validation

One new release test executes9216 full-state comparisons: W/X, real LR reads,
all16 NZCV inputs, tags, raw X/V and nondefault FPCR/FPSR; CMP+B.cond, TBZ or CSEL
continuations; callret ON/OFF; normal, full-target mismatch and pre-call stale
local epoch. Misses must precede LR/callee effects. Five directly related
leafflags/source-dependency tests pass. Initial invocation without the required
preciseinvalidate environment skipped the new test; it is NOT counted as
validation. Corrected invocation proves9216 cases actually executed. No broad
workspace/application/real-AOT/concurrency matrix was run.

## ONE unchanged original kernels screen, no repeats

Native/control/+inlinegp qsort4M =479.1/927.0/925.8ms: **-0.13%, effectively flat**,
51.75% native. All5 original CHECKs match. Static emission includes both sampled
call sites, so this was not merely a nonactivated option. Do not repeat or
promote. Selected profile/production remain unchanged; all-seven >=60% is unmet.

Next evidence changes the direction: bounded offline CFG reconstruction finds
the natural partition header's loop is reducible with162 source instructions
but56 private starts. The inline caller region currently rejects >32 starts.
Interior sampled call-block roots cannot discover the backedge below their
address. Thus tiny leaf-local savings leave the expensive public caller edges.
Investigate a bounded wider caller region (same256 source-word budget and all
existing safety/epoch/target/canonical rules), NOT another leaf flag microtrial.
These counts are static analysis, not a performance/promotion result.
