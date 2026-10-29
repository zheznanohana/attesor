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
