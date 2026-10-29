# Register read-cache prediction and next-use eviction

The GPR and vector caches still write every guest register result to `Cpu` memory.
This change adjusts profitability and eviction only; it does not introduce dirty
state, extend a cache across a block boundary, or change helper/label rules.

Previously both banks used the same test: any of four raw instruction fields
matching a register number counted as a future use. That includes destinations,
immediate bits, and references to the other register bank. Full caches evicted
the least recently used slot even when it was needed by the next instruction.

The new predictor distinguishes X/SP and V reads/writes for common integer DP,
scalar FP, paired memory, single-register memory and register-offset accesses.
A read consumes the previous value before a same-instruction write; an overwrite
before any read means the previous value has no future use. Zero register fields
do not count as X31/SP unless that instruction uses SP. Unsupported/unknown
encodings conservatively read both banks and kill nothing. This is a partial
operand decoder for prediction, not a new architectural execution decoder.

For the existing ten-instruction window, each encoding is decoded once per guest
instruction being compiled. A compact next-use table is computed for both banks;
allocation then uses direct table lookups. Empty slots remain first choice. Full
caches evict the value with the furthest next use (or none), with LRU as the tie
breaker. `ATTESOR_JIT_OFF=nextuse` restores the previous raw-field/LRU policy for
both decisions. Existing cache-disable and scalar-alias controls remain intact.

## Correctness evidence

```sh
cargo test -p attesor jit::emit:: -- --test-threads=1
cargo build --release -p attesor
python3 tests/attesor/nextuse-check.py target/release/attesor
```

Both policies passed the full-scale regression:

- 100,000 integer block attempts: 8,893 valid, zero mismatches; invalid/unsupported
  encodings are skipped, and 1,178 of 30,638 instructions use existing fallbacks.
- 100,000 FP blocks: 100,000 matching, zero skipped, 650,599 instructions and zero
  interpreter fallbacks.
- 5,000 TBI/helper blocks: zero mismatches, 70,000 native instructions and 10,000
  deliberate helper/fallback instructions.

The final script also tests select-chain execution and a pressure workload with
nine V registers (one more than the cache capacity), destructive destinations,
scalar load/store aliases and an independent eight-value integer recurrence.
Finite outputs match an independent x86 C reference in new/old policy,
redirected-commpage and experimental host-call/return modes. The 257-round
reference checksum is `c3ec706e00000000 02dc4648006840c8`.

An exceptional-input V-pressure recurrence includes signed zero/infinity,
canonical and payload NaNs, and signalling NaNs. ARM interpreter and both JIT
policies match exactly (`82ca0ba46236e8e4 02dc4648006840c8`); no cross-architecture
NaN payload equivalence is assumed. The final expanded script was additionally
smoke-tested with `--count 1000`. Existing executing internal-join and branch
unit tests still pass; two added units directly check bank separation,
write-before-read death and dead/furthest eviction instead of oldest eviction.

[Recorded correctness output](measurements/2026-10-29-nextuse-check.txt).

## Required serial performance acceptance

This candidate has not been timed under a controlled serial load. Passing the
correctness tests does not establish a speedup or justify enabling a slower
policy. The integrator must measure compilation cost as well as hot execution:

```sh
clang -O2 -ffp-contract=off -arch arm64 tests/attesor/nextuse-pressure.c \
  -o tests/attesor/build/nextuse-pressure
python3 bench/compare-cpu.py \
  --before target/release/attesor --after target/release/attesor \
  --before-env ATTESOR_JIT_OFF=nextuse --after-env ATTESOR_JIT_OFF= \
  --before-env ATTESOR_STATS=1 --after-env ATTESOR_STATS=1 \
  --guest tests/attesor/build/nextuse-pressure --runs 7 \
  --output /tmp/attesor-nextuse-pressure.json
```

The pressure program prints separate FP/integer kernel times and a semantic
checksum. The report retains whole-process wall time and stderr; the latter's
`ms translating` and translated instruction/block counts permit compilation-cost
comparison without counting interpreter reference execution as compilation.
Repeat with the mixed native-ratio kernels and `--cold-aot`, then DSH/ARM VS Code
cold startup and warm interaction. Unknown-instruction conservatism can add
unnecessary cache copies, and prediction itself adds compiler work; the policy
must earn its default through those measurements. Cross-block allocation,
rematerialization and write-back caches remain separate work.
