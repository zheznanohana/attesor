# Direct compare/test-and-branch lowering

CBZ/CBNZ and TBZ/TBNZ previously materialized their condition with `SETcc al`,
then immediately used `TEST al,al` and another conditional jump. They now consume
the flags from CMP or BT directly before entering the existing `goto` paths.
No new block-link mechanism is introduced: internal labels and dispatcher/chaining
stubs are the same ones used by the prior translator.

The not-taken conditions are JE/JNE for CBNZ/CBZ and JAE/JB for TBNZ/TBZ.
Guest NZCV remains in the Cpu and is unchanged; only host scratch flags differ.
Architectural XZR/WZR inputs directly select the constant successor, independent
of `Cpu::x[31]`, which stores SP in other instruction classes.

`ATTESOR_JIT_OFF=directbranch` selects the old SETcc/TEST path for controlled
same-binary comparisons. The AOT compatibility key already includes JIT switches;
the integrator should also advance the translator version when combining this
output change with other work.

## Correctness evidence

```sh
cargo build --release
cargo test -p attesor direct_branch_tests -- --test-threads=1
ATTESOR_JIT_OFF=directbranch cargo test -p attesor direct_branch_tests -- --test-threads=1
python3 tests/attesor/direct-branch-check.py target/release/attesor
```

The two unit tests actually execute generated code. They cover W/X compares,
all 64 test-bit positions, zero-register versus SP storage, all 16 NZCV values,
minimum/maximum signed offsets, backward/self/fallthrough targets and both edges
to internal successor labels. Registers and guest NZCV are checked exactly. In
the default mode a code-shape assertion additionally rejects SETcc and TEST in
these emitted branch units. Both default and baseline unit runs passed.

The standalone ARM probe checks 196,608 branch decisions and exact NZCV values
against portable integer predicates compiled natively for x86. It includes
high-half-only X values that must compare zero in W form. Both translator modes
print the native reference checksum `581343f670cdf383`. The generic branch
self-test sampled 20,000 encodings in each mode: 10,625 valid/native cases,
zero mismatches and 9,375 skipped encodings. This count is not a claim that every
random encoding is valid or that every instruction family is completely covered.

The small 4,097-iteration throughput-workload smoke test also matches the native
reference in both modes: `02146e960ebb2a24 2ed0b1c930fae8a4`.
[Probe results](measurements/2026-10-29-direct-branch-check.txt) and
[unit results](measurements/2026-10-29-direct-branch-units.txt) retain the output.

## Serial performance acceptance

No speedup number was measured during parallel development. Compile the probe
and compare the same guest and translator with the baseline switch off/on:

```sh
clang -arch arm64 -O2 tests/attesor/direct-branch.c -o /tmp/direct-branch-arm
clang -arch x86_64 -O2 tests/attesor/direct-branch.c -o /tmp/direct-branch-x86
python3 bench/compare-cpu.py --before target/release/attesor \
  --after target/release/attesor --guest /tmp/direct-branch-arm \
  --before-env ATTESOR_JIT_OFF=directbranch --after-env ATTESOR_JIT_OFF= \
  --guest-arg=--bench --guest-arg=20000000 --runs 7 \
  --output /tmp/direct-branch-comparison.json
/tmp/direct-branch-x86 --bench 20000000
```

Compare both checksum words with the independent native reference. The two
kernels include the loop's existing arithmetic and branch/chaining overhead;
they do not measure pure hardware branch latency or general application speed.
Real DSH/VS Code acceptance remains the integrator's responsibility. This change
has no application identity tests, GPU bridge changes or application patches.
