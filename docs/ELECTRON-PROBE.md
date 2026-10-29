# Real Electron startup and GPU metadata probe

`bench/electron-probe.py` launches the app's actual Attesor-owned universal entry,
which delegates through the installed shared runner. It does not invoke the ARM
binary directly through an alternate flag set, inject an app, or change runtime
sources. The runner's normal generic GPU policy is used. The selected translator
is forwarded through `ATTESOR` and `ATTESOR_BIN`.

```
python3 bench/electron-probe.py \
  --attesor "$HOME/bin/attesor" \
  --app-executable '/Applications/Example.app/Contents/MacOS/Example' \
  --target-pattern '^https?://127\.0\.0\.1:' \
  --ready-regex 'optional readiness line' \
  --expect-renderer Metal --expect-renderer NVIDIA \
  --output /tmp/electron-diagnostic.json --timeout 240 \
  -- --shared-data-dir='{data}/shared' --extensions-dir='{data}/extensions'
```

The additional argument flags are caller-selected, not app-specific rules in the
probe. Each literal `{data}` expands into a short private directory as one argv
element, without a shell. Use the application's supported isolation flags for any
shared stores beyond Chromium's `--user-data-dir`; the probe cannot infer those
from an app name. `--ready-regex` is optional. `--expect-renderer` is repeatable
and every supplied expression must match active renderer metadata. No GPU model
or application name is hardcoded. Choose an appropriate target URL regex for the
application; the example is only a placeholder.

## Isolation and read-only scope

- HOME is preserved, because the real owned stub locates
  `~/attesor-launcher/run-app`. A private Chromium user-data directory, DSH_HOME
  and cache are provided. Runtime data uses a short private `/tmp` path to avoid
  Chromium Unix-socket path limits. GUI alerts are suppressed for the test.
- Flat `.aot` and `.profile` files from an explicit `--cache-dir DIR` are copied
  to the run's private cache before use; unrelated files/directories are ignored.
  It is never used as a writable source. Without that argument, the cache starts
  empty; the user's default cache is never selected. `ATTESOR_NO_AOT_BUILD=1`,
  `ATTESOR_AOT_CHECKPOINT=0`, and `ATTESOR_STATS=1` are always set. Loaded-AOT log
  lines are retained as evidence. There is no prewarm or warm-cache build mode.
- Process inspection refuses launch if it finds an existing instance by its
  canonical bundle path. Incomplete/failed process inspection is an error. A
  per-app lock also prevents two copies of this probe from running together.
- The process is started in its own session. Cleanup signals only that session
  and observed descendants with matching process start identity. It never uses
  `pkill` by app name, profile, or executable. Unknown same-app processes found
  afterward are reported and left untouched, and make cleanup fail rather than
  authorizing a broad kill. A daemon that escapes lineage observation cannot be
  claimed as verified clean; inspect a failed cleanup report before retrying.
- The random loopback DevTools listener must belong to the probe's owned process
  set before HTTP/CDP data is accepted. HTTP proxy settings are bypassed for this
  loopback connection. Browser websocket connection/read operations are bounded
  to 15 seconds, with no page-websocket fallback.
- Readiness reads `/json/version`, page URLs from `/json/list`, and optionally
  the new application log text. The only CDP RPC is browser
  `SystemInfo.getInfo`. No DOM query, Runtime evaluation, screenshot, navigation,
  UI automation, mouse, keyboard, or text operation is performed.
- The GPU gate needs a nonempty active renderer, enabled hardware compositing,
  every requested renderer expression, and no SwiftShader/llvmpipe/softpipe/
  lavapipe/software renderer indication. An inactive device merely being present
  in the device inventory cannot satisfy the gate. Raw GPU metadata is retained.

The normal installed runner writes its existing GUI log under HOME. The probe
records its inode/offset before launch and copies only this run's appended text
into private diagnostic artifacts. It never truncates, clears, rewrites or deletes
the existing log. Entry stdout/stderr has a separate private file. Reports are
atomic files with mode 0600; artifact directories have mode 0700. Successful runs
remove their private runtime data after cleanup; failed runs retain it for
inspection. Explicit source caches and user profiles are never deleted.

A PASS means **the requested page URL (plus optional log line) was observed and
browser GPU metadata satisfied the gate**. It does not establish full UI
readiness, successful frame rendering, general application compatibility, or a
startup/performance benchmark. The timeout controls readiness; bounded metadata
and process cleanup can take additional time. Failure is nonzero and writes a
partial report with logs and observed evidence.

## Validation without starting apps

```
python3 tests/attesor/electron-probe-unit.py
python3 -m py_compile bench/electron-probe.py
python3 bench/electron-probe.py --help
```

These checks cover pure metadata gates, owned-entry file parsing, source-log tail
isolation/rotation and private JSON permissions. They do not start applications,
HTTP listeners, or browser connections. Actual application validation must be run
serially by the integrator after other CPU work has stopped.


## Cleanup diagnostics

Cleanup errors are structured objects. A failed `kill`/`killpg` includes the
operation, signal name/number, PID or process-group ID, errno/type, and observed
identity (UID, parent/group IDs, process start time and executable name). A group
error retains its observed member identities. If name lookup is unavailable it
is marked explicitly; it is never guessed from potentially sensitive argv.
Full command arguments are excluded from these error objects and signal audit
records, including subprocess timeout messages that could otherwise embed argv.

Each signal attempt is recorded in `cleanup.signal_attempts`. A failure does not
stop attempts on other already-authorized targets or silently clear the error
when a later signal succeeds. No permission elevation, broader PID match, or
permission-error exception is added. EPERM and other actual rejections remain
errors even if a later signal succeeds. ESRCH is recorded with its full target
context as `outcome=already_exited`; a process that exited during cleanup is not
a synthetic failure. Final process-start identity checks still decide whether
owned targets remain. Existing ownership checks and the narrow live-Popen
session fallback remain in force.

`electron-probe-unit.py` injects process snapshots, name lookup and signal calls
to verify EPERM continuation, target UID/start identity and argument redaction.
It never starts an application or delivers a real signal.
