# Cross-thread instruction publication and ISB

The JIT previously emitted ISB as a no-op. Its runtime observes translation
invalidation epochs only when it returns to the dispatcher. A reader that stayed
in its indirect-branch cache could therefore reuse a stale translated target
even after a correctly synchronized code update.

## Fix

ISB now terminates a translated block and returns directly to the dispatcher at
the next guest PC, with exit data zero. It does not use a patchable chaining
slot, and it cannot jump to an already-emitted successor inside the same unit.
The dispatcher then checks the epoch and clears stale per-thread IBTC entries
before resolving the following instruction. Cold interpretation also ends its
block at ISB. Static AOT discovery follows the post-ISB continuation, so this
boundary does not silently truncate prewarm coverage.

The ordinary indirect-branch emitter is unchanged. There is no added epoch load
or conditional branch on every ordinary BR/BLR/RET. No Cpu layout change or
new per-thread runtime field was required. SB remains distinct from ISB.
An ISB itself now incurs a dispatcher boundary; that is the synchronization
cost of this conservative implementation, not a claimed speed improvement.

## Valid synchronization, not an invalid SMC race

The executable probe follows cooperative publication:

1. A reader warms the original function and its target/return paths.
2. The writer requests a park; the reader release-acknowledges that it is no
   longer executing the target and spins elsewhere.
3. The writer changes MOV W0,#1 to MOV W0,#2 on a separate 16 KiB page, executes
   DC CVAU / DSB ISH / IC IVAU / DSB ISH, then release-publishes completion.
4. The reader acquire-loads completion and executes ISB before calling the target.

The target cannot be executing while the writer modifies it. Separating its
page from the reader also prevents conservative invalidation ranges from
accidentally forcing the reader out of its old lookup loop. This follows Arm's
[full-synchronization guidance for code publication across threads](https://developer.arm.com/community/arm-community-blogs/b/architectures-and-processors-blog/posts/caches-self-modifying-code-working-with-threads).
Unsynchronized concurrent instruction replacement is not the guarantee of this
change; the architecture has separate, restricted concurrent-modification rules.

The saved pre-fix binary deterministically returned **1 instead of 2** in the
indirect case after more than 10,000 warm calls. Its direct-chain counterpart
already passed because invalidation unlinks incoming direct slots. The fixed
binary passes both cases. Direct chaining is therefore a control, not a claimed
newly fixed failure.

## Reproduction and coverage

```sh
cargo build --release
cargo test -p attesor context_sync_tests -- --test-threads=1
cargo test -p attesor static_discovery_keeps_isb_successor -- --test-threads=1
python3 tests/attesor/icache-handoff-check.py target/release/attesor
# Optional, when retaining the pre-fix build:
python3 tests/attesor/icache-handoff-check.py target/release/attesor --baseline /tmp/attesor-ibtc-before
```

The process probe repeats direct and indirect publication four times each under
default tiering, immediate JIT, optional host call/return, redirected commpage,
and the interpreter. An executing unit additionally covers Plain (AOT-style) and
Patchable (JIT-style) units, all encoded ISB option fields, preservation of guest
NZCV, and an already-present internal successor that must not execute before the
dispatch boundary. AOT discovery's post-ISB entry has its own unit check.
[Recorded process checks](measurements/2026-10-29-icache-handoff.txt).

For serial before/after cost checks, compile `icache-handoff.c` for ARM and run
it with `--bench 5000000`. This separate workload has no ISB in its hot loop and
prints `ibtc-ordinary` milliseconds and a checksum. Its 4,097-iteration smoke
result agrees with an independent native-x86 reference. It does not measure the
cost of a synchronization barrier or prove a general application speedup. No
performance timing was collected during parallel agent development.

The code-generator version must be advanced when integrating this change so
older AOT translations that omit the ISB boundary are not reused.
