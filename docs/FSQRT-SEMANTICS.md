# FSQRT NaN semantics

FSQRT classifies input NaNs before its invalid negative domain. DN=0 quiets a
signaling NaN while retaining its sign and payload, and preserves quiet NaNs.
DN=1 returns the positive default NaN for either NaN kind. A negative nonzero
input, including negative infinity, produces the default NaN irrespective of DN.
Signed zero survives unchanged, and positive infinity survives unchanged.

These rules follow Arm's [FPCR definition](https://documentation-service.arm.com/static/5e7dc2e199dba00e4b734ac4)
and its [C language extension description of square root](https://arm-software.github.io/acle/main/acle.html#floating-point-data-processing-intrinsics).
The architectural FPSqrt pseudocode first calls FPProcessNaN for a NaN input,
then handles zeros and positive infinity, then the invalid negative domain.

The shared `fsqrt_dn` helper implements the result bits for every interpreter
FSQRT dispatch: scalar H/S/D, vector 4H/8H, vector 2S/4S, and vector 2D.
Preclassifying the negative domain also prevents a development-build optimizer
from losing the post-sqrt NaN canonicalization: the former opt-level=1 scalar
path emitted `sqrtss`/`sqrtsd` directly into the result store and leaked x86's
negative indefinite NaN. The classification has the same meaning at every
optimization level; it is not a build-profile special case.

The JIT's existing block FPCR guard continues to send DN mode to the interpreter
before executing native FP code. MSR FPCR now ends its block and returns through
the dispatcher; its continuation is retained by AOT discovery. Consequently a
mode written inside a block cannot bypass the successor guard, including by a
private region edge or a patched chain. This boundary applies to every FP
instruction protected by that guard, not just square root. Directed tests verify both
DN modes against independent bit tables through every interpreter form, and
verify that the JIT DN guard leaves vectors untouched before interpreting the
instruction. Source/destination aliases and upper-lane clearing are covered.

```
cargo test -p attesor-core
cargo test -p attesor
python3 tests/attesor/fpcr-mode-check.py --attesor target/debug/attesor
```

The standalone gate compiles the real guest MSR FPCR/FSQRT fixture and checks
the original 20 cases in interpreter, JIT, region JIT and region AOT modes. Its
expected checksum is fixed independently of the interpreter. It clears inherited
`ATTESOR_*` settings, uses a private temporary cache with background builders
disabled, explicitly prewarms the executable and requires nonzero main-image
loaded diagnostics for the final AOT process.

## FPCR scope audit

This change reads DN and never modifies FPCR. Existing RMode/FZ synchronization
through the runtime's host FP mode remains unchanged. AHP handling for precision
conversions remains unchanged. Half FSQRT still uses its existing widening and
rounding path; this change does not implement the currently missing general
FZ16 handling there. General FPSR accumulation (including FSQRT invalid-operation
status), enabled FP exception traps, and non-nearest half arithmetic rounding are
not completed by this result-bit fix. Those are separate existing interpreter
semantics gaps, not guarantees made by this change.
