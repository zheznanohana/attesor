# Task brief: stage 3 — dead V-register stores (A3) and cheap host call/return (B)

Reviewer: Claude. Implementer: Codex. Work only in `~/attesor-s3` (branch `stage3`). Never touch
`~/attesor`, `~/bin`, `~/attesor-launcher`, user apps, caches or processes you did not start. If git
commits are blocked, leave the work in the tree and write one message per part to
`target/stage3-A3-message.txt` and `target/stage3-B-message.txt`, plus `target/stage3-A3.patch` (A3 alone,
applies to the starting commit) so Claude can commit the parts separately. Do not push.

Context: `crates/attesor/src/jit/emit.rs`. Already in place: V-register cache in xmm8-15 (`VCache`,
`vsrc`/`vfill`, results always also stored to `Cpu`), FP slow paths out of line (cold paths, constant pool),
hot-loop GPR register allocation with write-back (`LoopRa`, regions; docs/tasks/LOOPRA.md) on by default,
flag-liveness rules (`flags_dead_at`, `GUEST_FAULTS_FATAL`). Measured (vs native x86): fp-nbody 17%,
matmul 23%, fib (recursion) 19-21%, qsort 44-50%.

## A3 — dead V-register stores within a block
- A scalar or vector FP/SIMD result whose register is completely overwritten later in the same block,
  before any exit, helper/fallback call, cold path that reads it from memory, or instruction that reads
  it from `Cpu` directly, does not need its store to `Cpu`: keep it only in the V cache slot.
- Decide with a forward scan of the block's remaining instructions (like `flags_dead_at`): the store is
  dead iff a later instruction fully writes Vd (128 bits, or a scalar write that zeroes the upper part)
  and nothing in between can observe `Cpu.v[d]` (memory reads of Vd by templates, helpers, fallbacks,
  exits, the loop-region exit write-back, stores of the register to guest memory read the cache or
  memory — audit). Unknown or partial writers (lane inserts, by-element destinations, loads of one lane)
  are not killers. Being conservative only costs speed.
- Must work inside loopra regions and AOT units. Keep `ATTESOR_JIT_OFF=vdead` to disable.
- Measure nbody/matmul (kernels.c, bench.c) loop instruction counts before/after with
  `ATTESOR_DUMP_BYTES=8192 --jit-dump` (nbody words in docs/tasks/FPCOLD.md).

## B — cheap host call/return (the old CALLRET measured slower: 110 vs 70 ms on callsites.c)
- Existing opt-in code: `host_call` and the RET path (`ATTESOR_JIT_ON=callret`). Its cost was: RAS depth
  check, epoch push/pop and compare, `RET_TARGET` store/load and compare, an extra indirect hop.
- New design: BL pushes the expected guest return address (pc+4) next to the host return address
  (`call` pushes the host address; push the guest address first). RET (guest target in rax) does
  `cmp rax, [rsp+8]; jne slow; ret`; the continuation after the call pops the guest address and runs the
  caller's next block directly (inline it in the unit as `inline_queue` does today). Mismatch (longjmp,
  tail calls, changed LR) takes the existing indirect path and must unwind the host stack frames that no
  longer match (bounded depth, like `RAS_LIMIT`; reset to the entry stack pointer `HOST_SP` on any
  dispatcher exit).
- Translation cache invalidation: a host return address may point into freed/replaced code. Replace the
  per-call epoch push with a cheaper scheme (e.g. on `EPOCH` change, the dispatcher resets the host stack to
  `HOST_SP`, and continuations are only reachable through `ret` from frames pushed after the last reset);
  explain why it is safe, including for other threads.
- Interactions: loopra regions (a call inside a region is a helper-like barrier: regions with BL are not
  formed today, keep it so or write back first), V cache (cleared across calls), flags.
- Default stays off unless fib, qsort (both kernels.c and bench.c) and `bench/native-ratio/callsites.c`
  are all faster with it on (3 alternating runs, `--no-aot`, report raw numbers and `uptime`); if faster,
  make it default-on with `ATTESOR_JIT_OFF=callret`.
- Add a self-test class `calls`: random call graphs (nested BL/BLR/RET, tail calls via B/BR, functions that
  modify LR before RET, longjmp-like returns to an outer frame, deep recursion beyond the depth bound),
  compared against the interpreter, mismatch=0, and a sensitivity proof (break the RET compare on purpose).

## Always run before each commit
```
cargo build --release -p attesor
cargo test --release --workspace -- --test-threads=1
for E in "" loopra vdead callret; do for c in blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops fpnan tbiblocks calls; do
  ATTESOR_JIT_OFF=$E target/release/attesor --selftest-jit $c 100000 | tail -1; done; done
```
All mismatch=0. (gdbstub.sh cannot bind sockets in the sandbox; Claude runs it.)

## Report (last message)
Per part: changed functions, exact test lines, instruction counts / timings with raw numbers, anything
uncertain, and where the commit messages/patches are.
