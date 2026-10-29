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
