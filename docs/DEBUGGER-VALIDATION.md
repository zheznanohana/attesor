# Debugger protocol and LLDB validation

The debugger now accepts `--gdb-fd FD`, taking ownership of an inherited stream
socket, in addition to the existing `--gdb PORT` localhost listener. A launcher
can create a Unix socketpair and pass one end to Attesor and the other to LLDB's
`process connect --plugin gdb-remote fd://FD`. This avoids a TCP listener and is
also useful when the execution environment forbids binding localhost. All
threads still use interpreter execution in either debug mode; the protocol
exposes only the initial guest thread.

The transport now verifies incoming packet checksums, negatively acknowledges
bad packets in ack mode, and retransmits its last response on NAK. Short or
invalid-hex register writes are rejected before any register changes; a malformed
whole-register-file write cannot leave partial changes. Memory writes must match
their declared byte count and contain valid hex. Malformed continue commands and
unaligned/wrong-sized AArch64 breakpoints are rejected rather than executed.
Ctrl-C produces the SIGINT stop reason (`T02`), not a fabricated SIGTRAP; subsequent stop queries retain that reason. The stub
advertises only implemented `vCont` actions (`c` and `s`). Target-XML read ranges
use saturating arithmetic, avoiding overflow for malformed offsets/lengths.

LLDB expression allocations (`_M`) now honor requested memory permissions and
are tracked by address and length. `_m` actually unmaps the recorded allocation
and invalidates translated code, instead of acknowledging while leaking memory. Uploading expression code into a debugger-owned RX allocation temporarily adds write permission and restores RX afterward.
Unknown/double frees fail. Individual allocations are limited to 16 MiB; other
addresses cannot be freed through this packet. Allocations deliberately remain
mapped unless explicitly freed or the process exits, so detach does not invalidate
memory a debugger intentionally left with the guest.

## Repeatable acceptance

Build and run:

```sh
cargo build --release -p attesor
python3 tests/attesor/gdb-protocol.py target/release/attesor
python3 tests/attesor/gdb-lldb-fd.py target/release/attesor
```

The protocol script starts real ARM guests and exchanges framed RSP packets
through inherited sockets. It covers register/memory access and invalid writes,
checksum and NAK behavior, no-ack negotiation, target-XML ranges, allocation/free,
breakpoint insertion/removal, single-step, continue, Ctrl-C, exit status, detach,
and debugger disconnect. Each process is owned by the test and bounded by socket
and process timeouts, with cleanup confined to its own process group.

The LLDB script uses the real LLDB binary with ARM symbols and DWARF. It checks
symbol breakpoint arguments, both a local expression and a call into a guest function (`add3(1, 2, 3) == 14`), a register modification, finishing
the current call, and the resulting program output and exit status. It retains
the substance of `gdbstub.sh` while using a transport that this environment permits.

The original TCP `tests/attesor/gdbstub.sh` was attempted here and failed because
the execution sandbox denied `bind(127.0.0.1)` with `Operation not permitted`.
That is not reported as a passing TCP acceptance test. Both transports use the
same framing, command dispatch and execution code; TCP listener setup itself
still needs validation in an environment that allows it.

No claim is made for watchpoints, attachment to an existing process, shared
library discovery, multi-thread debug control, or full signal delivery. Symbols
are loaded explicitly using the main executable slide printed by Attesor.

Final run on 2026-10-29: all 49 protocol assertions passed. Real LLDB returned
`10` for `a + b` and `14` for the injected call `add3(1, 2, 3)`, then finished
the register-modified call with `310`; the program printed `sum 1600` and exited
with status 7. Raw outputs are retained in
[the RSP report](measurements/2026-10-29-debugger-rsp.txt) and
[the LLDB report](measurements/2026-10-29-debugger-lldb.txt).
