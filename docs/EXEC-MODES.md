# Programmatic exec of ARM apps with universal GUI stubs

The injected x86 slice is an entry point for a graphical launch, not a genuine
native implementation. Both the guest spawn path and the native exec shim used
to treat any x86+ARM file as native. Consequently an Electron-as-Node backend
could enter the GUI runner, receive GUI/GPU switches and lose its original stdio.
The real DSH log exhibited `bad option: --no-sandbox` and backend exit status 9.
This fix changes executable classification, without application names, bundle
IDs or business-code patches.

## Selection rules

- Thin ARM and ARM-only FAT images run through Attesor.
- Genuine native and genuine x86+ARM universal implementations stay native.
- A FAT image with ARM plus an Attesor-owned x86 GUI stub selects ARM when
  invoked programmatically. Ownership is a complete `ATTESOR_STUB_V<digits>`
  string with a NUL terminator in the actual x86 Mach-O's `__TEXT,__const` or
  `__TEXT,__cstring` sections. A marker in an ARM slice is insufficient.
- The parser validates FAT32/FAT64 tables, file ranges, x86 Mach-O commands and
  section ranges. It does not scan an entire potentially large native program.
  Invalid/truncated ownership metadata never authorizes a native-to-ARM redirect.
- A script whose shebang interpreter needs translation follows the same rules.
  The single optional shebang argument retains Darwin's unsplit semantics.
  Native interpreters, including `/usr/bin/env`, retain their ordinary host path.
- Native shim path caches are thread-local; concurrent first spawns no longer
  read a partially initialized shared pathname. Exec's interpreter buffers are
  local to the call as well.

Programmatic redirection preserves arguments after argv[0], environment,
working directory, stdin/stdout, inherited IPC descriptors and exit status.
The existing front end still normalizes guest argv[0] to its executable path;
arbitrary custom argv[0] is not newly implemented here. The launcher separately
handles GUI versus Node/Chromium-child modes for direct native stub invocation.

## Reproduction

```
cargo build --release -p attesor
cargo test -p attesor selection_tests -- --nocapture
python3 tests/attesor/exec-modes-check.py target/release/attesor
```

The Rust unit checks thin/native/managed inputs, FAT32/64 in both byte orders,
marker placement, duplicates, missing terminators and malformed ranges. The C
classifier uses a shared header with the native shim and is checked independently
against compiled executable fixtures.

The process suite passed **51 checks**. Two differently named fixture bundles
have Electron/Chromium-style framework layouts; the executable/Node behavior is
provided by a small independent C probe rather than modifying either real app.
It covers guest spawn/exec, native spawn/spawnp/exec, guest -> native -> ARM,
real FAT32 and FAT64 ARM execution, genuine universal native execution,
shebangs including `/usr/bin/env`, empty/quoted/Unicode arguments, Node and
non-Node environments, cwd, standard input/output, IPC descriptors 3 and 7,
status 17, and eight concurrent native first spawns. Classification-only swapped
FAT inputs are not claimed as loader execution coverage: the current loader's
normal supported FAT output is big-endian.

The pre-fix installed binary failed the first managed guest spawn with status
103 and `WRONG: native GUI stub was executed`. The candidate passed that case
and the remaining suite. [Recorded candidate output](measurements/2026-10-29-exec-modes.txt).

Fixtures use a private temporary directory and cache, disable background AOT
builds, and neither modify nor register installed applications. Real DSH and
ARM VS Code UI/startup acceptance remains the integrator's next verification.

## Runtime configuration through native middlemen

The resolved sysroot/dyld and configured GPU directory/mode are now carried in
child environments as well as the direct Rust re-exec argv. JIT ON/OFF controls
also survive re-exec so multiprocess A/B measurements use the same policy in
helpers. Tests use a sysroot path containing spaces and a CPU-only GPU role
filter; all 116 exec/argv0/shebang/IPC cases verify these settings in the leaf.
This does not change the Metal protocol or add application-specific branches.
