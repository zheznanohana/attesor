# Track A — multi-block loop register allocation (loopra phase 2)
Branch `loopra2`, worktree `~/attesor-p2a`. Read docs/tasks/LOOPRA.md (phase 1) and COMMON.md.
Goal: extend write-back regions from single-block self loops to natural loops of several blocks (if/else
inside the body, early exits, nested loops' inner loop), in JIT units and AOT units, including code that V8 and
other JIT runtimes generate at run time.
1. Measure first: with the profiler / a new counter, how many hot loops in the headless-Node JS benchmark and in
   bench/bench.c + kernels.c are multi-block, their block counts and back-edge shapes; how much time V8's
   generated code spends in loops that phase 1 does not cover. Report this before building.
2. Region formation: a unit holding the loop's blocks (header dominates the body, bounded size), one register
   assignment for the whole region; every edge leaving the region writes back; internal edges jump directly
   with the region's register state; joins must agree on the state (same assignment everywhere in the region).
3. Same exits/helpers/cold-path rules as phase 1; flags across internal edges use the existing liveness rules.
4. Tests: extend `loops` to multi-block bodies (diamonds, early exits, inner loops, side exits from any block,
   helper calls in one arm), seeds, sensitivity. Measure branchy/qsort/sieve/fib and the JS benchmark A/B.

## Round 2 (Claude)
Round 1 committed (opt-in `loopra2`). Code got better (loop body 17 -> 10-11 insns) but JS is 1.72x slower
with it on. Find out why before changing more:
1. Profile on/off (`ATTESOR_PROFILE`, the jitcost counters from track C if useful): translation time and
   blocks/units translated, region entries/exits per second, exit write-back counts, cold-path/helper barriers
   inside regions, IBTC/dispatcher effects (regions change unit layout), per JS kernel (steady state:
   warm up, then measure).
2. Fix the dominant cause, e.g. form multi-block regions only for loops proven hot (count back-edge executions
   in the plain translation first, then retranslate the loop as a region — a second tier), cap region size,
   avoid regions with in-body helper barriers, keep the region's entry/exit cost low.
3. Goal: no kernel slower than off; report per-kernel on/off ratios for js.js, bench.c, kernels.c.
