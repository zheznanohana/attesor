# Host release profile candidate

Source: `791acf45c46de7653ca46af481f95599ffef155c` (tree `acc12d47f17583c7f2bd229b6b05517cec5d62d3`). Production profiles, application bundles and GPU sources are unchanged. These are candidate builds only; neither is installed.

The existing release profile uses opt-level 3, debug 1 and panic abort. Cargo's default release codegen unit count is 16 and explicit LTO is absent. The candidate adds only environment overrides `CARGO_PROFILE_RELEASE_LTO=thin` and `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`. No RUSTFLAGS, fast-math, target-cpu or deployment-target overrides were present. Both builds use rustc 1.99.1 / LLVM 23.1.1 on x86_64-apple-darwin and the same Cargo.lock.

Commands (with pre-existing CARGO_PROFILE_RELEASE_* variables removed):

```sh
cargo build --release -p attesor --target-dir /tmp/attesor-release-candidates/default-target
CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 cargo build --release -p attesor --target-dir /tmp/attesor-release-candidates/thin-cgu1-target
```

| Candidate | Binary bytes | SHA256 | Build wall seconds |
| --- | ---: | --- | ---: |
| default | 2323936 | 70b266a0f521247bd42cb581aa9e504d399fc94da6a12f75e8e262636c0cde88 | 61.38 |
| thin-cgu1 | 2223160 | df6c3a34d7fdc6b58c1dc0478967cb02edec0c1e988951db557b3183263f9c6f | 96.76 |

Builds were sequential with separate new target directories, but other agents shared the host; build times are observations, not isolated comparisons. One compiler RSS sample during candidate linking was approximately 280 MiB, not a measured peak. The candidate built successfully without an obviously prohibitive resource demand. Existing warnings appear in the build logs. Binary file size decreased by 100776 bytes (4.34%); split debugging artifacts are not included in these executable sizes.

## Rationale and measurement boundary

The host engine calls `attesor_core::interp::step` (`engine.rs`); JIT fallback calls `attesor_core::interp::exec` (`jit/mod.rs`). `step` is marked inline, but `exec` is not. The JIT emitter also calls cross-crate bitmask/SIMD immediate helpers. Thin LTO may optimize these boundaries and single-CGU compilation may improve intra-crate optimization. Many softfloat helpers already use generics and inline(always), so LTO should not be assumed to unlock all of that code for the first time. Emitted guest machine code is not inherently made faster by optimizing the Rust host compiler; hot translated execution, cold interpretation, and translation throughput require separate measurements.

No runtime performance measurements were made here. The coordinator must serially compare the two binaries using the same fixtures and settings before choosing a default. The candidate is pinned to this source revision and must not be compared against a different revision as though profile settings were the only variable.

Build logs, full manifest, exact correctness commands and test logs are in `/tmp/attesor-release-candidates/`. The manifest records source/tree/lock hashes, compiler identity, inherited compiler settings, build commands, binaries and their SHA256 values. Binary paths are `<candidate>-target/release/attesor` beneath that directory.

## Suggested serial follow-up (not executed here)

A short ARM64 fixture compiled from unchanged `tests/attesor/aot-prewarm-probe.c` is available at `/tmp/attesor-release-candidates/short-arm64` (with an x86_64 companion). It performs normal dyld/libc startup and one small integer kernel. The following compares empty per-process JIT startup plus execution, with AOT disabled by the existing driver:

```sh
python3 bench/compare-cpu.py --before /tmp/attesor-release-candidates/default-target/release/attesor --after /tmp/attesor-release-candidates/thin-cgu1-target/release/attesor --guest /tmp/attesor-release-candidates/short-arm64 --runs 7 --output /tmp/attesor-release-candidates/short-jit-comparison.json
```

For total cold interpretation, use the prepared `default-interp` and `thin-cgu1-interp` shell wrappers as before/after. Each simply execs its corresponding binary with `--interp` and passes all arguments through. Associate their wrapper hashes with the actual binary hashes above. These measurements include normal process startup and are not isolated JIT compile-throughput tests. For long hot translated kernels the existing `bench/native-ratio/kernels.c` is available, but optimizing the host may have little effect once native guest blocks execute. Do not infer hot-kernel gains from short-startup results or vice versa. `--selftest-jit blocks` mixes translation, reference interpretation, checking and execution; label that workload accurately if used.

Driver caveat: this short fixture prints `prewarm-checksum`, whereas `compare-cpu.py` automatically checks only lines starting `checksum`. Explicitly assert every recorded `samples[].stdout` equals every other sample and the `short-x86_64` reference output. An empty automatic checksum match does not validate this fixture.

Coordinator follow-up: main commit `eea2ca4` improves `bench/compare-cpu.py` to compare all non-timing stdout and accepts `--reference` for a native executable. Run that updated driver with `--reference /tmp/attesor-release-candidates/short-x86_64` to enforce the short fixture reference. The old driver in this pinned experiment worktree does not accept this option; the candidate binaries themselves remain pinned and unchanged.

## Correctness results

Both candidates passed the same six sequential checks, all exit status 0:

- NaN payload/sign behavior: `--selftest-jit fpnan 62208`, repeated with `ATTESOR_SELFTEST_FPCR=1`; each run reported 62208 native cases, zero mismatches/skips.
- `--selftest-jit fpblocks 10000`: 64997 guest instructions, zero mismatches/skips and zero fallback instructions.
- `fp16-status-check.py`: 60000 result/status cases with identical integer-reference/JIT/interpreter digest `725b1a2cd2c9de4f`, clear isolation and double boundaries pass; 20000 native half conversions each in default and FPCR modes, zero mismatch/skip.
- `thread-startup-check.py --runs 3 --rounds 30`: native reference plus three ARM64 processes, four joined workers per round, pass.
- `exec-modes-check.py`: all 116 checks pass for both binaries, including native/guest exec selection, two Electron-shaped fixtures, Node-mode and environment forwarding, IPC descriptors, argv0 and shebang behavior.

Candidate SHA256 values were verified again after all tests. Machine-readable metadata and exact commands are copied to `docs/measurements/2026-10-29-release-profile-candidates.json`. Full logs remain under `/tmp/attesor-release-candidates`. Correctness tests do not establish any performance improvement.
