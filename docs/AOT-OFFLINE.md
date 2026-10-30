# Track D: offline optimizer design and acceptance

## Scope and measurement-first findings (2026-10-29)

Worktree: `attesor-p2d`, branch `offline`. All measurements use this tree's
release binary, private `target/track-d` caches, and headless Node only. No GUI,
application mutation, user cache, or unrelated process management is involved.
Raw evidence is retained under `target/track-d`; final results follow below.

Before implementing the optimizer, the first stock whole-bundle prewarm took
24.292 s (uptime load averages 13.96 / 12.08 / 10.96): 14 images,
2,249,394 blocks. The main Electron launcher has just 34 blocks. Most output
belongs to bundled LibreOffice, Node, Python and ripgrep executables, not to
Electron Framework. The framework is MH_DYLIB; today's CLI excludes it because
its runtime slide is unknown. Thus static framework coverage is **zero**, not
the bundle's total. Profile-driven runtime translation must be compared by UUID
and load address, not by total cache blocks or sampled PC counts. Headless JS
is a reproducible executed-session proxy, not GUI readiness or a whole-app trace.

## Whole-image design (not all implemented by this slice)

1. **Discovery.** Enumerate executable sections of the selected ARM slice.
   Merge validated LC_FUNCTION_STARTS, text symbols and direct BL targets;
   LC_MAIN/LC_UNIXTHREAD seed stripped main images. Decode reachable basic
   blocks, keeping call continuations and indirect exits. Function starts are
   hints, not trusted bounds: tail calls, shared epilogues, jump tables and
   overlapping entries require conservative regions. Never execute guest code
   during preparation. Cap the worklist *before insertion* and bound blocks,
   bytes, memory and elapsed time; the current discovery count is not a strict
   budget (pending entries can exceed its nominal limit).
2. **IR.** Use function/region CFGs with typed GPR/SP/ZR, vector lanes, NZCV,
   explicit memory/helper effects and side exits. Keep canonical public entries;
   private entries may carry allocated values only when all predecessors agree.
   Unknown instructions, calls, FPCR changes, exceptions, invalidation and
   dispatch exits observe architectural state. Cross-block register allocation
   needs spill/reload reconciliation, not just a register map at the successor.
3. **Optimizations.** First ship conservative dead Cpu GPR stores in the emitted
   x86 region IR (below). Later add guest-level constants for ADR/ADRP+ADD,
   all-path NZCV liveness, loop/function allocation and dead vector stores.
   Distinguish guest memory stores (observable, never removed here) from stores
   to Attesor's private Cpu state. Preserve all externally observable state.
4. **Layout.** Group by function, then hot successor edges, preserving every
   public entry in the index; cold helpers/side exits follow hot code. iced's
   BlockEncoder handles branch relaxation. No absolute process host pointers.
5. **Cache.** This slice retains MCLAOT05: the same PC/offset index, alignment,
   locks, atomic publication and loader. Bump translator version and retain
   JIT option/build UUID/host-feature compatibility. Today's dylib cache is
   keyed by UUID + runtime address. A truly installed whole-framework cache
   needs image-relative guest operands and relocations applied at load time,
   validated against UUID/slice/text hashes and host codegen identity. Do NOT
   publish MAIN_BASE translations under runtime library keys. ASLR support is
   a prerequisite, not something `--prewarm` currently supplies.
6. **Scheduling.** Keep explicit CLI synchronous. An opt-in installer/first-use
   coordinator should spawn a bounded, cancellable worker at
   QOS_CLASS_BACKGROUND (0x09), one image/chunk at a time, with thermal/battery/
   interactive-work backoff and resumable manifests. Existing checkpoints
   already set this QoS; no daemon or launch registration is added by this slice.
7. **Disk policy proposal.** Default 128 MiB per app (including attributable
   library translations), 256 MiB global translations, and 384 MiB global
   shared DSC/slid-data storage: 640 MiB total, not an unbounded hidden cache.
   Profiles, metadata and temporary replacements count inside these caps.
   Evict an old generation before replacement if two copies cannot fit; readers
   already holding file descriptors/copies remain valid. A user can lower or
   disable disk persistence; over-budget images simply fall back to translation.
   Profile-first/hot functions, then startup paths; skip huge ancillary tools
   unless launched. Account index, code, profiles and in-flight replacement
   (reserve room before writing). Under stable advisory locks, remove stale
   UUID/codegen files first, then LRU cold images; do not unlink lock inodes.
   Shared files need a manifest/reference ownership model; active mmap/copy
   users remain valid after eviction. The existing 30-day tidy is not a byte
   quota. No claim that the proposed quota is enforced in this slice.

