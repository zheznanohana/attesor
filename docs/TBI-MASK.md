# Reusing the top-byte-ignore address mask

The memory emitter previously stripped bits 63:56 with dependent `shl rax, 8`
and `shr rax, 8` operations before every memory access. For an in-place commpage,
it now keeps `0x00ffffffffffffff` in r11 and uses one dependent `and rax, r11`.
The mask is loaded lazily at the first eligible access of each basic block.
No ISA feature beyond the existing x86-64 baseline is needed.

For a four-load block, inspection with `--jit-dump` shows one mask load and four
ANDs in place of eight shifts. The mask portion is 22 bytes versus 32 bytes in
that example. A single-access block has a larger mask portion (13 versus 8
bytes), so whole-workload timing is still required; static instruction savings
are not a general speedup claim.

## Correctness boundaries

- The operation retains exactly bits 55:0, matching the old logical shifts.
  Guest GPR/vector writes remain write-through to `Cpu`; faults, signals and
  helpers observe the same authoritative state as before this change.
- r11 was otherwise only used as the large-immediate scratch in `st_imm`.
  `mov_imm(r11, ...)` now invalidates mask knowledge. Any emitted call also
  invalidates it, and the interpreter-fallback path clears it explicitly.
- Every basic block begins with invalid mask knowledge, including chained and
  in-unit entries. No host register value is assumed across block boundaries.
- A mask loaded after an internal label is not recorded as available, using
  the existing `rc_frozen` conditional-emission rule. A skipped path therefore
  cannot establish a false invariant at a join. A previously valid mask remains
  usable until a possible clobber; even a conditional scratch clobber drops
  compile-time knowledge conservatively.
- When commpage redirection checks are needed, the existing shifts and helper
  path remain unchanged. This avoids introducing a caller-saved-register
  invariant across their conditional host call.
- `ATTESOR_JIT_OFF=tbimask` restores the original shifts. The AOT code-generation
  key already includes JIT OFF settings; integration must also bump the
  translator version for the new default output.

This does not implement cross-block register allocation, dirty-register
writeback caching or a different invalidation scheme.

## Repeatable verification

```sh
cargo build --release
cargo test -p attesor tbi_mask_tests -- --test-threads=1
python3 tests/attesor/tbi-mask-check.py target/release/attesor
target/release/attesor --selftest-jit blocks 100000
target/release/attesor --selftest-jit fpblocks 100000
target/release/attesor --selftest-jit fpnan 62208
ATTESOR_SELFTEST_MEM=1 ATTESOR_SELFTEST_SIMDMEM=1 \
  target/release/attesor --selftest-jit ldst 20000
```

The new `tbiblocks` test compares a 16-instruction block against the interpreter
with randomized values, all 256 top-byte tags, tagged SP, scalar/pair/vector
accesses, a large MOVZ scratch write and division's internal control flow. Its
fallback wrapper deliberately overwrites caller-saved r11 after executing the
instruction. In this source revision the half FABS and a non-post-index LD1
exercise that helper: 20,000 blocks match, with 280,000 native instructions and
40,000 intentional fallback instructions, both with mask reuse and old shifts.
The test requires actual helper coverage and at least 14 native instructions
per block; it does not count an all-interpreted run as a pass.

An executing unit test additionally poisons r11 and tests both branches of an
internal join: a conditional first mask load, and a conditional scratch overwrite
of an earlier mask. The pointer-walk probe matches an independent native x86
reference in default, old-shift, redirected-commpage and experimental host
call/return modes. Its short correctness run prints
`checksum 89b4bc1484a05646`.

The existing regressions passed: integer blocks 8,893 valid / zero mismatches
from 100,000 attempts, FP blocks 100,000 / zero mismatches / zero fallbacks,
NaN boundaries 62,208 / zero mismatches / all native, and memory class 10,636
valid / zero mismatches / 8,802 native from 20,000 attempts. Invalid encodings
account for skipped samples; no tolerance was added.

## Serial performance acceptance

`tests/attesor/tbi-chain.c` is a dependent pointer-load workload with real ARM
top-byte tags. The x86 reference removes tags explicitly. It checks the endpoint
and address-independent node value; its default workload times 10 million hops
through a 16 KiB ring. A short `4097`-hop run is used only for correctness.

```sh
clang -O2 -arch arm64 tests/attesor/tbi-chain.c -o tests/attesor/build/tbi-chain
python3 bench/compare-cpu.py \
  --before target/release/attesor --after target/release/attesor \
  --before-env ATTESOR_JIT_OFF=tbimask --after-env ATTESOR_JIT_OFF= \
  --guest tests/attesor/build/tbi-chain --runs 7 --output /tmp/tbi-performance.json
```

The integrator should also compare the mixed CPU kernels and real applications,
serially and without concurrent correctness jobs. No long-workload performance
measurement was taken during parallel development.
