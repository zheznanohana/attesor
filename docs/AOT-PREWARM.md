# Static application prewarm

`attesor --prewarm <program|App.app>...` builds main-executable and dyld
translations before a program is launched. `--aot` remains an alias. It reads
Mach-O code and function-start metadata; it does not run dyld, constructors,
`main`, app helpers, or application code. It does not launch or register an app,
change its contents, or create/update execution `.profile` files. Its output is
the normal AOT cache plus advisory lock files.

```sh
attesor --prewarm ./arm64-tool
attesor --prewarm "/Applications/Example.app"
attesor --prewarm --dyld /usr/lib/dyld ./tool-one ./tool-two

# Optional background job. The CLI itself is synchronous and has no daemon.
attesor --prewarm "/Applications/Example.app" > /tmp/example-prewarm.log 2>&1 &
```

Put options before target paths. Bundle traversal includes ARM `MH_EXECUTE`
files in nested helper bundles, skips native-only files and resources, does not
follow symlink-directory cycles, and deduplicates canonical paths. A library's
runtime slide is not known before dyld maps it, so bundled dylibs/frameworks are
not claimed as statically prewarmed. The arm64 dyld itself uses Attesor's fixed
`DYLD_BASE` and is translated once per batch.

The command prepares translations even when `ATTESOR_NO_AOT_BUILD=1` disables
automatic runtime building: an explicit prewarm is a direct build request.
`--no-aot`, interpreter and debugger modes are rejected with prewarm. Use the
same cache directory, emulator build, ISA/JIT switches, dyld and commpage mode
for preparation and launch. Normal UUID/path, host-feature and code-generation
compatibility checks still apply. Rebuilding Attesor invalidates previous
translations; this command does not bypass that protection.

## Corrections to the earlier `--aot` path

- Initialize commpage configuration before cache compatibility is calculated.
  The old command did not do so, leaving forced-redirect prewarms incompatible
  with a real runtime process.
- Use `LC_MAIN`/`LC_UNIXTHREAD` entry points as well as function starts. Static
  discovery follows a direct `BL` callee and its continuation, so a binary
  without `LC_FUNCTION_STARTS` can still pretranslate its entry/call chain.
- Reserve the image range without overwrite before mapping executable segments,
  validate that segments stay in that range, and unmap after each image. This
  permits several main images to reuse `MAIN_BASE` in one process without the
  old unconditional `MAP_FIXED` clobber/leak behavior.
- Keep main keys at the canonical runtime path and dyld at the configured path,
  selecting the same ARM slice and fixed addresses that normal loading uses.
- Preserve existing execution profiles and compatible larger concurrent builds.
  Empty/untranslatable or native-only targets fail clearly instead of reporting
  a successful zero-block preparation.

On this host the directed probe's main plus dyld preparation generated a main
translation and 30,164 dyld blocks. That is coverage evidence, not an application
speedup claim. A first process can load those translations before executing any
guest code. Remaining shared-cache/framework code may still translate at runtime;
subsequent profile-driven builds can refine coverage.

## Correctness acceptance

```sh
cargo build --release
cargo test -p attesor aot::tests -- --test-threads=1
python3 tests/attesor/aot-prewarm.py --attesor target/release/attesor
python3 tests/attesor/aot-checkpoint.py --attesor target/release/attesor
```

The prewarm test has 29 directed assertions. A private constructor/main sentinel
proves non-execution during preparation, followed by a real new process that
loads both main and dyld and matches an independent x86 checksum. Other cases
cover existing-profile preservation, forced commpage redirect, binaries without
function starts (the callee PC is present in the AOT index), app/helper batches,
aliases, Unicode/space paths and incompatible modes. The mapping unit test keeps
a sentinel in an occupied region and proves a failed reservation cannot
overwrite it. All seven existing checkpoint/fork/failed-exec process cases remain
part of acceptance.

## Serial startup measurement

```sh
python3 bench/prewarm-startup.py --attesor target/release/attesor --runs 7 \
  --output /tmp/attesor-prewarm-startup.json
```

Run this alone, after correctness jobs have finished. It alternates fresh empty
caches with the same statically prepared cache. Both use the same emulator,
guest and JIT/ISA configuration; automatic building is disabled. Every warm
sample must prove actual main/dyld loads, every cold sample must prove their
absence, all outputs must match native x86, and prepared AOT files must remain
byte-identical. Preparation time and estimated launch amortization are recorded
separately from process wall time. This is a short synthetic CLI process with one
integer kernel and dyld/libc startup; it is not Electron/GUI readiness or an
official Rosetta comparison. A one-pair script smoke check is not performance
acceptance; use repeated serial measurements before making a speedup claim.
