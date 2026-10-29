# Phase 2 — common rules for all four tracks (Claude reviews, Codex implements)
- Work only in your own worktree/branch. Never touch `~/attesor`, `~/bin`, `~/attesor-launcher`, the user's apps,
  `~/Library/Caches/Attesor` (use a private `ATTESOR_CACHE`), or processes you did not start. No GUI apps.
- If git is blocked, leave work in the tree, write commit messages to `target/<track>-commit-message.txt`
  (one per logical step) and save per-step patches. Do not push.
- Every translator change is opt-in behind `ATTESOR_JIT_ON=<name>` until Claude measures and accepts it;
  bump `TRANSLATOR_VERSION` when generated code changes.
- Correctness bar (lesson from the Q x16 miscompile, commit 11471a7: a wrong LEA scale broke TLS and text
  layout in real apps while 100k random loops passed): every new code path gets a differential self-test that
  exercises its operand forms, a sensitivity proof (break it on purpose, show mismatches), and runs with
  several seeds (`--selftest-jit loops N SEED`). Full list before each commit, with and without your flag:
  `cargo test --release --workspace -- --test-threads=1`; self-test classes blocks fuse fuse2 selchain fpblocks
  dpimm dpreg ldst branch loops calls fpnan tbiblocks: mismatch=0; `tests/attesor/loopra-aot-check.sh`.
- Real-workload check without GUI: DeepSeek Harness's Electron binary as headless Node:
  `ELECTRON_RUN_AS_NODE=1 <attesor> "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness" <script.js>`
  (JS benchmark: `~/attesor-dshprof/js/js.js`; TLS check: `~/attesor-dshprof/net/t.js` must print `https status 401`
  when networking is available). Profiler: `ATTESOR_PROFILE=<dir>`, `tools/profile-report.py`.
- Measurements: alternate before/after runs (>=3), report raw numbers and `uptime`; the machine may be busy
  with the other tracks, so prefer instruction counts (`ATTESOR_DUMP_BYTES=... --jit-dump`) and A/B ratios.
- Report at the end: what changed, test lines, measurements, open risks, commit message paths.
