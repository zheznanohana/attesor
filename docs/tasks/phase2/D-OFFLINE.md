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
