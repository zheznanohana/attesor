# Task brief: syscall and GPU-bridge breakdown in the profiler

Reviewer: Claude. Implementer: Codex. Work only in `~/attesor-prof2` (branch `profiler2`). Never touch
`~/attesor`, `~/bin`, `~/attesor-launcher`, user apps, caches or processes you did not start. If git
commits are blocked, leave the work in the tree and write the message to `target/profiler2-commit-message.txt`.
Do not push. Do not launch DeepSeek Harness or VS Code.

## Why
A real-app profile (see `~/attesor-dshprof/FINDINGS-1.md`) shows the busiest cost after translated code is
BSD syscall emulation: in DeepSeek Harness's main process 48 thread-seconds in `syscall-bsd` vs 27 in
translated code, and 37 thread-seconds in the GPU bridge in the GPU process. The profiler (`ATTESOR_PROFILE`,
`crates/attesor/src/profile.rs`, states set in `syscalls.rs`) does not say which syscalls or which bridge
calls. We need that to decide what to fast-path.

## What to add
1. Per syscall: when a thread is in a BSD syscall or Mach trap, the sampled state carries the syscall number
   (and class: BSD / Mach trap / machdep). Report a table: syscall name (use the names Attesor already has,
   e.g. the trace table in `syscalls.rs`), samples, % of syscall time, and the number of calls and total
   time measured directly (count calls and accumulate host time with a cheap clock in `handle_svc`, only
   when the profiler is on). Also split each syscall's time into "host kernel" (inside the real host
   syscall) and "emulation" (everything Attesor does around it: argument translation, path mapping,
   sandbox/container mapping, struct conversion), so we can see emulation overhead.
2. GPU bridge: the same for bridge calls (which call/selector names, calls, time), if the bridge entry
   points can be identified without changing the bridge protocol.
3. Idle classification: list which syscalls were classified idle, so we can check nothing busy hides there.
4. `tools/profile-report.py` merges and prints these tables per process type.
5. Keep the overhead with the profiler off at zero in translated code and negligible in Rust paths.

## Verify
- cargo build/test, the self-test list (blocks fuse selchain fpblocks dpreg ldst branch loops: mismatch=0).
- A small arm64 test program you write that does many `stat`/`open`/`read`/`close`, `getpid`, `gettimeofday`,
  `mach_absolute_time`, `kevent` with zero timeout, plus a sleeping thread: the report must show them with
  plausible counts and the host/emulation split.
## Report
Changed functions, test lines, the test program's syscall table, commit hash or saved message path.
