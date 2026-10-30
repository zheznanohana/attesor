# Loopra Step 3 report

Implemented in the `loopra` worktree, starting at `400b1c1` (the task/review documentation after Step 2's `1af3c27`).

## Changes

- `crates/attesor/src/jit/emit.rs`: added `src`, `dst`, `copy`, and `init_dst`; direct region templates for immediate/shifted/extended ADD/SUB, logical operations, moves, bitfields/EXTR, variable shifts, MADD/MUL, scalar loads/stores, and CSEL variants. CMP/TST read assigned operands without destroying them. TBZ uses assigned operands; CBZ/B.cond retain direct tests and flag fusion. Source/destination aliases, W-register zero-extension, SP/ZR, and spills are preserved.
- Final fused loop flags are materialized on the exit edge when the next iteration overwrites NZCV before a reader. This is restricted to helper-free native bodies with commpage checks off; it relies on the existing fatal guest-fault contract. Helpers and other exits retain canonical state.
- Review item 1: `jit/mod.rs` and `jit/selftest.rs` add a translation-time counter request enabled/restored only by `run_selftest_loops`. Normal generated code has no locked counter increment.
- Review item 2: materialize the TBI mask in r11 before `fast`; restore it before a backedge if scratch use or a helper invalidated it. A helper-poisoning execution test covers that edge.
- Review item 3: pre/post-index and pair addresses use rdx, or the assigned base directly. Hot memory templates have no stack temporaries. The commpage helper's cold save/restore preserves rdx and call alignment, including an explicit rdx-clobbering test.
- `crates/attesor/src/aot.rs`: translator version 44. Regions remain opt-in, JIT-only, and disabled for verification.
- Added 2,034 directed operand encodings, executed with both select outcomes (4,068 comparisons against the interpreter), plus mask/counter/address regression checks.

## Instruction counts (review item 4)

| Loop | Before, off | Before, loopra | After, off | After, loopra |
| --- | ---: | ---: | ---: | ---: |
| crc32 | 43 | 36 | 43 | 14 |
| int-hash | 28 | 23 | 28 | 8 |
| branchy | 75 | 66 | 74 | 33 |

Counts use the exact words from `docs/tasks/LOOPRA.md` and `ATTESOR_DUMP_BYTES=4096 target/release/attesor --jit-dump ...`, with `ATTESOR_JIT_ON=` and `ATTESOR_JIT_ON=loopra`. Baseline dumps were captured before editing. Each hex dump was assembled as x86-64 `.byte` data and disassembled with `objdump -d`. Count from the backward jump's target through that jump, inclusive: both the conditional exit branch and backedge count; entry loads, instrumentation, and cold exit stores do not. These are this worktree's measured baseline counts, rather than the review's earlier 44–45 estimate. crc32 (14 <= 16) and int-hash (8 <= 12) meet the targets.

Raw hex, disassemblies, the measurement script, and test logs are in `target/loopra-step3/`. No timing benchmarks were run, as requested.

## Validation

`cargo build --release -p attesor` passed. Existing compiler warnings remain.

`cargo test --release --workspace -- --test-threads=1`:

```text
test result: ok. 31 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

`ATTESOR_JIT_ON=`, 100000 cases per class:

```text
jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=0
```

`ATTESOR_JIT_ON=loopra`, 100000 cases per class:

```text
jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
```

## Scope and remaining work

No known unresolved correctness issues. The existing single-block, opt-in JIT scope and CAS/LSE/SHA256H/H2 exclusions remain. Long timing benchmarks are left to Claude, as instructed. No push or other-worktree changes.

## Commit status

No commit was created: `git add` was blocked by the sandbox (`Unable to create /Users/USER/attesor/.git/worktrees/attesor-loopra/index.lock: Operation not permitted`). Changes remain in this worktree. The complete commit message is saved as `target/loopra-step3-commit-message.txt`. Nothing was pushed.
