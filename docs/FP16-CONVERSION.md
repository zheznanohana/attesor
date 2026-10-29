# FPCR-sensitive half conversion compatibility

The vector single-to-half FCVTN/FCVTN2 JIT previously hard-coded nearest-even
rounding, while the interpreter's integer half converter also ignored FPCR.
FCVTN rounding is controlled by FPCR; this is specified in Arm's
[Instruction Set Reference Guide, D6.52](https://documentation-service.arm.com/static/6245c734b059dc5ff9a8bdab).
The JIT now selects MXCSR rounding (already synchronized with FPCR), without
adding instructions to the normal conversion path.

The interpreter's scalar single/double-to-half and vector single-to-half
conversion helpers now honor all four rounding modes, source flush-to-zero,
default NaN, and alternative half precision. Conversion instructions ignore
FZ16, as required by Arm's FPConvert pseudocode in the
[Arm-authored architecture supplement, shared FPConvert](https://kib.kiev.ua/x86docs/ARM/ARMARMv8/DDI0600A_d_armv8_r64_supplement.pdf).
AHP conversions map NaNs to signed zero and infinities to signed maximum
alternative-half values. Ordinary IEEE conversions preserve the quieted NaN
payload unless DN requests the default NaN.

Non-default DN/AHP/FZ16 blocks continue to use the existing interpreter fallback;
no unsafe widening of the JIT's FPCR support mask was made. Result-bit correctness
is tested here. Complete FPSR exception accumulation, exception traps, general
half arithmetic, and widening conversions under every control mode are outside
this change's claims.

## Reproduce

```sh
cargo test -p attesor-core half_conversion_tests
cargo build --release -p attesor
python3 tests/attesor/fp16-convert-check.py target/release/attesor
```

The two unit tests include explicit single/double halfway, overflow and tiny
positive/negative values, special control bits, and all IEEE half encodings except
NaNs round-tripped through single precision in all four rounding modes.

The end-to-end probe compares full result bits with an independent integer C
reference. It exercises scalar FCVT, vector FCVTN/FCVTN2, destructive register
aliases, upper-half clearing/preservation, signed zero, NaN payloads, signalling
NaNs, infinities, finite overflow, subnormals, ties, and random inputs. Four
rounding modes times five flag configurations (normal, FZ, FZ16, DN, AHP) produce
60,000 operations. Four additional hand-computed checks explicitly verify the
smallest positive/negative FP32 subnormal under round-toward-positive/negative:
FZ clear produces a half subnormal only in the selected direction, while FZ set
produces signed zero. Both JIT and forced interpreter match the reference:

```
fp16-convert PASS cases=60000 digest=40c315811183ddde
```

The fixed-width differential class separately tests 20,000 encodings each at
default FPCR and rotating rounding/FZ settings. Both report 20,000 matching,
zero mismatches/skips and 20,000 native translations. This confirms that the
FPCR rounding test is not passing solely through interpreter fallback.
[Recorded output](measurements/2026-10-29-fp16-convert.txt).

The original binary fails the probe on DN negative NaNs. Restricting the probe to
ordinary control bits with `--round-only` also fails immediately after switching
to round-toward-positive: negative overflow wrongly becomes infinity instead of
the largest finite half. This establishes defects independently of the changed
interpreter.

## VideoToolbox verification limit

During this round, the existing `vtcodec.m` probe was compiled for both native
x86-64 and arm64. Both could not create H.264 or HEVC compression sessions and
returned the same status `-12908`. Consequently this environment does not prove
or disprove hardware video compatibility. Earlier successful round-trip records
remain separate evidence from another execution environment; this round does
not replace them with a hardware-acceleration claim. No VideoToolbox bridge was
introduced based on an identical native/guest failure.
