# Task brief: loop register allocation in AOT translations, and short loops

Reviewer: Claude. Implementer: Codex. Work only in `~/attesor-aotra` (branch `loopra-aot`). Never touch
`~/attesor`, `~/bin`, `~/attesor-launcher`, the user's apps, `~/Library/Caches/Attesor` or processes you
did not start. If git commits are blocked by the sandbox, leave the work in the tree and write the commit
message(s) to `target/loopra-aot-commit-message.txt`. Do not push.

## Background
Hot-loop register allocation ("loopra", docs/tasks/LOOPRA.md, `LoopRa` in `crates/attesor/src/jit/emit.rs`)
is now on by default, but only for JIT units: `loopra: loopra_enabled() && stub_kind == StubKind::Patchable`
in `Unit::new`. AOT units (`aot.rs` `build()`, `StubKind::Plain`, cached on disk and loaded by
`load_file`) never get regions. Real apps load most hot code from the AOT cache from the second launch on,
so today they barely benefit. Measured gains on JIT code: int-hash +34%, branchy +54%, memops +45%,
crc32 +19%; but `sieve` (bench/bench.c) -6%.

## Part 1 — regions in AOT units
- Allow regions in `StubKind::Plain` units. Every way out of a region must still write back first
  (Plain stubs exit to the dispatcher; check `Unit::finish`, `goto`, the stubs, `exit_label`,
  `dispatcher_exit`, cold paths, the constant pool, FPCR guard).
- AOT code is position independent and stored to disk: no absolute host addresses in region code (the
  self-test counter must stay out of AOT units, or use a RIP-relative/`r15`-relative counter only in
  self-test mode).
- `TRANSLATOR_VERSION` bump; AOT identity must change when loopra on/off changes (check how
  `ATTESOR_JIT_OFF` / `ATTESOR_JIT_ON` already enter the AOT key; `ATTESOR_JIT_OFF=loopra` must produce a
  different cache key than the default).
- Tests: (a) a unit test that builds a Plain unit containing a region, assembles it at two different
  bases, and executes both copies against the interpreter; (b) an end-to-end check script
  `tests/attesor/loopra-aot-check.sh`: build arm64 programs with integer loops (int-hash, crc32 table,
  sieve, branchy-style csel loops, pointer chasing, pre/post-index and LDP/STP loops), run each
  (1) natively as x86 builds for the expected output, (2) under `--interp`, (3) under the JIT with a
  fresh private `ATTESOR_CACHE`, letting the AOT build happen (`ATTESOR_AOT_SYNC=1` or a graceful exit),
  (4) again with that cache loaded (prove with `ATTESOR_STATS=1` that AOT blocks were loaded and that
  regions ran from AOT code). All outputs must match.
- The full self-test list (below) with loopra on and off.

## Part 2 — short loops (`sieve` -6%)
- `sieve`'s inner loop (`for (j = i*i; j < n; j += i) c[j] = 1;`) is entered once per prime with few
  iterations for large primes, so the region's fixed cost (entry loads of all assigned registers, exit
  write-back, exit flag store) dominates. Measure it: extract the loop words, count entry/exit/body
  instructions with `ATTESOR_DUMP_BYTES=4096 attesor --jit-dump`, and time `sieve` with loopra on/off
  (3 alternating runs, `--no-aot`, report raw numbers and machine load from `uptime`).
- Reduce the fixed cost without hurting long loops, e.g.: assign only registers the body actually uses
  (already ranked) and do not load registers that are written before being read in the body; write back
  only registers the body can have written; keep anything else that the measurements point to. If a
  heuristic is needed, base it on the loop body, not on guesses about trip counts.
- Report before/after for sieve, int-hash, crc32, branchy, memops (same method).

## Always run before each commit
```
cargo build --release -p attesor
cargo test --release --workspace -- --test-threads=1
for E in "" loopra; do for c in blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops fpnan tbiblocks; do
  ATTESOR_JIT_OFF=$E target/release/attesor --selftest-jit $c 100000 | tail -1; done; done
tests/attesor/gdbstub.sh target/release/attesor
```
All mismatch=0.

## Report (last message)
Changed functions, exact test lines, end-to-end script output, the sieve analysis and the before/after
table, anything uncertain, commit hashes (or the saved message path).
