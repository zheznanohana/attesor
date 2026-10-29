# Track G — Round 2 handoff

## Implemented

- **#25**: fixed the fast dispatcher's CREATE path retaining `sync=true`:
  command buffers/encoders now actually batch with subsequent kind-4 records.
  `MCL_NOASYNC` and `MCL_NOBATCH` still force synchronous results. Commit,
  presentation, result-bearing calls and waits retain their flush semantics.
- **Protocol v2**: op 6 negotiates capabilities; the header carries version 2.
  The host accepts old v1 Metal batches, but new guest/host libraries must be
  deployed as a pair. Entire batch envelopes/argument bounds are validated
  before executing records, so malformed tails do not partially execute.
- **Shared input uploads**: >4 KiB pointer payloads have explicit shared-address
  records. Queued void calls take one immutable snapshot; synchronous calls
  borrow input until acknowledgement (no additional snapshot). Snapshots live
  on the single global ordered queue, not on a creator thread's scratch buffer;
  they are freed only after host execution. A 4 MiB queued-snapshot budget also
  forces flushing. No cross-thread transport ordering was relaxed.
- `[mclupload]` counters distinguish snapshot, borrowed and inline input bytes.
  `measure-headless.py --after` retains these counters alongside hostcall counts
  and paired throughput. Missing baseline counters are **null**, not zero.
  Counters do not include framework/driver copies or all command-stream copies.
- **#27**, exact opt-in **`ATTESOR_GPU_VT=1`** with `--gpu`: callback-based
  H.264/HEVC compression/decompression sessions execute on existing dedicated
  host bridge threads. Host sessions require hardware; unsupported hardware
  returns an error, never a pretend `hw=1` or a software fallback.
- Input/output pixel buffers cross as **IOSurface IDs**, never CF object
  addresses. Decoded buffers remain retained by the host until the guest has
  imported/acknowledged their surface. Compressed samples cross as bytes,
  parameter sets, NAL length, full CMTime values, extensions and attachments.
- VT callbacks use the existing guest event pump; user callbacks and rebuilt
  CF objects run only on guest threads. Complete/wait/finish and synchronous
  encode/decode results fence guest callback delivery. Reentrant callbacks
  do not wait on their own event-pump barrier. Weak session registration,
  invalidation and final CFRelease tear down host state.
- VT property set/reset/copy, property dictionaries, prepare, completion, delayed
  decode finish and format acceptance are bridged. `vtcodec.m` now checks API
  failures, synchronizes callbacks, releases sessions and requires hardware
  when opt-in is set.

No translator core, generated code, translator version or Rust transport changed.
No installation, GUI launch, shared Attesor cache, or external process changes.

## Scope / remaining risks

- **Hardware acceptance is pending.** No Metal/VT sessions were run inside this
  sandbox. CPU mocks, format reconstruction and build success do not prove GPU
  correctness, real IOSurface sharing, HEVC bitstream handling or acceleration.
- This is a bounded callback-based VT API subset, not all VideoToolbox APIs.
  Custom allocators, multi-sample compressed buffers, non-H.264/HEVC codecs,
  multipass/storage objects, per-frame output-handler APIs and host pixel-buffer
  pools are not bridged. `GetPixelBufferPool` returns NULL for bridged sessions;
  callers must allocate IOSurface-backed inputs themselves. Non-plist property
  values and non-IOSurface inputs fail; there is no copying/software fallback.
  Do not pass bridged sessions to other, unsupported VT entry points.
- Texture `replaceRegion` remains on the conservative synchronous legacy path:
  do not infer upload lengths from row pitch alone (compressed formats and last
  row padding). Shared payloads currently optimize eligible fastpath pointers,
  including buffer/encoder byte uploads, not every texture upload overload.
- Existing GUI `metallayer.m` / `mtkview.swift` are deliberately **not run**, even
  outside the sandbox, under this task's no-GUI restriction. Headless offscreen
  rendering is covered by `headless.m`; that is not layer/view presentation QA.
- Re-run hardware coverage before accepting performance/default-on behavior.
  Shared addresses are trusted in-process pointers, not an isolation boundary.

## Actual results

External baseline supplied by Claude, 120 frames / seed 1, machine load 6–12:

