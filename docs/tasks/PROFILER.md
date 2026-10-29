# Task brief: guest sampling profiler

Reviewer: Claude. Implementer: Codex. Work only in `~/attesor-prof` (branch `profiler`). Never touch
`~/attesor`, `~/bin`, `~/attesor-launcher`, the user's apps or caches, or running processes you did not
start. Commit when done; do not push.

## Why
We want to know where real apps (DeepSeek Harness / VS Code, Electron, several guest processes) spend
time under Attesor before choosing the next big optimization (native library thunks vs translation
speed vs code quality). Micro-benchmarks are not enough.

## What to build
`ATTESOR_PROFILE=<dir>`: every guest process writes `<dir>/<name>-<pid>.txt` when it exits (and on a
periodic flush every 10 s, so force-killed processes still leave data).

1. **Sampling**: a host thread per process samples every running guest thread (registry: `CPUS` in
   `crates/attesor/src/engine.rs`) at ~1 kHz. Use only lock-free reads of `Cpu` fields; never stop or
   signal guest threads. Each sample records the guest `pc` and the thread's current **state**.
2. **State** — add a cheap per-thread state word (atomic store at transitions, no locks) with at least:
   `jit` (running translated code), `interp` (interpreter: cold blocks, fallbacks, `--interp`),
   `translate` (JIT/AOT translation in progress), `syscall` (inside `handle_svc`, split Mach traps vs BSD),
   `gpu` (inside the Metal bridge calls, if they go through an identifiable entry), `idle` (blocked in a
   syscall that waits: e.g. `mach_msg` receive, `__psynch_*`, `kevent`, `select/poll`, `__semwait`).
   Transitions live in `crates/attesor/src/jit/mod.rs` (dispatcher, translate entry, interp_block),
   `syscalls.rs` (svc entry/exit). Keep the overhead below 1% when the env var is unset (a branch on a
   static bool) and low when set.
3. **Attribution**: map `pc` to image and symbol with `crate::symbols` (`describe`, the image list), and to
   the dyld shared cache image for addresses in the cache. Aggregate in the sampling thread.
4. **Output** (plain text, sorted): total samples; time share per state; top 40 images by samples (only
   `jit` + `interp` states); top 60 symbols; for the top images, the share spent in `interp` vs `jit`.
   Also: translation totals (blocks translated, ms spent) from the existing stats.
5. A small merger script `tools/profile-report.py <dir>` that sums all per-process files of one run and
   prints the same tables for the whole app (per process type too, using the process name/args).

## Constraints
- Correctness of guest execution must not change; when `ATTESOR_PROFILE` is unset the generated code
  must be byte-identical (no new emitted instructions in translated code; state changes only in Rust
  code paths around dispatch/translate/interp/syscalls).
- Add `ATTESOR_PROFILE` to the env whitelist in `crates/attesor/src/main.rs` so child processes inherit it.
- Document it in `docs/DEBUGGING.md` (short section).

## Verify
```
cargo build --release -p attesor
cargo test --release --workspace -- --test-threads=1
for c in blocks fuse selchain fpblocks dpreg ldst branch; do target/release/attesor --selftest-jit $c 50000 | tail -1; done
```
Then a real check with a small arm64 test program you build yourself (e.g. a loop calling `qsort`, `memcpy`,
`strlen`, `compress` from libz, plus some sleeping) under `ATTESOR_PROFILE=$TMPDIR/p`: the report must
show the expected images/symbols (libsystem_c / libz / main) and an `idle` share during the sleep.
Do NOT launch DeepSeek Harness or VS Code; Claude will run the real-app profile.

## Report (last message)
Changed files/functions, exact test output lines, the report of your test program, anything uncertain,
commit hash.