## End-to-end slice contract

`ATTESOR_JIT_ON=offline-dse` enables a post-emission AOT-only pass. Default and
runtime JIT code are unchanged. The region IR is iced instructions plus symbolic
labels. Backward must-overwrite dataflow intersects successor sets, with a
zero-initialized least fixed point (cycles alone cannot prove a value dead).
Only aligned full 64-bit MOV stores to Cpu.X0..SP may be removed. Reads of any
part of a slot make it live. Non-GPR memory, helper calls, unknown/indirect exits,
unsafe host operations, or writes to the Cpu base register are barriers. The
integration with `jitcost` permits only its exact full-width immediate `Jit`
entry marker to private `Cpu.profile_state` to be transparent to GPR liveness;
the marker itself remains unchanged. Reads, partial/register-sourced writes,
other marker values and all other non-GPR fields remain barriers. In particular,
`loopcensus` publishes architectural PC at every public entry. Those writes stay
barriers and can eliminate all DSE opportunities in the per-instruction-entry
self-test; non-census runs separately require positive removed-store coverage. Thus
neither guest memory faults nor helpers can observe deferred Cpu state. A W
write's emitted full zero-extending X store is eligible; byte/dword stores are
not full kills. Other Cpu fields, flags, FP state and guest memory are untouched.

Removed instructions become zero-byte pseudo instructions, retaining original
instruction indices and symbolic IPs, so public/internal labels, cold paths,
constants and cache index relocation remain exact. Preserve the original
assembler on failure. Report removed stores and original/optimized host
instruction counts during stats-enabled builds. Differential tests must execute
the same pass and serializer/loader, not merely the unchanged JIT with a flag.
Sensitivity tests deliberately ignore an observer or one branch successor in a
test-only mutation and require interpreter mismatches; no fault-injection flag
is shipped. Directed tests also cover partial overlap, barriers, loops, public
alternate entries, register widths/SP/ZR and cache identity rejection.


## Baseline coverage and disk measurements

Baseline build: translator 53, git `a632091`, release build before any translator
edit. Alternating fresh whole-bundle prewarm and fresh synchronous profiled JS
session (three pairs); wall time includes publication. No other job from this
track ran during these measurements. Load columns are the raw three `uptime`
averages (two logged-in users). Cache byte counts are logical file sizes.

| Pair | Prewarm seconds | Prewarm uptime load | JS + profile-AOT seconds | Session uptime load |
|---|---:|---|---:|---|
| 1 | 24.292163 | 13.96 / 12.08 / 10.96 | 18.987776 | 13.08 / 12.01 / 10.96 |
| 2 | 23.144716 | 11.92 / 11.81 / 10.91 | 15.932839 | 11.17 / 11.64 / 10.88 |
| 3 | 20.122801 | 10.77 / 11.52 / 10.85 | 13.092309 | 9.76 / 11.26 / 10.77 |

Every bundle prewarm produced 2,249,394 blocks / **334,143,328 bytes**
(318.664 MiB). This includes dyld: 30,411 blocks / 4,306,592 bytes. Launcher:
34 blocks / 19,200 bytes. The bulk comes from unrelated-to-this-session bundled
executables. Direct framework `--prewarm` failed as expected in 0.016 s with
`not an arm64 executable`: framework static blocks/cache bytes = **0 / 0**.

Framework UUID `4c4c44be55553144a1e383408f6c571f` loaded at `0x4000004000`:

| Session | Recorded profile PCs | Expanded AOT blocks | Framework AOT bytes | All AOT bytes | All cache bytes |
|---|---:|---:|---:|---:|---:|
| 1 | 166,619 | 801,998 | 102,572,576 | 123,478,112 | 486,510,720 |
| 2 | 168,588 | 811,564 | 103,894,480 | 124,797,728 | 487,845,992 |
| 3 | 169,547 | 816,405 | 104,548,288 | 125,462,256 | 488,518,608 |

