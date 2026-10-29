# Half conversion exception status

Half conversions previously returned correct result bits but did not accumulate
floating-point exception bits in guest FPSR. This change handles the masked
conversion exceptions IOC, OFC, UFC, IXC and IDC for scalar FP32/FP64-to-half
(interpreter) and vector FP32-to-half FCVTN/FCVTN2 (JIT and interpreter).

The integer converter now returns result bits and cumulative flags. It records
signalling-NaN invalid operations, IEEE overflow plus inexact, alternative-half
invalid overflow without inexact, and tininess **before rounding** when inexact.
Exact subnormal half results do not raise UFC. A flushed subnormal source raises
IDC without also raising UFC/IXC. Existing DN/AHP/FZ/FZ16 result semantics are
preserved. These rules follow the Arm-authored
[FPConvert/FPRoundCV/FPRoundBase pseudocode](https://kib.kiev.ua/x86docs/ARM/ARMARMv8/DDI0600A_d_armv8_r64_supplement.pdf),
sections I1-496 and I1-509–511.

The F16C JIT path classifies original FP32 inputs and widened half results with
SIMD operations and ORs the resulting bits into the guest's FPSR. It does not
import host MXCSR exception bits, which can be left behind by native libraries,
earlier operations, or even the classification comparisons. Guest writes to
FPSR therefore clear guest flags independently of host sticky state. Non-default
DN/AHP/FZ16 blocks retain the existing interpreter guard. No new conversion
fallback/helper call is introduced for the normal native path.

LLVM can turn integer-looking zero tests on float bitcasts into floating-point
comparisons. With host DAZ enabled, that loses the distinction between a source
subnormal and zero and would omit IDC. The interpreter classification retains
an opaque integer value before testing its bit fields; a dedicated poisoned-host
unit test covers this case.

## Verification

```sh
cargo test -p attesor-core half_conversion_tests
cargo build --release -p attesor
python3 tests/attesor/fp16-status-check.py target/release/attesor
python3 tests/attesor/fp16-convert-check.py target/release/attesor
```

Four unit tests cover directed result boundaries, all finite half round trips,
exception-status boundaries, and host MXCSR flags/DAZ contamination. The status
probe uses a separate integer C oracle and directly reads guest FPSR after each
conversion; its 60,000 cases include four rounding modes, ordinary/FZ/FZ16/DN/AHP
settings, source/destination aliases, QC/DZC preservation, NaN signs and payloads,
finite overflow, exact/inexact subnormal outputs and values rounded up from tiny
to minimum normal. Both JIT and interpreter match all result bits and flags:

```
fp16-status PASS cases=60000 digest=725b1a2cd2c9de4f clear-isolation=PASS double-boundaries=PASS
```

Additional explicit double-source tables include a smallest-double subnormal,
which must not be rounded through FP32, and signalling NaNs. The probe repeatedly
runs an exceptional conversion, clears guest FPSR, and verifies a subsequent exact
conversion reports zero. The default and rotating-FPCR FCVTN differential classes
also execute 20,000 encodings each with zero mismatches/skips and 20,000 native
translations. The generic differential snapshot omits FPSR; the direct probe,
not that snapshot, is the status correctness evidence.

`fp16-status.c --bench 1000000` provides a separate serial timing workload. It
prints result and status checksums separately: old builds can have identical
results but incorrect status. Status classification adds instructions per native
conversion, so compatibility has a measurable cost that must be reported by the
integrator. No speedup is claimed here, and no timing was collected concurrently
with the other agents.

This is not complete FPSR support. Other floating-point arithmetic, widening
conversions, integer-to-half conversions and enabled synchronous FP exception
traps remain outside this change. The change must not be used to claim full
IEEE exception compatibility.
