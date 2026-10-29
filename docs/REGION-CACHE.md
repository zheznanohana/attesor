# Write-through natural-loop register cache experiment

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