The session cache includes **361,369,600 bytes of pre-slid shared-cache data**,
not translated instructions. Do not add this silently to an app's code budget.
The profile PC set records JIT/AOT discovery, including inlined/expanded blocks;
it is **not an exact execution-frequency trace**. Expanded AOT blocks are not
executed-block counts either. Comparing UUID/address-specific sets establishes
zero static framework coverage of all these recorded PCs, but does not establish
an exact dynamic percentage for other images or dynamically generated V8 code.
`tools/profile-report.py` on session 1 reports 80,002 translation units / 5,341
ms and 79,085 samples (81.99% idle, 6.77% translate, 6.69% JIT). This supports
where compilation work happens, not an Electron first-launch speedup claim.
Raw logs/JSON: `target/track-d/baseline*`, `prewarm-{1,2,3}.stderr`,
`js-{1,2,3}.{stdout,stderr}`, `profile-report-1.txt` and `profiler-{1,2,3}`.
Baseline headless TLS/crypto passed local SHA/AES-GCM/ECDSA/ECDH work; HTTPS
reported `https error ENOTFOUND`, so a 401/network-success assertion is unavailable.

Reproduce the sequence with a fresh private cache per sample, synchronous
`ATTESOR_AOT_SYNC=1`, `ATTESOR_AOT_CHECKPOINT=0`, `ATTESOR_STATS=1`,
`ELECTRON_RUN_AS_NODE=1`; run `--prewarm <bundle>` then headless `<electron>
<js.js>` with `ATTESOR_PROFILE=<private-dir>`, repeated three times. The exact
script and inputs/paths used are retained in `target/track-d/baseline.py`.

## End-to-end benchmark measurements

The initial `python3 bench/offline-measure.py` series compiled the **unchanged** `bench/bench.c`
and `bench/native-ratio/kernels.c` with `clang -O2 -arch arm64`. The same binary
and stats configuration built two fresh caches (flag absent / `offline-dse`),
then ran A/B three times. Every run verified a nonzero **main-image cache load**
and SHA-256-identical AOT files before/after; automatic builders were disabled.
No runtime optimization code was added to the JIT. Results are from the same
emitted optimizer as the final build; subsequent edits only strengthen tests
and add fail-closed handling for an encoder error.

| Main image | Public blocks | Emitted iced IR entries before → after | Removed real MOV stores | Code bytes before → after |
|---|---:|---:|---:|---:|
| bench | 112 | 7,075 → 7,060 | 15 | 36,608 → 36,544 (-0.175%) |
| kernels | 27 | 2,129 → 2,122 | 7 | 10,928 → 10,896 (-0.293%) |
| dyld (each cache) | 30,411 | 782,238 → 775,705 | 6,533 | 3,815,072 → 3,783,664 (-0.823%) |

Iced IR counts include any data/zero-byte pseudo-ops; the removed counts are
actual full-width MOV instructions. These are deterministic code-size/static
instruction-work reductions in the files actually loaded, **not dynamic retired
instruction counts or a demonstrated wall-time speedup**. Total main files
include a 16 KiB minimum header/index area: bench 52,992 → 52,928 bytes;
kernels 27,312 → 27,280 bytes. Cache index coverage is unchanged.

Preparation seconds (baseline → offline): bench+dyld 0.417995 → 0.855787;
kernels+dyld 0.279040 → 0.651844. Extra offline work is intentional. Preparation
load was 7.71 / 7.85 / 9.04 (bench pair) and 7.90 / 7.89 / 9.04 (kernels pair).

Raw process wall seconds, each row in actual alternating order:

| Pair | bench A | bench B | B/A | kernels A | kernels B | B/A |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 4.421207 | 5.706019 | 1.291 | 5.765041 | 8.916534 | 1.547 |
| 2 | 5.300377 | 2.727466 | 0.515 | 3.424588 | 3.212271 | 0.938 |
| 3 | 2.230262 | 2.266479 | 1.016 | 3.135528 | 5.275253 | 1.682 |

Raw `uptime` load averages for those processes (bench A, kernels A, bench B,
kernels B in each pair):

- Pair 1: 7.90/7.89/9.04; 10.63/8.46/9.23; 11.14/8.60/9.28; 14.25/9.28/9.52.
- Pair 2: 18.90/10.46/9.93; 20.99/11.03/10.14; 20.03/11.00/10.13; 20.03/11.00/10.13.
- Pair 3: 19.15/10.96/10.12; 19.15/10.96/10.12; 18.09/10.88/10.10; 18.09/10.88/10.10.

The measured static/code-footprint gain is small; the wall timings are unstable
and often regress. **Do not enable this by default or claim a timing win.**
Raw per-kernel milliseconds, preparation output, loads, checksums, and complete
uptime strings are in `target/track-d/perf/results.json`. A broader function IR,
register allocation or more profitable pass is required for a substantial runtime
benefit; a quiet-machine follow-up is still necessary.

