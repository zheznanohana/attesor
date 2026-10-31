# Local stage 1: experimental V residency (not the 60% milestone)

Rosetta translates x86 guests to ARM hosts; Attesor does the opposite. A mapping
cannot simply be inverted. Attesor already has an AVX512-backed 24-XMM cache.
This experiment changes **lifetime**, not cache capacity or guest ISA semantics.

## Evidence boundaries

The successful bounded cloud survey is [run 41088137438](https://github.com/zheznanohana/attesor/actions/runs/41088137438).
On macOS 26.6.2 (25G83), Ghidra actually decompiled 24 selected functions each
from runtime and libRosettaRuntime (zero failures in these selections). Runtime
SHA256: `07416ace5466a31e4850543b93df4240ec963e1cc6e69b157ee904736b271e16`;
library SHA256: `8e75b2b1ab1e5659de7d02ae4d612bed893ae5cf0fd717757bf0fa51039d631d`.
Translation-cache/ABI/JIT/AOT structures and flag-live-in/out string references
were observed. That does **not** recover Apple's allocator or prove its current
optimizations. The dynamic translated ARM instruction stream was not captured.
No Apple binary, full decompilation, or protected cache is redistributed.

The independently authored local design adapts resident-state/canonical-exit
principles discussed in [Dougall Johnson's historical public analysis](https://dougallj.wordpress.com/2022/11/09/why-is-rosetta-2-fast/).
It is not represented as a lifted Apple algorithm.

## New opt-in `vloop`

- Requires the existing GPR `loopra` proof of a single natural self-loop.
- All touched V registers must fit: 8 slots without AVX512, 24 with it.
- Accept only audited scalar S/D binary/sqrt and ordinary single S/D loads and
  stores; reject extra public entries, vector lanes, conversions, calls, FPCR
  writes, quick/verification/commpage modes and disabled instruction classes.
- Reload fixed full-128-bit V locations on public entry, retain them across the
  private backedge, and materialize on public/helper exits. Scalar memory
  operations remain 32/64-bit; no widened memory access or AVX512 pass-through.
- Keep NaN fixups and non-native-FPCR guards. Inherit the existing fatal guest
  fault contract; this does not add resumable guest signals.
- With `loopra2`, reject a GPR-only outer clone if it would swallow a proven
  V-resident inner loop. This is a structural heuristic, not per-program tuning
  or a new cross-join V allocator. Unsupported shapes retain existing behavior.
- Default **off**. `ATTESOR_JIT_ON` replaces defaults: append `vloop` to the full
  existing defaults, not just `ATTESOR_JIT_ON=vloop`. `ATTESOR_JIT_OFF=vloop`
  wins. Production defaults and installed binaries are not changed.
- Codegen version 69; existing build UUID and exact ON/OFF/DISABLE identity
  separate incompatible disk caches.

## Reproducible correctness checks

```sh
cargo build --release -p attesor
# In a serial test environment with loopra,vloop enabled:
ATTESOR_JIT_ON=loopra,loopra2,vloop \
  cargo test --release -p attesor vloop_ -- --test-threads=1
python3 tests/attesor/vloop-check.py target/release/attesor \
  --loopra2 --output /tmp/attesor-vloop-correctness.json
```

The checker uses independently compiled x86 reference and real ARM guest code,
float/double, 24 live V registers, high-bank loads, exact checksums, feature off,
low-bank fallback, interpreter, private AOT prewarm and two proven main-image
cache loads per configuration. These are correctness checks, **not a speed test**.
Unit tests cover all architectural X/V state, flags, aliases, NaNs, FPCR rejection,
slot capacity, public observation barriers and nested-loop selection.

The first candidate passed 166 translator tests (1 ignored), 9 core tests, and
100,000 FP-block + 62,208 NaN + 100,000 loop + 100,000 call differential cases
without mismatch. The later outer-loop-selection change has its own directed
plain/patchable on/off regression. Do not relabel old tests as a run of a newer
binary; retain binary hashes and raw logs for each stage.

## Performance protocol

Per owner instruction: one quick screening round first; repeat only substantial
changes. Compare unchanged baseline, candidate disabled/enabled, and same-host
native fixtures; retain both qsort sizes and exact outputs. No simultaneous
builds/tests. Reject detected heavy background CPU (video/renderer/compilers);
keep rejected data and record interrupted/resumed batches. Desktop scheduling,
CPU clock and memory effects remain uncontrolled; a single apparent crossing
of 60% is not acceptance.

Initial screening found substantial matmul improvement but `loopra2` swallowed
that residency. The structural precedence fix is measured separately. No current
claim that fib, both qsort sizes, sieve, matmul, N-body and branchy all reach 60%.
Remaining work must follow actual hot code, not averages or benchmark rewrites.

Three-round confirmation of the combined candidate versus unchanged baseline:
matmul median 74.902 -> 68.270 ms (-8.9% elapsed, 32.4% native); N-body
114.0 -> 90.6 ms (-20.5%, 24.3% native). N-body chiefly benefits from existing
`loopra2`, not a newly discovered Apple allocator. The 1.5M qsort median regressed
about 6%; the candidate is **not** promoted to production defaults. First-round
gains of 26–30% were optimistic and are not used as confirmed numbers. No target
is confirmed above 60%. Desktop clock/cache variability remains a limitation.

## Codegen98 / later targeted stack result

See [FUNSTACK98.md](FUNSTACK98.md). Per-basic-block raw-SP range versioning,
shared exact-width memory backend and original native tagged SLOW path improve
fib32 median18.261->13.869ms (-24.05%),41.94% native over three rounds. Only two
extra rounds followed a clear initial screen. Seven focused tests pass;
production unchanged. `funstack` joins the experimental selected profile, not
production defaults. This is not full common-config >=60% acceptance.
