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

## Directive update (user, 2026-10-30) — applies to every Codex run and every helper agent
- Division of labour: Codex does the implementation AND the testing/acceptance work (build, L0/L1/L2 tests, A/B
  measurements, bisecting, fixing, reports). Claude (limited quota) only schedules, spot-checks, and does what the
  sandbox cannot (GUI apps, Metal/VideoToolbox, sysctl, network), then merges and releases. Do not wait for Claude;
  decide with numbers and write the decision in your report. Keep reports short (tables, exact result lines).
- At most two helper agents at a time (machine performance); no new fan-out. Codex runs stay sequential per tree.
- Finish line: total test (all self-test classes with several seeds, token combinations, AOT/cargo tests, benchmarks),
  everything debugged, then publish as a new version to the PRIVATE GitHub repo (zheznanohana/attesor). Never change
  the repo visibility; never touch main, ~/bin, ~/attesor-launcher or installed apps from a track.
- Disk hygiene: after a test run, delete large temporary outputs (target/<run dirs> over 300 MB, private caches,
  duplicate dscslide/AOT cache copies); keep patches, commit messages, reports.

## Clarification (user): Claude still tests — a defect loop
- Claude runs independent tests (real apps, GUI, GPU/VT, network, per-module A/B, the total package). When Claude
  finds a defect it writes it to ~/attesor-dshprof/bugs/BUG-<n>.md (repro command, expected vs actual, suspected
  area). Codex fixes it (check that directory at the start of every run and fix open bugs first; mark fixed ones
  with a line `FIXED: <commit message path>`), Claude retests. Repeat until no open bugs.
- End of project: Claude measures the performance gain of EACH module (feature token, OFF vs ON, interleaved) and of
  the TOTAL package (start-of-day build ba099b9 vs final, vs native) and writes the final comparison.

## Final policy (user, latest — overrides the defect-loop clarification above)
- Per-feature gate = tiered L1 only: no major problem (builds, cargo tests, all self-test classes mismatch=0 with the token on
  and combined, directed test with sensitivity proof for translator-core features) and no performance regression.
  A regression that can be fixed: fix it. A regression that cannot be fixed: the feature is useless, REMOVE it (code and
  token) from the merged tree and list it in the report as dropped, with the numbers.
- Claude does the detailed testing (L2, real apps, GUI/GPU/network, per-module and total performance) once, after the
  final merge. No per-bug round trips are required before that.