- native **339.9 fps**;
- guest **259.4 / 270.5 / 246.3 fps**;
- checksum **fc302ac9fe269bef**, identical;
- **63.8 hostcalls/frame** including setup; kinds 1:403, 2:403, 4:6836, 5:16.

**No after throughput or real VT hardware result is claimed.**

Sandbox checks (logs in `target/g/round2/`):

- `cargo build --release --workspace`: PASS.
- `cargo test --release --workspace -- --test-threads=1`: **75 passed**, 0 failed.
- `python3 gpu/test-headless.py`: **8 passed**, including three CPU-oracle seeds
  and 24 intentional byte corruptions.
- `gpu/test-round2.py --sensitivity`: native production-host dispatcher,
  native/guest H.264 wire reconstruction, production guest dispatcher and VT
  asynchronous event pump: PASS. Covers cross-thread queue ordering, uploads
  after caller reuse, borrowed synchronous uploads, drops/waits/toggles,
  negotiation/malformed messages, exact VT opt-in, CF lifetime, callback flags,
  error/refcon/frame tokens, reentrancy, delivery barriers and invalidation.
- Production Rust bridge/profiler v1 compatibility check: PASS (hook + legacy library);
  no Metal device opened.
- Two deliberate private-copy mutations (restore CREATE's erroneous synchronous
  flush; corrupt snapshot data): **both detected at directed assertions**.
  This is CPU-path sensitivity, not real Metal/VT-path sensitivity.
- Guest/host dylibs and both-ABI `vtcodec.m`: compiled; only pre-existing
  selector-cast warnings in the production guest build.
- Python syntax and `git diff --check`: PASS.

## Exact commands Claude must run OUTSIDE the sandbox

The untouched baseline libraries already exist in `target/g/before`.
Do **not** rebuild/overwrite them or install anything to `~/bin`.
All new outputs below get a fresh timestamped private directory.

```sh
set -eu
cd /Users/zhez/attesor-p2g
export TMPDIR=/private/tmp
RUN="$PWD/target/g/round2/claude-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RUN"
export PYTHONPYCACHEPREFIX="$RUN/pycache"

cargo build --release --workspace
OUT="$RUN/after" bash gpu/build.sh
python3 gpu/test-round2.py --attesor "$PWD/target/release/attesor" \
  --sensitivity --output "$RUN/unit"

for SEED in 1 1157247118614186750 18446744073709551615; do
  python3 gpu/measure-headless.py --attesor "$PWD/target/release/attesor" \
    --before target/g/before --after "$RUN/after" \
    --output "$RUN/ab-$SEED" --runs 3 --frames 120 --seed "$SEED"
done

python3 gpu/verify-round2.py --attesor "$PWD/target/release/attesor" \
  --bridge "$RUN/after" --output "$RUN/probes" --runs 3
```

`verify-round2.py` compiles and executes **tests/attesor/rtprobe.m** and
**families.m** natively and through the candidate bridge, requiring identical
stdout, after a native headless Metal preflight. It then runs
**tests/attesor/vtcodec.m** in three alternating native / guest-VT-off /
guest-VT-on sets. H.264 and HEVC must each encode/decode 60 frames with bad=0;
native and guest-VT-on must report encoder **hw=1** and decoder **hw=1**.
Raw timings, commands, stdout/stderr and per-run uptime are retained in `probes/`.
Exit 77 means unavailable, never success; any mismatch/error is a failed gate.
The gate does not run the GUI layer/view tests.

For individual reruns after the gate has built the binaries, these are the
actual guest commands (keep the same private environment):

```sh
export ATTESOR_GPU_DIR="$RUN/after"
export ATTESOR_GPU_BRIDGE="$RUN/after/libmclbridge.dylib"
export ATTESOR_CACHE="$RUN/manual-cache"
export ATTESOR_NO_AOT_BUILD=1
unset DYLD_INSERT_LIBRARIES ATTESOR_GPU_VT
./target/release/attesor --no-aot --gpu "$RUN/probes/rtprobe-arm64"
./target/release/attesor --no-aot --gpu "$RUN/probes/families-arm64"
ATTESOR_GPU_VT=1 ./target/release/attesor --no-aot --gpu "$RUN/probes/vtcodec-arm64"
```
