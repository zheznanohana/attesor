# Attesor

**Run arm64 macOS apps on an Intel Mac.** Attesor is a "reverse Rosetta": it loads an arm64 Mach-O
together with the real arm64 macOS userland (dyld and the arm64e dyld shared cache, taken from an Apple
Silicon macOS image that *you* download) and executes the arm64 code with an interpreter plus an x86-64
JIT. System calls and Mach traps go to the Intel host kernel where the semantics match and are emulated
where they don't. Metal calls can be bridged to the Intel Mac's real GPU.

The motivation: Apple dropped Intel Macs, and apps built for newer macOS versions will eventually be
arm64-only. This project explores how far you can get anyway.

Attesor is a fork of [maclator](https://github.com/linuxkid473/maclator) by linuxkid473 (MIT). See
[What Attesor adds](#what-attesor-adds) for the differences.

> **Status: experimental research project.** Things that run today are listed below; many apps will not.
> Expect rough edges. See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for what is missing.

## What runs (verified upstream on an Intel Core i5-4440, macOS 26.7)

| Program | Status |
|---|---|
| `fastfetch`, `neatvi` (arm64 CLI tools) | works |
| a small AppKit calculator (`tests/guest/calc`) | works |
| VS Code 1.139 arm64 (Electron) | works; typing latency ~40 ms |
| Chromium 157 arm64 snapshot | works, including **GPU acceleration** (ANGLE/Metal → host AMD GPU, 60 fps canvas) |
| balenaEtcher 2.1.7 arm64 (Electron) | starts (device access untested) |

Upstream listed as not done: audio, most IOKit hardware, sandboxed apps, Swift-heavy Apple apps, `CAMetalLayer`-based
apps. Attesor's status for each is below.

## What Attesor adds

Verified on an Intel laptop (RTX 3060 with a third-party NVIDIA Metal driver, macOS 26.7) with the programs in
[`tests/attesor/`](tests/attesor/), each compared against the same program built for x86-64:

| Area | Status |
|---|---|
| `CAMetalLayer` / `MTKView` | **fixed**: drawables are presented by the guest's own Core Animation; `presentDrawable:` used to ship the drawable to the host bridge and crash. 60/60 frames on screen, MTKView inside SwiftUI renders. |
| Audio | **works** (no change needed): AudioQueue, HAL IOProcs, AVAudioEngine and AVAudioPlayer run at the device rate, audible. |
| Sandboxed apps | **container emulated**: guests signed with `com.apple.security.app-sandbox` get the same `~/Library/Containers/<id>/Data` home, Documents and container id as natively. Not enforced: the kernel sandbox profile is not applied, and `NSTemporaryDirectory()` stays the system temp dir. Opt out with `ATTESOR_NO_CONTAINER=1`. |
| Swift / SwiftUI | **works** for the tested programs: Codable, actors, task groups, existentials, Regex, SwiftUI state and `.task`. Large Swift apps are not yet tested. |
| IOKit | basic queries work: platform expert/serial, power sources, HID manager. |
| Metal coverage | `supportsFamily:` and device limits match native; ray tracing (acceleration structure build + intersector in a compute kernel) and the MetalFX spatial scaler give bit-identical results (`rtprobe.m`). |
| VideoToolbox | **works, in software**: H.264 and HEVC encode → decode round trips are correct (`vtcodec.m`). The host's hardware HEVC encoder is not reached from the guest (HEVC encode 2.1 s vs 0.15 s native for 60 720p frames; decode 76 vs 58 ms). |
| Persistent translations | Checkpoints retain partial per-thread profiles and survive guest fork/failed exec. SIGKILL → restart tests prove actual cache loads; full-app startup benefit still needs measurement. |
| Native libraries | Opt-in host zlib bridge (`ATTESOR_NATIVE_LIBS=1`): CRC32, Adler32, compress2/uncompress; error paths, tagged pointers, threads and child-process forwarding tested. See docs/NATIVE-LIBRARIES.md. |
| Half conversion | FPCR rounding, FZ/DN/AHP conversion results verified against an independent integer reference; complete FPSR status is not claimed. |
| App launching | Universal x86 stub plus retained ARM slices; atomic inject/restore and 25 isolated launcher checks, including real ARM execution. Finder/Dock appearance requires separate visual verification. |
| Debugger | `attesor --gdb PORT` or `--gdb-fd FD`: lldb/gdb debugging of the first guest thread. RSP and real LLDB expression/call tests pass via an inherited socket; see docs/DEBUGGING.md. |
| JIT | native x86 translations for TBL/TBX, all FRINT forms, SHA-256 (`sha256h/h2/su0/su1`, no SHA extension needed), vector UCVTF/SCVTF, FCVTZU — each checked with the differential self-test (`mismatch=0`). |


![chrome://gpu in the arm64 Chromium 157 snapshot running under Attesor on an Intel Mac Pro: canvas, compositing, rasterization, WebGL and WebGPU hardware accelerated on an AMD Radeon RX 460 via ANGLE/Metal](docs/img/chromium-gpu.png)

*The arm64 Chromium 157 snapshot running under Attesor on an Intel Mac (`chrome://gpu`): compositing, rasterization, WebGL and WebGPU are hardware accelerated on the machine's AMD Radeon RX 460, through the ANGLE/Metal bridge.*

The latest eight-workstream integration, automated acceptance and serial performance results are
recorded in [docs/TEAM-ACCEPTANCE.md](docs/TEAM-ACCEPTANCE.md).

## Quick start

You need an Intel Mac on macOS 26 (Tahoe) with Xcode Command Line Tools and a Rust toolchain, about 10 GB of
free disk, and an internet connection for the one-time download of the arm64 dyld shared cache.

```bash
cd attesor                    # this repository
scripts/install.sh            # builds attesor + the GPU bridge, installs to ~/bin
scripts/fetch-sysroot.sh      # downloads the arm64e dyld shared cache (≈5.5 GB) into ~/attesor-sysroot
attesor ./some-arm64-binary  # that's it; the sysroot is found automatically
```

Details, alternatives and requirements: **[docs/SETUP.md](docs/SETUP.md)**.
Running Chromium, VS Code and other apps: **[docs/USAGE.md](docs/USAGE.md)**.

## Documentation

| | |
|---|---|
| [docs/SETUP.md](docs/SETUP.md) | requirements, building, getting the arm64 userland, installing the GPU bridge |
| [docs/USAGE.md](docs/USAGE.md) | command-line flags, launching Chromium / VS Code / Etcher, tips |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | symptoms and fixes |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | loader, dyld/shared cache handling, syscall emulation, JIT, AOT cache |
| [docs/GPU.md](docs/GPU.md) | the Metal → host GPU bridge: design, protocol, profiling, pitfalls |
| [docs/PERFORMANCE.md](docs/PERFORMANCE.md) | measurements, JIT optimisations, Chromium configuration |
| [docs/DEBUGGING.md](docs/DEBUGGING.md) | flags, signals, JIT differential self-tests |
| [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) | open problems, bug list, history |
| [bench/](bench/README.md) | benchmark and test drivers |

## Legal

Attesor contains **no Apple binaries**. The arm64 userland is downloaded by you from Apple's own update
servers (via [`ipsw`](https://github.com/blacktop/ipsw)) and stays on your machine. You are responsible for
complying with Apple's software license terms for whatever you download and run, and with the licenses of the
apps you run. `ref/` holds excerpts of Apple open-source code (APSL 2.0 / Apache 2.0) used as reference, with
their license headers intact. See [NOTICE](NOTICE). Attesor does not attempt to run, or to circumvent
protection of, FairPlay/DRM-encrypted binaries.

MIT licensed, see [LICENSE](LICENSE).
