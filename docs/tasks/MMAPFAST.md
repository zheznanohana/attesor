# Task brief: fast path for 16 KiB-aligned mmap (stage 2b)

Reviewer: Claude. Implementer: Codex. Work only in `~/attesor-mmap` (branch `mmapfast`). Never touch
`~/attesor`, `~/bin`, user apps, caches, or processes you did not start. If git commits are blocked, leave the
work in the tree and write the message to `target/mmapfast-commit-message.txt`. Do not push.

## Evidence
`docs/tasks/node-profile.txt` (profiler syscall table, DeepSeek Harness's Electron binary run as Node doing
file and HTTP work): `mmap` 3,245 calls, 1,199 ms total, host 47 ms, "emulation" 1,151 ms (0.35 ms per call).
`mmap_16k` in `crates/attesor/src/syscalls.rs` does up to four host syscalls per guest mmap (reserve
len+16K, MAP_FIXED over the aligned window, two munmaps) to give the guest 16 KiB alignment (arm64 macOS
pages are 16 KiB, the Intel host's 4 KiB). V8/PartitionAlloc/libmalloc call mmap a lot.

## Do
1. First check how the profiler attributes host vs emulation time when one guest syscall issues several
   host syscalls (`profile::host()` / `hostsys::raw`); fix the attribution if only the last host call is
   counted, so the numbers are right. Report the corrected split for the same workload.
2. Fast path: for non-fixed requests, first map `len16` directly (with the caller's hint if any); if the
   result is 16 KiB-aligned, return it (one host syscall). Otherwise unmap it and use the existing
   reserve-and-trim path. Keep every existing semantic: MAP_FIXED untouched, errors as before, `PROT_EXEC`
   / `MAP_JIT` stripping, the AOT/symbol registration of executable file mappings.
   Consider a hint that keeps allocations aligned (e.g. remember the last aligned end and pass it as a hint)
   only if measurements show the first-try hit rate is low; report the hit rate.
3. Look for the same pattern in other memory syscalls (`mach_vm_allocate`/`mach_vm_map` traps in
   `mach_trap`, `munmap`, `mprotect`) and report their per-call emulation cost; fix only if clearly costly.
4. Tests: a unit/integration test with many mmap sizes/flags (anonymous, file-backed, with hints, fixed,
   failures) checking 16 KiB alignment, length and contents; the existing self-test list (mismatch=0);
   `cargo test --release --workspace`. Re-run the Node workload command below and report the syscall table
   (calls, total/host/emulation ms for mmap/munmap/mprotect) before/after:
   `ELECTRON_RUN_AS_NODE=1 RES="/Applications/DeepSeek Harness.app/Contents/Resources" ATTESOR_PROFILE=<dir>
    target/release/attesor "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness" <workload.js>`
   with the workload in `docs/tasks/node-workload.js` (headless, no window), then
   `python3 tools/profile-report.py <dir>`.
## Report
Changed functions, test lines, before/after syscall numbers, hit rate, commit message path.
