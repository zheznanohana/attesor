# Application launching integration

The launcher retains Claude's universal-stub design: an x86-64 entry is added
beside the original ARM slice(s), and invokes the shared `run-app` script. The
application remains one bundle with its original resources. The UI shim can show
the application's own icon with an ARM badge. This makes the bundle launchable
by the Intel system; it does not give the emulated application a native ARM
kernel identity or enforce its code-signing entitlements.

## Changes and invariants

- The V2 stub passes the executable and every argument directly with `execv`.
  Spaces, quotes, shell metacharacters, Unicode and empty arguments stay data.
  Missing runner returns 127 even if the optional alert succeeds. Alert paths
  and application names are AppleScript arguments rather than source text.
- `attesor-apps` keeps its existing commands and aliases. Its Python
  implementation serializes mutations with a file lock. Every modifying command
  checks whether the app is running; permission errors and unknown process state
  refuse the operation. Scan never restarts the Dock.
- A same-directory prepared executable replaces the original atomically. Every
  retained ARM slice is compared byte for byte before replacement. The exact
  pre-injection executable is saved under a hash of the canonical application
  path, so equal bundle identifiers cannot collide.
- Restore uses the saved original only when its ARM slice(s) still match the
  current application. An application update is retained. Stub upgrades replace
  only the owned x86 slice and keep the current ARM code. Genuine native x86
  slices are not replaced.
- Migration from the old nested-wrapper layout prepares a complete copy and
  atomically swaps directories using macOS `renamex_np(..., RENAME_SWAP)`.
  A failed preparation/swap leaves the old bundle intact. This one-time copy
  requires enough free disk space for the original application.
- `fixdock` remains an explicit repair command, and its process check also
  treats unknown visibility as running. No real Dock changes are needed for
  command-line tests.

`ATTESOR_SCAN_DIRS` accepts colon-separated directory paths, including spaces.
`ATTESOR_NO_DESKTOP=1` disables badge generation, LaunchServices registration,
quarantine changes and Dock repair for isolated tests. `ATTESOR_PROCESS_CHECK`
can select an external process checker (argument: application path; return 0
running, 1 stopped, other unknown). It exists for controlled environments and
fixtures; the default is the system `pgrep` with an escaped path pattern.

## Reproduction

```
python3 tests/attesor/launcher-check.py --attesor target/release/attesor
```

The test uses private applications, HOME, runner, process checker and tool
fixtures. It never injects a real installed app, edits a user's Dock, or launches
an alert. GUI appearance and Finder/Dock behavior require separate visual
validation; passing these command-line checks does not claim that validation.

The final run passed **25 checks**, including delegation into the built emulator
(the retained ARM program prints `guest-ok` and its exit status 17 survives the
stub and runner). Other cases cover empty/quoted/Unicode/shell-like arguments,
ordinary and Electron GPU flags, missing-runner status 127, running/unknown
process refusal, failed injection/restore preparation, path-specific backups,
app updates, old-stub upgrades, arm64+arm64e slices, exact thin/fat restoration,
and legacy nested-wrapper migration. The raw output is in
[the recorded checks](measurements/2026-10-29-launcher-check.txt).

The real-emulator check discovers the installed ARM cache before changing HOME;
`--sysroot DIR` can specify it explicitly. Omitting `--attesor` runs the other
24 isolated command-line checks. The process checker is controlled in the fixture,
so the running/unknown tests prove refusal semantics, not unrestricted process
visibility in this sandbox. The default checker remains fail-closed.


## Generic Electron and Chromium process roles

Automatic scanning and stub injection remain the default for newly discovered
ARM-only applications. No application-name or bundle-id rule is used.

The same application executable may be launched as its GUI, an Electron-as-Node
host, or a Chromium child process. Before adding GUI/GPU switches, changing cwd
or opening GUI log files, `run-app` now recognizes a nonempty
`ELECTRON_RUN_AS_NODE` or the `--type=…` / `--type …` process option. Those roles
`exec` Attesor with the original executable and arguments. They retain inherited
stdin/stdout/stderr pipes, cwd, environment (including `NODE_OPTIONS`) and exit
status. Top-level GUI startup keeps its existing defaults and log behavior.

The additional `tests/attesor/run-app-modes.py` suite uses both Electron Framework
and Chromium Embedded Framework fixture applications through real owned stub
entries. It verifies 64 KiB pipe data, exact arguments, environment/cwd/status,
renderer/utility/GPU/split-option modes, and a GUI parent launching the same
executable as a Node child. With `--attesor`, both fixtures also execute a real ARM
program through the transparent child path. All **17** checks pass. This suite is
called by `launcher-check.py` after its original **25** checks. Real DeepSeek
Harness and ARM VS Code validation remains a separate integration check.


## One PID from GUI entry to guest

The top-level GUI runner now uses `exec` as the Node/child runner already did.
The x86 stub, shell runner and translator therefore retain the PID originally
started for the application; the GUI no longer runs behind a waiting Bash
supervisor. Existing GUI GPU switches, cwd, log rotation, stdout/stderr logging,
and translated-program exit status remain unchanged. Node and Chromium child
paths retain their inherited pipes and environment.

A runtime's nonzero GUI exit is now written to the GUI log without a modal exit
alert: there is no foreground supervisor left to display one after the runtime
exits. A missing runtime also fails directly through shell `exec` and its error
is logged (the platform shell supplies the failure status). The native stub's
separate missing-runner startup message is unchanged.

The process-role regression now captures the `Popen.pid` used to start the stub
and compares it with the actual translator PID, for GUI and every child role.
Both framework fixtures additionally run a real ARM program that writes
`getpid()` to a private file, proving identity through the real GUI and Node
paths rather than only a fake translator. These checks do not claim that a PID
alone proves every LaunchServices or GUI automation behavior.

Final regression: the original **25** launcher checks and **35** extended role/
PID checks passed. The latter includes both real ARM GUI/Node PID comparisons
and confirms a GUI runtime failure does not invoke the old post-exit alert path.
[Recorded output](measurements/2026-10-29-gui-exec.txt).
