# Conservative NZCV liveness boundaries

The translator already fuses arithmetic/test setters with following conditional
branches and CSEL chains. It can omit NZCV stores when both branch successors,
or the code after the selects, overwrite those flags before another reader.
This is more than an unoptimized store-after-every-instruction baseline.

The previous `flags_dead_at` scanner treated every unrecognized non-terminator
as transparent. For example, `CMP; CSEL; undefined; CMP` omitted the first CMP's
NZCV stores even though the interpreter helper exited at the undefined
instruction. The pending exception then saw the old flags. Loads and stores
also cannot be assumed to reach a later writer.

The scan now stops at every instruction except a small whitelist of non-faulting
pure operations (NOP, ADR/ADRP, ADD/SUB immediate without S, and valid move-wide),
and the existing bounded direct-B traversal. Flag-killing encodings are checked
for legal widths, shifts, and immediate encoding; reserved instructions do not
prove a kill. FCMP is conservatively not used as a kill in this scan. A CSEL
chain routed through the interpreter by the debug dpreg switch cannot consume
host flags and therefore cannot omit the architectural stores.

The executable regression places an observing, early-exit interpreter helper
between a fused CSEL and a later flag writer. Five independent integer
subtraction cases cover zero, borrow, negative results, and signed overflow.
With the old scan restored, it fails immediately (NZCV 7 versus expected 8 for
0-1); with the conservative scan, the helper observes the correct flags.
Static boundary cases also cover ADC/SBC, conditional readers, MRS, memory,
reserved encodings, SVC, ISB, RET, and unreadable ends.

This fixes correctness and is suitable for the default path. No performance
claim is made. Broader dead-store/lazy-flags work needs a validated instruction
side-effect model and exception boundaries first. Host signal delivery to guest
handlers is not implemented by the existing runtime, so the tests do not claim
coverage of guest signal-handler state.

Reproduce:

```sh
cargo test -p attesor -- --test-threads=1
ATTESOR_JIT_DISABLE=dpreg cargo test -p attesor helper_exit_observes_preceding_cmp -- --test-threads=1
```

Any integration of this code-generation change must increment the AOT translator
version so old cached code cannot retain the incorrect liveness decision.

## Non-faulting instructions (follow-up)

The whitelist above also stopped at ordinary integer instructions (EOR, MUL, AND, shifts, LDRB...),
which put four NZCV stores back into most loops: int-hash +35%, branchy +15% against the previous
release. `flags_safe_passthrough` now accepts every valid encoding of the non-faulting integer
data-processing classes (logical/add/sub immediate and register without S, bitfield, EXTR, 2- and
1-source, MADD/MSUB/MULH family). Loads and stores pass too while `GUEST_FAULTS_FATAL` holds: guest
faults end the program, so stale NZCV is only visible in the crash dump. Reserved encodings, SVC,
ISB, flag readers and the debug-disabled CSEL path remain boundaries. int-hash 525 -> 396 ms,
branchy 1287 -> 1123 ms (interleaved A/B, same load).
