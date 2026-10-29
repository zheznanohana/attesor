# Crypto extension validation

SHA256SU0 and SHA256SU1 now use Intel SHA256MSG1/MSG2 when SHA-NI is
available. MSG1 matches SU0 directly. For SU1, the JIT first adds the
aligned n1,n2,n3,m0 vector to d, then MSG2 computes the sigma1 terms and
the dependency from the first two output words to the last two. All inputs
are loaded before any destination write, including Rd=Rn, Rd=Rm and all-equal.

The existing scalar SHA-256 lowering is retained for CPUs without SHA-NI
and for `ATTESOR_JIT_OFF=shani`. AES-NI and PCLMUL paths retain their
runtime CPU feature guards. The AOT cache must include AES, PCLMUL and SHA
CPU feature bits in its compatibility key (handled separately in the AOT work).
No SHA-1/SHA-512/SHA-3 coverage is claimed by this change.

## Repeatable correctness checks

```
cargo build --release
ATTESOR=$PWD/target/release/attesor tests/attesor/crypto-check.sh
```

The suite requires AES-NI and PCLMUL on the host to meet its native-path
coverage gate. It samples 12,000 encodings each for AES, PMULL/PMULL2,
SHA256H/H2, SHA256SU0 and SHA256SU1, twice: default and forced non-SHA-NI
lowering. On this Intel host every one of the 120,000 cases matched the
interpreter and used a native JIT translation (zero fallbacks and skips).
The register encodings are randomized, including operand overlap.
Raw output is in `measurements/2026-10-29-crypto-check.txt`.

The independent known-answer program additionally checks FIPS AES-128
ECB encryption/decryption, SHA-256 of `abc`, and streaming SHA-256 of one
million `a` characters. Native x86, translated ARM and forced non-SHA-NI
translated ARM all print `crypto-kat: PASS`.

`sha-schedule.c` is a dedicated schedule workload, independent of
CommonCrypto dispatch. Its x86 implementation is portable scalar C; its ARM
implementation executes SU0/SU1 intrinsics. The 17-iteration semantic smoke
check produces `checksum 05e2e1ad19256390` in all three modes.

## Serial performance acceptance

Compile the schedule workload as shown in its source (the correctness script
also builds it). The default one-million-iteration run prints `sha-sched`
milliseconds plus a checksum. Use `bench/compare-cpu.py` with this guest and
baseline/candidate binaries, with other CPU workloads stopped. Check the
checksum against `sha-schedule-x86` as well as comparing timings. No performance
number was collected during parallel agent development; all performance claims
must come from the integrator's serial run. An isolated schedule speedup does
not establish an HTTPS or application speedup.
