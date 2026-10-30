# Track D — offline whole-image translation with an optimizer (Rosetta-style)
Branch `offline`, worktree `~/attesor-p2d`. Read COMMON.md and docs/AOT-PREWARM.md, docs/AOT-CHECKPOINTS.md.
Goal: translate an installed app's images ahead of time in the background (at install/first launch), so the first
launch is already warm, and spend more compile time per function than the JIT can (cross-block register
allocation within a function, dead flag/store elimination across blocks, constant propagation of addresses).
Note: this does not help code generated at run time (V8), that is track C.
1. Measure first: today's `--prewarm` coverage and cost for DeepSeek Harness's Electron Framework (blocks,
   seconds, cache size) vs what a real session executes (profile-guided AOT); disk budget per app.
2. Design: function discovery (symbols, LC_FUNCTION_STARTS, call targets), an IR per function or region,
   optimizations, code layout, cache format compatible with today's loader; background scheduling at low QoS;
   cache size limits (the user finds ~1 GB too much: propose a budget and eviction).
3. Implement an end-to-end slice: one optimization in the offline pipeline with a measurable gain on bench/bench.c
   and kernels.c when loaded from cache, self-tests on the optimized code (differential vs interpreter),
   seeds/sensitivity. Report what the full version would take.

## Addendum (Claude, schedule): items merged into track D for its next round
- #15 Cache size and warm-up speed: the AOT cache reaches 0.7-1 GB (users notice; a test run saw 4.3 GB swap);
  write AOT files in increments (chunks appended at checkpoints, so a 60-second session already persists the
  big images; today the Electron Framework AOT only appears ~160 s into a session), reduce the speculative
  expansion (`build()` follows direct branches up to 4x the profile), consider compression of cold parts.
- #16 Idle-time background prewarm: translate installed apps' images while the machine is idle (QoS background,
  pause on activity/battery), so first launches are warm.
- #32 Hot/cold code layout inside AOT files from profiles (hot blocks together).

## Round 2 (Claude) — priorities after round 1
Round 1 committed (87258e3 design/measurement, a4f1d19 opt-in `offline-dse`; no measurable speedup: keep it
opt-in, do not spend more on it). The key finding is that `--prewarm` covers **zero** of the Electron Framework
(relocation), and that framework holds most of DeepSeek Harness's execution. In order:
1. Make offline/prewarm translations of large slid dylibs (Electron Framework) loadable: position-independent
   blobs keyed by image UUID so the loader can use them at the actual load address (follow how runtime AOT
   files for `lib-<uuid>-<base>` work today; base-independence if possible). Prove with the headless-Node run
   that framework blocks load from the prewarmed cache (ATTESOR_STATS) and that translation counts drop.
2. #15 incremental AOT: append-only chunks written at checkpoints, so a 60 s session persists the big images
   (today the Electron Framework AOT appears only ~160 s into a session); reduce speculative expansion
   (`build()` follows direct branches up to 4x the profile) and measure cache size vs warm-start blocks.
   Size budget per app with eviction (propose numbers; users notice ~1 GB).
3. #16 idle prewarm agent (low QoS, pauses on activity/battery) using 1+2.
4. #32 hot/cold layout, only if 1-3 are done.
QA: tiered (COMMON.md). Measure warm-start translation counts and CPU, raw numbers.
