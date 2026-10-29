# JIT floating-point NaN correction

This change fixes result bits for scalar add/subtract/multiply/divide/square
root and scalar/vector/by-element fused multiply-add. It also fixes vector
binary arithmetic and by-element multiplication. No correctness tolerance was
added to the self-tests.

## Root causes

1. Converting any x86 negative canonical NaN to ARM's positive default NaN
   overwrote an identical-looking NaN propagated from an input. The input's
   sign and payload must be retained.
2. The FMA slow path identified infinity-times-zero by testing whether an
   unfused product's bits matched x86's indefinite NaN. A propagated NaN can
   have those same bits, so the test also needs to exclude NaN multiplicands.
3. FMA and binary arithmetic did not consistently implement signalling-NaN
   priority. Signalling operands precede every quiet operand, with each group
   following ARM's operand order (addend, first multiplicand, second
   multiplicand for FMA).

The fixes overlay quieted input NaNs in reverse priority order and keep the
FMA infinity-times-zero/quiet-addend special case separate. They run only on
NaN-result paths; ordinary scalar and vector binary arithmetic retains its
existing fast result check. FMA now writes its final result register directly
with VFMADD213 and uses a single branch over the slow path, eliminating one
result copy and one unconditional branch from the former fast path.

## Reproduction

Build with `cargo build --release`, then run:

```sh
target/release/attesor --selftest-jit fpblocks 100000
target/release/attesor --selftest-jit fpnan 62208
ATTESOR_SELFTEST_FPCR=1 target/release/attesor --selftest-jit fpnan 62208
ATTESOR_SELFTEST_FPCR=1 target/release/attesor --selftest-jit ff200c00:1e200800 20000
ATTESOR_SELFTEST_FPCR=1 target/release/attesor --selftest-jit ff000000:1f000000 20000
```

The new `fpnan` boundary matrix includes 12 precise input bit patterns:
signed zero, signed one, signed infinity, signed canonical quiet NaN, signed
quiet NaN with a payload, and signed signalling NaN with a payload. It spans
18 scalar/vector/by-element opcode kinds and both precisions, including
destructive destinations, mixed exceptional/ordinary vector lanes, invalid
operations and non-default rounding/flush-to-zero settings when requested.
The count 62,208 traverses its deterministic input/opcode schedule once; it
is a targeted matrix rather than a proof of every instruction encoding.

Before the fix, the deterministic 100,000-block FP test reported 622 failures.
The final version reports
100,000 matching blocks, zero mismatches/skips, 650,599 executed guest
instructions and zero interpreter fallback instructions. Its FP NaN matrix
matched in all 62,208 cases with zero skips and 62,208 native translations,
both at default FPCR and with rotating FPCR modes. Random FPCR instruction
classes also matched: binary arithmetic 8,425 valid / 4,873 native and scalar
FMA 14,966 valid / 9,964 native out of 20,000 attempts each; invalid or
unsupported encodings account for the remaining attempts.

The final direct-result FMA variant passed both complete NaN matrices and the
100,000-block regression with the same results. Performance must be measured
serially after integration; no
speedup figure is claimed from concurrent agent work. This work does not
claim full FPSR exception-status coverage or general architectural completion.
