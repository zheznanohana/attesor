# Write-through natural-loop register cache experiment

Current CPU acceptance (translator version 66): both `regioncache` and `loopra` are opt-in,
with `ATTESOR_JIT_OFF` taking precedence. Write-through slots now use the normal next-use
proof instead of copying dead values merely because a register belongs to the loop layout;
the existing private-edge repair reloads invalidated slots. JIT and disk AOT are tested.
The numbered implementation notes below are historical; current measurements and decisions
are in [ACCEPTANCE-REPORT.md](ACCEPTANCE-REPORT.md).

Enable with `ATTESOR_JIT_ON=regioncache`. It is off by default. This first
implementation supports only a single basic block whose direct B/B.cond/CBZ/TBZ
backedge returns to its own start. It does not implement multi-block regions,
vector carryover, dirty register writeback, or application-specific translation.

The loop ranks decoded GPR reads and assigns at most five guest registers to
fixed callee-saved host slots. Other GPR reads continue from Cpu. The same layout
applies after helper invalidation and cache refill, so a loop edge never needs a
parallel register shuffle. X31 is cached only as SP; XZR reads and discarded
writes continue through the existing instruction-specific paths.

Every published Unit label, dispatcher entry, AOT block offset and patchable
external target points to an ordinary public entry which loads the chosen
registers from Cpu. Only the currently emitted loop's own direct edge can reach
the private label after those loads. The private label is before the FPCR guard:
looping cannot bypass a newly unsupported floating-point mode.

At a private edge, each selected host slot must either have the correct guest
mapping in the current compile-time cache state or be loaded again from Cpu.
These edge repairs do not change compile-time metadata: the alternate arm of a
conditional branch retains its original contract. Repairs use only flag-neutral
loads into cache slots. Helpers and conditional instruction joins retain their
existing conservative invalidations. Calls still invalidate vector/scratch state.
Vector cache metadata starts empty when emitting the loop body; no previous
iteration's vector cache state is assumed valid.

Every architectural register write remains immediately visible in Cpu, including
writes before a faulting load. Exits and edges to other blocks retain the existing
canonical entry protocol. Verification mode (`no_internal_jumps`) disables the
private loop path. Existing AOT identity already includes the complete
`ATTESOR_JIT_ON` value, so opt-in and ordinary generated code cannot share a key.
The cache format/protocol and GPU bridges are unchanged.

## Validation

```
cargo test -p attesor
cargo build -p attesor
python3 tests/attesor/regioncache-check.py target/debug/attesor
```

The directed unit gates exercise randomized initial guest values through every
public entry, Plain and Patchable code, SP/XZR aliases, a helper that observes
preceding writes and modifies arbitrary GPRs, conditional fallthrough, external
diamond joins, scalar FP source/destination aliases across iterations, FPCR
changes at a helper followed by a backedge, and a real protected-page fault on
the second iteration. The fault test uses a forked child and shared Cpu state so
the parent observes the committed state after the signal without introducing a
production signal-recovery mechanism.

The standalone checker runs randomized differential instruction/block gates,
then compares an explicit ARM natural-loop recurrence and compiler-generated
register-pressure recurrence against independently compiled native x86 C. It
checks baseline JIT, region JIT, commpage redirection, old next-use policy, and
cold/warm AOT execution in a private temporary cache. The checker disables
background builds, proves the cold cache has no main-image artifact, explicitly
prewarms the main image, and requires nonzero main-image loaded diagnostics in
two new processes. It also verifies that baseline code rejects the opt-in cache.
It deliberately does not time performance. Public-entry
preloads and selecting only five fixed registers can cost more than they save;
serial representative measurements must decide whether the experiment should
be enabled or expanded.

## Ten-register write-back variant (Step 2)

`ATTESOR_JIT_ON=loopra` uses the same single-block discovery and read ranking,
with rbx/rbp/r12/r13/r14/rsi/rdi/r8/r9/r10 as fixed slots. Public entries load
from Cpu; self-backedges skip those loads and retain the FPCR guard. Assigned
GPR writes update only the slot. Other GPRs continue through Cpu. This is the
write-back policy only; templates still copy operands through their scratch
registers. Verification mode and Plain/AOT units cannot enable it.

The emitter records the union of native destination writes for each region.
Unknown interpreter writes dirty all slots. Region-specific dispatcher tails
are emitted by `Unit::finish` after that union is complete, including early
FPCR and helper-status exits. Direct non-self edges synchronize before either
an internal public entry or a patchable slot, so a chained stub also receives
canonical state. In-body helpers synchronize all assigned slots: on a later
iteration, an early helper can observe a native write following it in the body.
Interpreter calls reload every slot afterward; commpage calls also restore the
caller-saved slots. All synchronization uses flag-neutral MOVs.

Scratch-register audit: pair and scalar pre/post-index accesses use aligned
stack temporaries inside regions instead of r8/r9, allowing even their operands
to occupy those slots. CAS/LSE (rsi) and SHA256H/H2 (rsi/rdi/r8 in the software
path) exclude a loop from regions, regardless of host instruction support. The
remaining scalar FP and SIMD templates use only the reserved GPR scratches
rax/rcx/rdx/r11 and XMM registers. r10 has no other emitter use. Direct guest GPR
loads/stores go through the region-aware load/store helpers; entry loads and
synchronization are the deliberate exceptions.

This relies on `GUEST_FAULTS_FATAL`: signals.rs does not deliver resumable guest
fault handlers. A fatal crash dump can show stale Cpu GPRs. Region entry asserts
this invariant. The runtime entry counter embeds a JIT-only host address, so it
is also excluded from AOT. Translator version is 43.

The extended `loops` differential generator includes pair offset/pre/post forms,
register-offset memory, scalar/vector FP, SIMD/SHA, and x16-x19/x24-x26. Directed
unit tests exercise all ten slots across interpreter and commpage calls, helper
status exits, a cold FPCR exit after a backedge, and unpatched/patched exit stubs.
