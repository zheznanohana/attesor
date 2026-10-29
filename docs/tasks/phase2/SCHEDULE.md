# Phase 2+ schedule — all 35 directions (Claude schedules and reviews, Codex implements)
Rule: tracks that edit the translator core (`jit/emit.rs`, `jit/mod.rs`) run in sequence, not in parallel;
tracks that edit other subsystems run in parallel. Every change is opt-in until measured and accepted.

## Wave 1 (running since 21:55, from v0.3.0-internal)
| Track | Items | Area |
|---|---|---|
| A loopra2 | #2 multi-block loop register allocation | emit.rs (regions) |
| B avx512 | #9 AVX-512 V registers, NaN fix-ups | emit.rs (SIMD/FP) |
| C jitcode | #14 run-time code cache (if measured), #19 precise invalidation | jit/mod.rs, emit.rs (small) |
| D offline | #3 function-level optimizer, #15 cache size/increments/compression, #16 idle prewarm, #32 hot/cold layout | aot.rs, new offline pipeline |

## Wave 1b (now, parallel: no translator-core edits)
| Track | Items | Area |
|---|---|---|
| G gpu | #25 GPU bridge batching/zero-copy, #27 VideoToolbox hardware bridge | gpu/ (guest libmclmetal + host libmclbridge) |
| P process | #28 shared translations across an app's processes, #29 zygote/fork server, #30 dyld launch-closure cache, #23 Mach IPC/thread-sync emulation costs, #24 path-mapping caches, #35 thread QoS (translation in background, UI threads first) | spawn.rs, threads.rs, syscalls.rs, cache/AOT sharing via shared memory, loader.rs |
| N native | #26 native host replacements for system libraries (image, Accelerate/vImage/vDSP, compression, CoreFoundation hot paths) chosen by profiling native Mac apps | native/, loader interposition |

## Wave 2 (after wave 1 merges; translator core, one at a time or with disjoint files)
| Track | Items | Area |
|---|---|---|
| E translate | #11 direct byte emitter for hot templates, #12 tiered translation, #13 background translation of successors | emit.rs encoder, jit/mod.rs |
| F calls | #10 leaf inlining + per-site inline caches for BLR, #18 IBTC size/associativity, #20 rel32 chain patching | emit.rs branches, jit/mod.rs dispatcher |
| H memory | #6 drop the TBI mask on accesses (fault-and-fix for tagged pointers), #31 huge pages for the code cache, #33 16 KiB page-semantics costs | emit.rs ldst, signals/fault handler, code allocator |
| I isel | #5 NZCV in a host register / cross-block lazy flags, #7 BMI2/LZCNT/POPCNT/CRC32 instruction selection | emit.rs dp templates |

## Wave 3 (after wave 2)
| Track | Items |
|---|---|
| T tune | #34 per-app automatic choice of opt-in switches from profiles (with guard rails), final re-profiling of DSH, VS Code and one native Mac app |
Already done: #1, #4, #8, #17, #21, #22. Not done by decision: #36 per-app runtime swap, #37 kernel extension, #38 V8 flag changes.