## Final-binary repeat with private slide-data warmup

After the final Rust build (including the encoder-failure fallback), the driver
was strengthened to run and verify one untimed warmup per cache before the
three alternating pairs. This materializes private DSC slide data equally on
both sides. This repeat uses the **final binary UUID**, so its retained A/B
blobs, unlike historical build artifacts, remain compatible with that binary.
All 16 warmup/measured processes loaded their main AOT blob; all AOT hashes
remained unchanged. Deterministic removed-store and code-byte counts above
were reproduced exactly (bench 15/64; kernels 7/32; dyld 6533/31408).

| Pair | bench A seconds | bench B seconds | B/A | kernels A seconds | kernels B seconds | B/A |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 1.999144 | 2.511855 | 1.256 | 2.539077 | 4.293577 | 1.691 |
| 2 | 1.839735 | 2.059912 | 1.120 | 2.608574 | 2.895003 | 1.110 |
| 3 | 1.960957 | 1.839645 | 0.938 | 2.596706 | 2.583728 | 0.995 |

`uptime` load averages (bench A, kernels A, bench B, kernels B):

- Pair 1: 12.37/10.56/11.12; 12.37/10.56/11.12; 11.94/10.50/11.10; 11.94/10.50/11.10.
- Pair 2: 11.54/10.44/11.07; 11.42/10.44/11.07; 11.42/10.44/11.07; 10.98/10.36/11.04.
- Pair 3: 10.98/10.36/11.04; 10.67/10.31/11.01; 10.67/10.31/11.01; 10.67/10.31/11.01.

Preparation seconds and load averages (each includes main + dyld):

- `prepare-bench-baseline`: 0.458853 s; 6.81 9.52 10.79.
- `prepare-bench-offline-dse`: 1.763285 s; 7.47 9.61 10.81.
- `prepare-kernels-baseline`: 1.015104 s; 7.47 9.61 10.81.
- `prepare-kernels-offline-dse`: 1.168540 s; 7.47 9.61 10.81.

This warmed-data repeat still **does not establish a wall-time gain**; in the
first two pairs both workloads regressed. The only accepted measurable gain
here is code footprint/static MOV elimination. Do not infer runtime performance
acceptance from reduced code bytes. Raw per-kernel output, wall times, full
uptime strings, warmup records, cache loads and hashes:
`target/track-d/perf-final/results.json`; transcript: `perf-final-run.log`.

## Correctness acceptance and remaining work

All required gates completed before preparing the patches, with no default
rollout. Logs live in `target/track-d`:

```sh
# Run the complete matrix once with the flag absent and once with this flag:
ATTESOR_JIT_ON=offline-dse cargo test --release --workspace -- --test-threads=1
# attesor: 73 passed; core: 9 passed, in EACH configuration.

# Each class received 10,000 attempts in EACH configuration; parse mismatch=0,
# since some historical classes do not return a failing status on a mismatch:
# blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks
ATTESOR_JIT_ON=offline-dse target/release/attesor --selftest-jit blocks 10000

# Run each of 1, 0x123456789abcdef, 0xdeadbeef with AND without the opt-in:
ATTESOR_JIT_ON=offline-dse target/release/attesor --selftest-jit loops 10000 1
ATTESOR_JIT_ON=offline-dse target/release/attesor --selftest-jit offline 1000 1

ATTESOR_JIT_ON=offline-dse tests/attesor/loopra-aot-check.sh target/release/attesor
# Also run unflagged: all 7 workloads passed in BOTH configurations.

python3 tests/attesor/offline-check.py --attesor target/release/attesor
python3 tests/attesor/aot-prewarm.py --attesor target/release/attesor
python3 tests/attesor/aot-checkpoint.py --attesor target/release/attesor
cargo test --release -p attesor jit::offline -- --test-threads=1 --nocapture
```

- Workspace: 82 tests passed per configuration, no failures.
- All 26 general class summaries: **mismatch=0** (attempts can be skipped by
  historical randomized suites; full counts are retained in the logs).
- Additional loops: 10,000 per seed/configuration, all mismatch=0, exercising
  Q scale-16, negative SXTW, aliases, maximum-length loops and nested entries.
- Disk-backed `offline`: 1,000 per seed/configuration, **6,000 total**, all
  mismatch=0. Every 1,000-sample run loaded 6,510 public block entries, and an
  assertion rules out replacement JIT translations. Each flagged seed removed
  445 stores. This tests real serialization/header/index/loader execution,
  including W zero-extension, SP/ZR, both diamond edges, alternate public
  entries, loops, memory/CRC helper/SVC observers, Q pairs and scaled offsets.
