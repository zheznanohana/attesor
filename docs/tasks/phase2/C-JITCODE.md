# Track C — fast paths for run-time generated code (V8, JSC, Wasm, .NET, regex engines)
Branch `jitcode`, worktree `~/attesor-p2c`. Read COMMON.md.
JS under Attesor runs 5-6x slower than native Chrome (headless-Node benchmark ~/attesor-dshprof/js/js.js:
loops 494 ms, closures 302 ms, sort 553 ms under attesor). V8's tiers help even under translation
(--jitless is 2-8x slower), so V8's generated code is the thing to make fast.
1. Measure first (most important deliverable of this track): for the JS benchmark, split time into: translating
   guest code generated at run time, invalidations (count, bytes, which syscalls/IC IVAU trigger them, how much
   re-translation follows), dispatcher/indirect-branch lookups, calls/returns, and execution of translated code.
   Use/extend the profiler (states, counters). Report a table and the top 3 costs.
2. Then fix the top costs, e.g.: a cheaper first translation tier for code that is likely short-lived;
   invalidate only the changed range and keep translations of unchanged neighbours; inline caches for
   indirect calls (BLR) keyed by target; faster IBTC; avoid re-translation after W^X mprotect flips that did
   not change bytes (compare a hash of the guest bytes).
3. Each change A/B on the JS benchmark (raw numbers), self-tests, the TLS check, seeds/sensitivity.

## Addendum (Claude, after a review of prior art)
Block chaining already exists (`chain_slot`, patched exit slots, unlinked on invalidation; IBTC for indirect
branches): do not re-implement it. Candidates to evaluate only if your measurements point there:
- Persistent cache for run-time generated code (FEX-Emu 2610 style: key by content hash + translator version,
  size cap ~1 GB, automatic cleanup on version change). Measure the hit rate first: V8 code embeds absolute heap
  addresses that change per run, so content hashes may rarely match (regexp/Wasm code may).
- Page/region translation at mprotect(PROT_EXEC) time: only if most generated code becomes hot.
- Direct rel32 patching of chain slots instead of `jmp [slot]` (small).
Borrow architecture only: QEMU is GPL; do not copy code from it.

## Round 2 (Claude)
Round 1 committed (74a004b profiling, 3ed3cbf slotreuse). Your table is dominated by start-up (39% image
translation/loading in a ~3 s run), while the 5-6x gap users see is measured inside JS (Date.now around each
kernel, i.e. steady state). Next:
1. Profile steady state only: warm up each kernel in js.js, then start sampling (add a profiler start/stop
   control, e.g. a signal or a file trigger, or a time window) and report the same table for the steady phase
   per kernel (loops, closures, sort, objects, json, regex, strings): execution of translated code vs
   dispatcher/IBTC vs runtime translation vs invalidation vs syscalls, and the hottest guest PCs/blocks
   (which V8 tier generated them, block count of their loops).
2. From that, list which wave-2 tracks (E translation speed, F calls/IBTC, H memory, I isel) and track A
   (multi-block loops) would address what share; fix anything that is clearly C's scope (run-time code) now.
