# Task brief: hot-loop register allocation ("loopra"), phase 1

Owner of the plan and reviewer: Claude. Implementer: Codex. Work only in this worktree
(`~/attesor-loopra`, branch `loopra`). Never touch `~/attesor`, `~/bin`, `~/attesor-launcher`,
the user's apps or caches. Commit after every step with a clear message; do not push.

## Why
Guest GPRs live in `Cpu` (memory, `r15`-relative). The JIT caches at most 5 of them in callee-saved
host registers (`CREGS` = rbx rbp r12 r13 r14, `RegCache` in `crates/attesor/src/jit/emit.rs`), writes
every result through to `Cpu`, and resets the cache at every block start. A single-block integer loop
therefore costs ~4x the instructions: `and w13,w11,#0xff; eor x8,x8,x13; mul x8,x8,x22; add w11,w11,#31;
cmp w11,w12; b.ne` (6 ARM insns) becomes 29 x86 insns (4 stores, 3 copies, 2-4 reloads per iteration).
Measured: integer kernels run at 50-65% of native x86.

Measured register demand of real ARM code (400k insns of Electron Framework): a 16-instruction window
uses a median of 7 distinct GPRs (91% fit in 11 host regs); 32 insns: median 10. So a loop's working
set nearly always fits if host registers are assigned per loop and results stay in registers.

Prior art in this repo: `ATTESOR_JIT_ON=regioncache` (`loop_layout`, `LoopCache`, docs/REGION-CACHE.md)
detects single-block self loops and keeps 5 slots across the backedge, still write-through. Measured
only +-5%. Reuse its detection and ranking; replace its write-through policy.

## Phase 1 scope: single-block self loops only
A block whose terminator (B / B.cond / CBZ / CBNZ / TBZ / TBNZ) targets the block's own start.

### Step 1 — verification first (no translator change)
Add a differential self-test class `loops` to `crates/attesor/src/jit/selftest.rs`
(`attesor --selftest-jit loops N`), modelled on `run_selftest_blocks` / `run_selftest_selchain`:
- Random single-block loops: a down-counter in one register, a body of 2-14 random instructions from
  integer DP immediate/register, loads/stores (base regs pointing into a private buffer, including
  pre/post-index writeback), CSEL family, flag setters, then a backedge (`subs + b.ne`, `cbnz`, `tbnz`
  variants) to the block start, iteration count 1-50, then `svc`.
- Draw registers from a small pool so values are reused, and sometimes from a pool of 12-16 registers
  so the working set exceeds the host register budget (spill paths).
- Exits to cover: the normal loop exit edge; a conditional side exit in the middle of the body
  (e.g. `cbz` to an address after the loop); an instruction that falls back to the interpreter inside
  the body (pick an encoding the JIT does not translate natively); `svc` inside the body.
- Reference: interpreter (`interp::exec`) run to the final `svc`. Compare all of `snapshot()` plus the
  buffer. Report `ok= mismatch= skipped= native= loop_regions=` where `loop_regions` counts loops the
  new code path actually took (0 for now; wire a counter that step 2 will increment).
- Sensitivity proof: temporarily break the current translator (e.g. drop one `rc_inval`, or skip one
  store in `stx`) and show `loops` reports mismatches; revert the break; record both results in the
  commit message.
- Commit. Then stop and report (see "Report format").

### Step 2 — write-back region with 10 host registers (opt-in `ATTESOR_JIT_ON=loopra`)
- Host registers for the region: existing 5 + rsi rdi r8 r9 r10. Audit every use of these five in the
  emitter (helper calls, pair-access writeback in r8/r9, others) and make the region code correct:
  save/write back around helper calls and interpreter fallbacks, reload after.
- Region entry (block start): load the assigned guest registers from `Cpu`. Inside the region, an assigned
  register's value lives only in its host register: writes do not store to `Cpu`; track dirtiness.
- Every way out of the region writes back the dirty assigned registers first: loop exit edge, side
  exits, `exit_label`, `st_pc_and_exit`, indirect branches, svc, fallbacks, helper calls, cold paths,
  the stubs emitted in `Unit::finish`. Implement this once at the exit-emission points, not per instruction.
- Every direct read of guest GPRs from memory (`xoff(` sites, about 19) must go through the region's view.
- The backedge jumps to a label after the entry loads (as `LoopCache.fast` does).
- Faults: guest fault signals are not delivered (`signals.rs`, `GUEST_FAULTS_FATAL` in emit.rs), so stale
  `Cpu` values are only visible in crash dumps; keep a comment/assert tying this feature to that fact.
- Off in verification mode (`no_internal_jumps`) and in AOT builds for now. Bump `TRANSLATOR_VERSION`.
- Review findings from step 1 (Claude): extend `loop_insn` before relying on it:
  - LDP/STP (signed offset, pre/post-index) and LDR/STR register-offset forms: the pair templates use
    r8/r9 as scratch, which this step makes allocatable.
  - Scalar/vector FP and SIMD instructions in loop bodies (reuse `fp_insn` from the fpblocks generator,
    and `init.v` values from `fp_val`): any template that uses rsi/rdi/r8/r9/r10 as scratch would silently
    clobber an allocated guest register. Audit all of them (grep the emitter), fix or exclude them from regions.
  - Guest registers x16-x19 and x24-x26 in the pools (not only x0-x15).
  - Keep the sensitivity proof for the new path: break one exit write-back on purpose, show `loops`
    with `ATTESOR_JIT_ON=loopra` reports mismatches, revert; record both results in the commit message.
- `loops` must pass with `ATTESOR_JIT_ON=loopra` (mismatch=0, loop_regions > 0), and all existing
  classes must still pass with and without the flag. Commit, stop, report.

### Step 3 — hot templates operate on assigned registers directly
- Add an operand interface: `src(n)` returns the host register holding guest `n` (or loads it into a
  scratch), `dst(n)` returns the register to write (assigned register, or scratch + store).
- Convert the hottest integer templates first: ADD/SUB (imm, shifted, extended), logical (imm, reg),
  shifts/bitfield, MOVZ/MOVN/MOVK/ORR-mov, MADD/MUL, LDR/STR (imm, reg offset, pre/post), CMP/TST,
  CSEL family, CBZ/TBZ/B.cond. Prefer `lea`/three-operand forms to avoid copies; keep flags fusion working.
- Measure: `ATTESOR_DUMP_BYTES=4096 attesor --jit-dump <loop words>` instruction counts before/after for
  the int-hash loop above (`12001d6d ca0d0108 9b167d08 11007d6b 6b0c017f 54ffff61`) and the branchy loop
  (`3840158d 93407d4e cb0e010f 8b0e010e 720001bf 9a8e01ee 5200054f 721901bf 9a8811ce 1a8f114f 1100054a
  1200094a 7100fdbf 1a8a81ea 9a8881c8 f100056b 54fffe01`). Do not run long benchmarks; Claude measures.
- Commit, stop, report.

## Always run before each commit
```
cargo build --release -p attesor
cargo test --release --workspace -- --test-threads=1
for c in blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops; do
  target/release/attesor --selftest-jit $c 100000 | tail -1
done          # and again with ATTESOR_JIT_ON=loopra once step 2 exists
```
Every class must report mismatch=0 (fpblocks too).

## Report format (last message of each run)
- What changed (files, functions), in a few lines.
- Exact test output lines (all self-test classes, cargo test summary).
- Anything you were unsure about or left out, and why.
- The commit hash.
