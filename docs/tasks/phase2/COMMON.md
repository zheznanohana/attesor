# Phase 2 — common rules for all four tracks (Claude reviews, Codex implements)
- Work only in your own worktree/branch. Never touch `~/attesor`, `~/bin`, `~/attesor-launcher`, the user's apps,
  `~/Library/Caches/Attesor` (use a private `ATTESOR_CACHE`), or processes you did not start. No GUI apps.
- If git is blocked, leave work in the tree, write commit messages to `target/<track>-commit-message.txt`
  (one per logical step) and save per-step patches. Do not push.
- Every translator change is opt-in behind `ATTESOR_JIT_ON=<name>` until Claude measures and accepts it;
  bump `TRANSLATOR_VERSION` when generated code changes.
- QA is tiered (do not run everything for every commit):
  - L0, every commit (2-5 min): build; `cargo test --release --workspace -- --test-threads=1`; only the self-test
    classes your change touches, 20,000 cases, one seed; your own directed tests. Translator-core changes
    (emit.rs, jit/) also need a sensitivity proof for the new path (break it on purpose, show mismatches).
  - L1, before Claude merges (15-25 min, Claude may run it): all classes 100,000 with and without your flag,
    `tests/attesor/loopra-aot-check.sh`, gdbstub, the headless TLS check.
  - L2, before default-on or a release (Claude runs it): several seeds at 300k+, real apps (DSH/VS Code start,
    login, menu-bar text), benchmarks.
  - Risk: translator-core changes need L0 with sensitivity and L2 before default-on; other subsystems (GPU
    bridge, process, syscalls, native libraries) use their own directed tests and real workloads instead of the
    translator self-test matrix; docs/scripts only need to build and run.
  - Lesson behind the sensitivity rule: the Q x16 miscompile (commit 11471a7) broke TLS and text layout in real
    apps while 100k random loops passed, because no test generated that operand form.
- Real-workload check without GUI: DeepSeek Harness's Electron binary as headless Node:
  `ELECTRON_RUN_AS_NODE=1 <attesor> "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness" <script.js>`
  (JS benchmark: `~/attesor-dshprof/js/js.js`; TLS check: `~/attesor-dshprof/net/t.js` must print `https status 401`
  when networking is available). Profiler: `ATTESOR_PROFILE=<dir>`, `tools/profile-report.py`.
- Measurements: alternate before/after runs (>=3), report raw numbers and `uptime`; the machine may be busy
  with the other tracks, so prefer instruction counts (`ATTESOR_DUMP_BYTES=... --jit-dump`) and A/B ratios.
- Report at the end: what changed, test lines, measurements, open risks, commit message paths.
