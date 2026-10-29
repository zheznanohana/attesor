# Optimization work tracking

Goal: continue all eight directions from the Claude plan, improving compatibility
and performance toward native execution. Rosetta 2 translates the opposite
direction and cannot run as a comparator on this Intel host. Ratios measured
here are against the same workload compiled for native x86-64, not a Rosetta score.
No general speedup or completion claim follows from a single microbenchmark.

The eight-agent first integration round is recorded in [TEAM-ACCEPTANCE.md](TEAM-ACCEPTANCE.md),
including updated test evidence, deployment and serial measurements. The table below is the initial
audit baseline, retained for context.

## Scope and completion evidence

| Direction | Current evidence | Remaining work / completion evidence needed |
|---|---|---|
| 1. JIT code quality | Existing flag fusion, register read caches, opt-in CALLRET; scalar source-alias reuse and removal of redundant FMA NaN tests added in this round | Benchmark loops, FP, calls and branches; investigate cross-block register lifetime and dispatch cost; resolve existing FP mismatches; verify real apps |
| 2. Ahead-of-time translation and persistent cache | Periodic in-process checkpoint implementation exists | Prove cache load and startup benefit after forced termination; investigate unflushed per-thread profile tails, rebuild races, and background pretranslation |
| 3. Crypto extensions | AES/PMULL/SHA paths and probes exist | Audit instruction variants and host feature guards; compare native outputs and HTTPS workload performance |
| 4. Remaining hot NEON instructions | Multiple SIMD translation batches landed | Collect current fallback histograms in actual app processes; prioritize remaining runtime hotspots and verify each native path |
| 5. Debugger | GDB remote stub and regression script exist | Run protocol regression; audit threading, symbols and unsupported debugger operations; document actual coverage |
| 6. Other compatibility | FPCR work, VideoToolbox round trips and MetalFX/ray-tracing probes exist | Revalidate probes; investigate hardware video path; retain precise limits for sandbox and identity-sensitive APIs |
| 7. Direct ARM program launching | Shell/child-process support; Claude's uncommitted universal-stub launcher changes remain in the worktree | Review and verify double-click, argument forwarding, restore, updates and Dock behavior; no kernel extension planned without evidence it is needed |
| 8. Host-native library bridges | Proposal only beyond existing GPU bridge | Profile candidates, select a concrete library, implement ABI/callback/lifetime handling, verify semantics and end-to-end speed |

## Round 1: scalar FP cache inputs and FMA checks (2026-10-29)

Scalar FP operand loads now have a snapshot of the vector cache before the
destination is invalidated. This permits a destructive instruction such as
`fadd d0, d0, d1` to read its old d0 from a host register. Only the scalar
operand loader can use the snapshot: all of its uses precede any destination
write, cache allocation or helper call. General vector loaders retain the
conservative invalidation policy. `ATTESOR_JIT_OFF=scalaralias` disables this
specific optimization for comparison.

Fused multiply-add already branches around its NaN handling when no result lane
is NaN. Default-NaN canonicalization now happens inside that slow path, avoiding
a second NaN check for normal scalar, vector and by-element fused operations.
Non-fused multiplication keeps its own fixup. The translator version is 36.

Validation on this host:

- Release workspace build succeeds (existing compiler warnings remain).
- 100,000 FP blocks: baseline and changed builds produce identical diagnostic
  output: 99,378 matching, 622 mismatching, zero skipped, zero fallback instructions
  among the translated blocks. These are existing correctness failures, not a
  fully passing suite. The first 15 printed failures and final counts match.
- Scalar FMADD class: 16,000 attempts, 11,945 matching, zero mismatches,
  4,055 skipped, 7,960 native translations.
- Vector FMLA class: baseline and changed output identical: 12,019 matching,
  three mismatching, 3,978 skipped, 12,022 native translations. Existing NaN
  failures remain to be fixed.
- By-element class: 12,000 attempts, 3,417 matching, zero mismatches,
  8,583 skipped, 561 native translations. This sample does not prove all variants.
- `tests/attesor/fpalias.c`, built at `-O2 -ffp-contract=off` for both architectures,
  prints bit-identical final double/single results in native x86 and translated ARM.

Use `bench/compare-cpu.py` to collect alternating, serial comparisons and retain
raw samples. Do not run correctness tests concurrently with timing measurements.
App performance and installation remain separate verification steps.

Measured without concurrent test jobs, seven alternating pairs, AOT disabled:

| Workload | Before median | After median | Time reduction |
|---|---:|---:|---:|
| FP n-body kernel | 144.2 ms | 132.9 ms | 7.8% |
| Long standalone FP program (whole process) | 1988.4 ms | 1892.8 ms | 4.8% |
| Integer hash | 381.5 ms | 380.2 ms | 0.3% |
| qsort | 1107.2 ms | 1104.2 ms | 0.3% |
| memcpy | 27.7 ms | 27.9 ms | -0.7% |
| Branch state machine | 1079.6 ms | 1082.8 ms | -0.3% |

Changes under 1% are not treated as demonstrated improvements or regressions.
The complete mixed benchmark's process wall time was essentially unchanged.
Raw samples and binary SHA-256 hashes are in
[the kernel report](measurements/2026-10-29-fp-kernels.json) and
[the long FP report](measurements/2026-10-29-fp-long.json).
The first scalar-alias-only experiment had a small, noisy effect; these final
measurements are for both changes together. No end-to-end GUI speedup has been
established. This candidate has not replaced `~/bin/attesor` yet.

## Thread startup regression discovered during native-library testing

A four-worker create/join probe without zlib reproduced an existing race in the
baseline: the first repeated process terminated with SIGSEGV. `bsdthread_create`
returned while the host child had not yet published its Mach thread port in the
guest pthread allocation. The creator now waits for kernel-style setup to finish
before returning; execution of the guest entry point still happens concurrently.

`python3 tests/attesor/thread-startup-check.py target/release/attesor` compares
with native x86 and runs ten independent ARM guest processes, each creating and
joining 120 immediately-returning workers. All 1,200 guest thread results matched.
This is targeted regression evidence, not proof of all guest thread lifetimes.
