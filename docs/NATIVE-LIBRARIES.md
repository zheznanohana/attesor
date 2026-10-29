# Host-native library bridges

All bridges are opt-in. `ATTESOR_NATIVE_LIBS=0` (or unset) disables them;
`ATTESOR_NATIVE_LIBS=zlib`, `vimage`, or `zlib,vimage` selects libraries.
The legacy value `1` still means **zlib only**, not every available bridge.
Unknown names fail before guest startup. `native/build.sh` builds both shims;
`scripts/install.sh` installs both (do not run installation to test a worktree).

## vImage integer convolution (phase N)

```
cargo build --release
TMPDIR=/private/tmp native/build.sh
ATTESOR_CACHE="$PWD/target/private-native-cache" \
ATTESOR_NATIVE_LIBS=vimage ATTESOR_NATIVE_DIR="$PWD/native/out" \
  target/release/attesor ./arm64-program
```

`libattesorvimage.dylib` interposes only `vImageConvolve_Planar8` with a NULL
temporary buffer. Ten fixed-width argument slots carry its eleven C arguments
(the NULL scratch argument is implicit), including signed divisor, kernel
dimensions, ROI offsets, background byte and flags. The host copies the two
32-byte LP64 `vImage_Buffer` descriptors, removes top-byte tags from the
descriptors, nested pixel pointers and kernel, and calls x86-64 Accelerate.
There are no ObjC/CF objects, guest function pointers, or shared mutable scratch
state. Valid-buffer, nonaliasing and kernel-accumulator limits remain vImage's
normal caller contract. Native allocation failure is returned by vImage.

`kvImageGetTempBufferSize` and non-NULL caller scratch always call the original
guest function: scratch size/layout is architecture-private. They are not
silently changed to host sizes. `attesor_native_vimage_calls()` (trap operation
18) counts only actual host convolution calls (operation 17), independently of
the existing zlib diagnostic. Both shims are stripped from native children's
insertion lists and reselected for ARM children; a child can disable them.

Validation and repeatable measurements:

```
tests/attesor/native-vimage-check.sh
tests/attesor/native-vimage-sensitivity.sh
python3 tests/attesor/native-libraries-measure.py --out target/native-survey
python3 tests/attesor/native-libraries-measure.py --bridge --out target/native-ab
```

The directed suite compares full output bytes, padding and signed error codes
against native x86, for three seeds, four library selections, odd kernel sizes
1..7 in each dimension, signed/extreme coefficients, zero/positive/negative
divisors, all four edging modes, ROI offsets, tagged pointers, guest scratch,
four concurrent pthreads and a 1 MiB internally tiled image. The expected host
counter is 136 when selected and zero otherwise. Seven directed error paths,
native-middleman exec/child-disable, unknown selection and missing-shim paths
are included. The sensitivity script changes only a private shim copy to pass
`divisor+1`; full-byte comparison must reject it.

The survey deliberately does NOT bridge resampling, FFT, ImageIO/CF objects,
or opaque library state. In particular, ARM/x86 `vImageScale_Planar8` produced
different bytes in a directed 60x12 -> 13x27 fixture, despite matching on the
initial half-size fixture. Reproduce it with the survey binary's
`rejected-scale` argument (writes raw bytes, not timing text). LZ4 encoding and
FFT also differed across architectures. See
[`tasks/phase2/N-NATIVE-REPORT.md`](tasks/phase2/N-NATIVE-REPORT.md) for raw
measurements, rejected/deferred APIs, limitations and acceptance status.

## Existing zlib bridge

`ATTESOR_NATIVE_LIBS=1` routes dynamically bound `crc32`, `adler32`,
`compress2` and `uncompress` calls to the host x86-64 libz. The default is off.
An arm64 interposition shim issues a dedicated Attesor trap (`x16=0x4154`),
which calls the host implementation in the same process. This does not depend
on the Metal bridge or a GPU.

## Build and use

```
cargo build --release
native/build.sh
ATTESOR_NATIVE_LIBS=1 ATTESOR_NATIVE_DIR="$PWD/native/out" \
  target/release/attesor ./arm64-program
```