- Seven offline unit tests passed. Sensitivity deliberately removed a live
  store in test-only code: read observer **1 mismatched slot**, conditional
  successor **2 mismatched slots**, both versus the interpreter. The correct
  optimizer matches the interpreter before each deliberate mutation.
- New process test: native = interpreter = default JIT = flagged JIT = warm
  optimized AOT, checksum `54d1913a2b51bd3b`; 6 main-image stores removed;
  two cache loads byte-identical; incompatible caches rejected both ways.
- Existing static prewarm: 29 checks passed. Checkpoint/fork/failed-exec:
  seven cases passed with the opt-in; a separate unflagged run is recorded in
  `checkpoint-default.log`. Existing build warnings were not hidden or fixed
  as unrelated work. Python syntax checks use a private PYCACHEPREFIX, and the
  shell script passes `bash -n`; `git diff --check` passes.

### Real headless Electron, optimized framework actually loaded

The final binary built a profile-derived optimized framework blob, removing
122,974 stores from 18,907,551 iced IR entries. A fresh headless JS process then
**loaded 814,500 framework blocks** (plus main, dyld and DSC), and every JS result
matched the pre-change baseline: loops 28665, objects -259384288, JSON 80000,
regex 400000, strings 2025151008, closures 19360, sort 6448448. This is disk-load
and correctness evidence, not an alternating runtime-speedup experiment.
The TLS script also loaded that optimized framework and matched local crypto
and Unicode results, but again printed `https error ENOTFOUND`. Networking was
unavailable; **`https status 401` is not claimed**. Logs:
`electron-check.log`, `electron-offline-{build,warm,tls}.{stdout,stderr}`.

### Full-version prerequisites / open risks

- This is a conservative **region** optimizer over the existing sorted
  4,096-block chunks, not a full guest function IR or a whole-framework install
  translator. Cross-chunk edges fail closed. Dynamic V8 translation is unchanged.
- Implement validated symbol/function/unwind/indirect-target discovery and strict
  discovery/compile limits, then effect-typed guest IR with edge reconciliation,
  address constants and cross-block allocation. Require independent interpreter
  and native checks for each operand form, fault/helper/FPCR exits and reentry.
- Implement image-relative guest-address relocation and a validated new cache
  format/loader before installed frameworks can be prewarmed at unknown slides.
  Existing immutable AOT entries are not tracked in the JIT invalidation index:
  this test explicitly clears its own registrations before rewriting a private
  test page. Production support for mutable image text needs a separate lifetime/
  invalidation design; do not extend this pass to runtime-generated code.
- Implement cancellable background scheduling, per-app ownership manifests,
  publication-reservation accounting and the byte/LRU quotas proposed above.
  None of those deployment policies is enforced by this slice.
- Code-footprint gain is verified but small; wall-time gain is **not** established.
  Quiet-machine serial acceptance and reviewer measurements are still required.
  Keep `offline-dse` opt-in until Claude accepts it; no default enabling or push.

### Artifact retention and commit fallback

After all processes exited, the named private measurement caches were archived
as `target/track-d/cache-artifact-manifest.json` (file sizes, AOT headers and
SHA-256), with raw PC sets preserved under `profile-sets/`, then their bulk was
removed. Redundant benchmark `.dscslide` data was also removed, recorded in
`removed-perf-slide-data.json`. This removed multiple gigabytes of experimental
artifacts; logs, raw JSON, profiles, benchmark executables and the final-binary
small A/B AOT blobs remain (roughly 26 MiB total track-D evidence). No user cache was involved.
First timing samples included creation of private pre-slid data; subsequent
samples reused it. This is another reason not to infer startup speedup from
these timings. Re-running the performance driver requires a fresh output
`--directory`; loading the retained final caches will regenerate their slide data.

Git metadata is outside the permitted worktree, in the protected main checkout.
No staging, commit or push was attempted there. Per-step patches and messages:

- `target/d-offline-01-design.patch`
- `target/d-offline-01-commit-message.txt`
- `target/d-offline-02-implementation.patch`
- `target/d-offline-02-commit-message.txt`

All code is left in this `offline` worktree for review. Full correctness and
measurement transcripts are retained under `target/track-d/`; the full gate
commands are also preserved in `target/track-d/run-gates.sh`.
