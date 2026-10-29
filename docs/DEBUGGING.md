# Debugging, profiling and JIT self-tests

## Debugging a guest program with lldb (`--gdb`)

`attesor --gdb PORT <arm64-program> [args…]` stops before the first instruction (dyld's entry) and waits for
a GDB-remote debugger on `127.0.0.1:PORT`. It prints the main image's slide and the lldb line to load its
symbols:

```bash
attesor --gdb 2345 ./prog &
lldb -o 'target create ./prog' -o 'gdb-remote 2345' -o 'target modules load --file prog --slide 0x400000000'
(lldb) b main      # symbols, source lines, frame variables, expressions, finish, register write, si, Ctrl-C
```

The debugged thread runs in the interpreter (`--gdb` implies `--interp`): breakpoints are a pc check before
each instruction, so guest code is never patched, and single-stepping is exact. Only the program's first
thread is debugged; threads it creates run on their own. Not supported: watchpoints, shared-library
discovery (load other images' symbols with `target modules load` by hand), attaching to a running process.
`ATTESOR_GDB_LOG=1` prints the protocol traffic. For an inherited connected socket,
use `--gdb-fd FD` instead of `--gdb PORT`; the FD is owned by Attesor. The socketpair
regressions work without a localhost listener:
`python3 tests/attesor/gdb-protocol.py target/release/attesor` and
`python3 tests/attesor/gdb-lldb-fd.py target/release/attesor`.
The original TCP regression is `tests/attesor/gdbstub.sh`.
See [DEBUGGER-VALIDATION.md](DEBUGGER-VALIDATION.md) for verified coverage and limits.

## Debugging / profiling toolbox

Flags & env: `--interp` (no JIT), `--no-aot`, `--trace` (syscalls/Mach msgs/paths), `--trace-file`
(per-process `/tmp/attesor-trace-<pid>.log`), `--dump-on-fault` (`ATTESOR_DUMP_ON_FAULT=1`,
guest regs + disasm to `/tmp/attesor-fault-<pid>.log`), `ATTESOR_STATS=1`,
`ATTESOR_FALLBACK_HIST=1` (histogram of instructions falling back to the interpreter, written
to `/tmp/attesor-hist-<pid>.txt`, digit-normalised so exact forms show),
`ATTESOR_JIT_THRESHOLD`, `ATTESOR_JIT_DISABLE=dpimm,branch,ldst,dpreg,simd,…`,
`ATTESOR_SPIN_DEBUG=1` (reports a dispatcher spinning on one pc), `ATTESOR_AOT_SYNC`,
`ATTESOR_NO_AOT_BUILD`, `ATTESOR_CACHE`, `ATTESOR_SYSROOT`, `ATTESOR_JIT_OFF=fcvt,fpelem,fmla,fmadd,ext,qdmulh,abd,addlp,3diff,hn`
(route single SIMD translation groups to the interpreter, for bisecting a bad translation),
`ATTESOR_STDERR_LOG=1` (copy each guest's stderr to `~/Library/Logs/Attesor/stderr/<name>-<pid>.log`, capped at 4 MB:
for helpers whose stderr a parent pipes away, e.g. Electron utility/host processes).
Signals: `kill -USR1 <pid>` all guest threads (pc/regs/backtrace), `kill -USR2 <pid>` lock-free
register dump of the interrupted thread + JIT counters (works in forked children).
`attesor --jit-dump <hex insns…>` prints generated x86 hex for a block (disassemble via a `.byte`
file + `clang -c` + `objdump -d --x86-asm-syntax=intel`). `ipsw dyld a2s/disass` (on the test Mac) maps
shared-cache addresses to symbols (unslid vaddr = `0x180000000 + (pc - 0x600000000)`).
`sample <pid> 8 -file f` for host-side profiles.

**Differential JIT self-test** (the important safety net):
```bash
ATTESOR_SELFTEST_MEM=1 ATTESOR_SELFTEST_SIMDMEM=1 \
  attesor --selftest-jit MASK:VALUE 8000      # hex mask/value of the encoding class
# also classes: dpimm dpreg ldst branch
attesor --selftest-jit fuse   200000   # (flag setter, b.cond | csel) pairs: compare+branch/select fusion
attesor --selftest-jit fuse2  200000   # [setter, b.cond, A, B, svc] with A/B overwriting all flags, run to the svc:
                                        #   exercises dead-flag-store elimination (prints dead_flag_skips)
attesor --selftest-jit blocks 300000   # random straight-line blocks over a small register set (reuse!) incl.
                                        #   loads/stores/pre/post-index writeback: exercises the register cache
attesor --selftest-jit selchain 200000 # flag setter feeding 1-3 selects, then a writer or a fused b.cond:
                                        #   host flags kept across selects, NZCV stores skipped
attesor --selftest-jit fpblocks 100000 # random scalar/vector FP + SIMD&FP load/store blocks over v0-v5:
                                        #   exercises the V-register cache (xmm8-15); ATTESOR_SELFTEST_LEN=1 for
                                        #   single instructions. Expected: only NaN-sign mismatches (a propagated
                                        #   NaN equal to the x86 indefinite pattern is turned into the default NaN)
```
`ATTESOR_JIT_OFF=scalaralias` disables reuse of pre-write cached scalar FP inputs.
`ATTESOR_JIT_OFF=vcache` disables the V-register cache, `selflags` the select-chain flag elimination
(`ATTESOR_JIT_ON=callret` enables the experimental host call/ret translation of BL/BLR/RET, measured slower).
`ldst` centres pc in a 4MB window so the `ldr <v>, literal` pool (±1MB) stays mapped: expect `mismatch=0`.
(Before, the 4K test page left the literal pool outside it: garbage reads showed up as ~13 mismatches on some
hosts and faulted outright on hosts with sparse mappings around the test page.) Sensitivity check of `blocks`: removing one `rc_inval` gives ~60 mismatches
per 200k blocks. All three new classes also pass on the test Mac's real Haswell.
Prints `ok= mismatch= skipped= native=`. **Always check `native=`**: an instruction that falls
back to the interpreter trivially matches it (that hid four bugs for a while). Masks must leave
the U bit (29) and Q bit (30) free when testing both variants. Known accepted mismatch: the sNaN
case above.

## Guest sampling profiler

Set `ATTESOR_PROFILE=/path/to/run` to sample all guest threads at approximately 1 kHz.
Each process writes `<name>-<pid>.txt` on exit, before exec, and every 10 seconds;
use a fresh directory for each run. Snapshots are cumulative and atomically replaced.
Children inherit the setting. Merge the run with `python3 tools/profile-report.py /path/to/run`;
the merger also groups Electron/Chromium processes by name, `--type` and utility subtype.

Reports include thread-time shares for JIT, interpreter, translation, BSD/Mach syscalls,
Metal bridge calls, waits (`idle`), and dispatcher overhead, plus translation totals.
Image and symbol tables count only JIT/interpreter samples; image percentages use that
execution total, and each image shows its JIT/interpreter split. The full aggregate tables
are retained below the top 40 images / top 60 symbols for lossless merging.

Sampling reads the atomic guest PC and a state word updated at Rust transitions, without
stopping or signalling guest threads or modifying generated code. The JIT publishes PC
at its existing block/exit points, so attribution within a chained block remains approximate.
Symbol lookup uses Mach-O symbols and shared-cache exports; stripped/internal routines may
appear as a nearest exported symbol or image offset. `idle` classifies waiting syscall
entries, including their setup and return time; it is sampled thread time, not CPU usage.
Force-killed processes retain the last 10-second snapshot (shorter runs need a normal exit).