`scripts/install.sh` builds the shim and installs it to `$PREFIX/native/`.
Without `ATTESOR_NATIVE_DIR`, Attesor searches `native/` next to the emulator,
the emulator's directory, then `~/.attesor/native/`. An explicit
`ATTESOR_NATIVE_DIR` overrides that search. Requesting acceleration with a
missing shim fails before loading the guest with a clear diagnostic.

`ATTESOR_NATIVE_LIBS=0` keeps the original guest implementation. The option and
shim directory are forwarded to re-launched ARM children. ARM shims are removed
from native children's `DYLD_INSERT_LIBRARIES`, and fresh ARM children receive
the shim again if enabled. A child can disable the bridge independently.

## ABI and limits

Darwin arm64 and x86-64 use the same LP64 types for these four zlib APIs.
The bridge retains seed values, 32-bit checksum lengths, 64-bit buffer lengths,
error return values and destination-length updates. Source, destination and
length pointers remain in shared guest/host address space; top-byte tags are
removed at the host boundary. Calls must satisfy zlib's normal valid-buffer
contract. Independent calls can execute concurrently on guest threads.

No `z_stream`, guest allocator or callback crosses the boundary. Streaming
`deflate`/`inflate`, dictionary/state APIs, `crc32_z`/`adler32_z`, and statically
linked copies are not directly replaced. Host and guest zlib versions can use
different compressed representations; the contract is successful interoperation
and decoded data, not a guaranteed byte-identical compressed stream across OS
versions. This is a concrete first library bridge, not a general C ABI bridge.
Small calls may lose to trap overhead; default enablement needs performance
acceptance on actual workloads.

## Correctness acceptance

```
ATTESOR=$PWD/target/release/attesor tests/attesor/native-zlib-check.sh
```

The suite builds isolated probes and a native exec helper. It compares native
x86 against translated ARM with the bridge off and on, including:

- published CRC32 and Adler32 values for `123456789`, incremental seeds and
  zero-length/null initialization;
- 4 MiB compressible and pseudorandom inputs, compression levels 0/3/6/9,
  complete round trips and too-small decode buffers;
- invalid compression level, too-small compression output, invalid encoded
  input, empty payload and a fixed externally encoded `abc` stream;
- tagged pointers and four guest pthreads with independent compression buffers;
- ARM guest -> native helper -> ARM guest, both retaining and disabling the
  bridge in the child;
- explicit missing-shim failure.

The shim exports `attesor_native_calls()`, a diagnostic served by the host
counter. The expanded probe records 4332 calls enabled and zero disabled.
Its normal stdout matches the native x86 probe exactly; the diagnostic goes
to stderr. Counter evidence distinguishes an actual host path from a guest
implementation that happens to produce the same output.

The new concurrent probe exposed an existing guest-thread startup race in
bridge-disabled mode: a host thread wrote its Mach port into an already-unmapped
guest pthread object (`threads.rs:123`). The independent reproducer is four
no-op threads created and joined repeatedly; it requires no zlib or bridge.
The crash report is `attesor-2026-10-29-122417.ips` (PID 76031). A temporary
synchronous setup handshake eliminated observed failures in five reproducer
runs; the thread-runtime correction is handled separately by the integrator.
A fresh build matching this bridge commit reproduced the disabled-mode startup
SIGSEGV again (PID 78906); the standalone bridge source therefore does not claim
that concurrency is accepted. `NATIVE_ZLIB_SINGLE_THREAD=1` isolates library
semantics from that runtime defect while leaving the default suite's concurrency
gate intact. The integrator supplied thread-runtime fix `06f92a6`. The combination of bridge
`dd99394` and that fix passes the full default suite, including all four-thread,
tagged-pointer and native-middleman cases, with 4332 host calls enabled and zero
disabled. The combined validation log is in
`measurements/2026-10-29-native-zlib.txt`. Default enablement remains pending
serial performance acceptance and application-level evidence.

## Performance acceptance

Build `tests/attesor/native-zlib-bench.c` for arm64 and x86-64 with `-O2 -lz`.
It reports separate times for 512 MiB of CRC32, 512 MiB of Adler32, and one
16 MiB compression/decompression round trip, plus semantic checksums. Compare
the same candidate emulator with the switch off/on in alternating serial runs;
first ensure the native and both translated modes print identical checksums.
No performance number was collected during parallel development.
