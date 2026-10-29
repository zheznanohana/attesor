# Track G: headless measurement gate

`headless.m` uses Foundation and Metal only: no AppKit application, windows,
layers, views, drawables, display link or presentation. Each frame submits eight
small buffer-copy command buffers and one offscreen render pass. It exercises:

- `newBufferWithBytes:length:options:` at lengths 1, 7, 8, 4095, 4096, 4097,
  16384, 65536, immediately overwriting the caller's input after each call;
- both `replaceRegion` upload overloads, alternating each frame, with padded
  rows and immediate reuse of the input;
- render-pass descriptor marshalling, exact RGBA clear colours, GPU completion,
  managed-resource synchronization where applicable, and full byte readback.

The workload compares **every output byte**, not just a checksum. A checksum
also permits native/guest/independent CPU-oracle differential comparisons.
`--oracle FRAMES SEED` runs the CPU reference without requesting a device.
`--check-oracle` deliberately corrupts first/middle/last bytes for every buffer
length and requires all 24 mutations to be detected. These are oracle tests,
**not proof of GPU correctness or bridge-path sensitivity** on a machine without
a usable Metal device.

## Build the untouched baseline, then measure

Run from the worktree root, using a fresh output directory each time:

```sh
TMPDIR=/private/tmp OUT=../target/g/before bash gpu/build.sh
TMPDIR=/private/tmp python3 gpu/test-headless.py
TMPDIR=/private/tmp python3 gpu/measure-headless.py \
  --before target/g/before --output target/g/baseline-run --frames 120 --seed 1
```

The driver builds x86-64 and arm64 workloads, checks oracle sensitivity, requires
a working **native** Metal preflight, compares native GPU results to the CPU
oracle, and only then runs three guest measurements with `--gpu --no-aot`.
Use `--sysroot PATH` if automatic discovery is insufficient. The driver clears
inherited `ATTESOR_*`, `MCL_*`, and `DYLD_*` variables, uses a private cache/temp
directory, and never installs libraries or overwrites an existing output tree.
The preflight's exit code 77 means **unavailable**, not success. Failed runs do
not produce throughput claims.

After a measured, separately built candidate exists:

```sh
TMPDIR=/private/tmp python3 gpu/measure-headless.py \
  --before target/g/before --after target/g/after \
  --output target/g/ab-run --runs 3 --frames 120 --seed 1
```

Order is B1/A1/B2/A2/B3/A3. Repeat with several seeds (for example 1,
1157247118614186750, 18446744073709551615). `summary.json` retains raw workload
seconds, throughput, checksums, per-run `uptime`, hostcall counts, and paired
before/after time ratios. Per-run profiles and `profile-report.txt` retain top
selectors, total calls and operation times. Hostcalls/frame includes setup and
all operations (RPC/free/event/batch/selector definitions); it is not a
steady-state-only transport counter. Check the raw profiles for extra threads.

`submitted_upload_bytes` counts caller-supplied buffer bytes plus texture pixel
payload (excluding row padding). It is **not** an actual bridge memcpy counter.
Existing `MCL_STATS` logs retain command-stream bytes, also not all memcpy bytes.
Measuring actual copies will require dedicated accounting after this baseline
gate works. Verification/readbacks are included in workload time; results are
for this deliberately small-command, correctness-heavy workload, not app FPS.

## Current gate result (2026-10-29)

In the restricted development environment, native `MTLCreateSystemDefaultDevice`
returns nil. Native and bridged guest `families.m` match only the null-device
output; that is not a GPU coverage pass. Native and guest `vtcodec.m` both fail
session creation for H.264 and HEVC with OSStatus -12908. `rtprobe.m` aborts in
MetalFX after the null-device condition. Its unchanged layer/view counterparts
launch windows, so **do not run** `metallayer.m` or `mtkview.swift` under the
no-GUI rule. They need explicitly offscreen variants for future verification.

No production bridge, protocol, Rust code, translator or translator version has
been changed. No #25 optimization or #27 VT bridge is implemented or accepted.
Do not interpret this measurement infrastructure as completion of those items.
The next gate is a usable headless Metal/VT execution environment and a valid
baseline; then implement/version/test candidates, including real path mutation
sensitivity, before claiming performance or hardware acceleration.
