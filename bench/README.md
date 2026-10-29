# Benchmarks and test drivers

Everything here is driven from a dev Mac against the Intel test Mac over ssh:

```bash
export HACK=user@your-intel-mac.local     # ssh target (key auth)
```

The Python scripts run **on the Intel Mac** (Python 3.9 from the Command Line Tools is enough, no packages;
`ws.py` is a tiny DevTools websocket client). Copy them to `~/attesor-run/` there (`gpu-exp.sh` and friends do this
for the files they need).

| file | what |
|---|---|
| `compare-cpu.py` | Serial alternating comparison of two translator builds (`--before`, `--after`, `--guest`, `--runs`, `--output`). Disables AOT by default; `--cold-aot` measures recording with a fresh private cache. Supports per-build `--before-env`/`--after-env`, repeated `--guest-arg`, and checksums. Writes raw timings and medians to JSON. Run correctness tests separately. |
| `prewarm-startup.py` | Serial empty-cache versus static `--prewarm` comparison of a private short CLI workload. Proves actual main/dyld cache loads, checks native output and unchanged AOT files, and records prep cost separately. Does not measure GUI/Electron readiness. |
| `bench.c` | arm64 CPU micro-benchmark (qsort, sieve, crc32, list walk, matmul, memops). Build with `clang -arch arm64 -O2 bench.c -o bench && codesign -f -s - bench`, run `attesor --no-aot ./bench`. Use `--no-aot` when comparing translator builds: AOT caches are keyed by `TRANSLATOR_VERSION` and different builds keep invalidating each other. |
| `gpubench.py <secs> <arcs> [js]` | animated 2D-canvas page in the running Chromium (DevTools on :9333): median frame time / fps, optional JS compute timing. Uses focus emulation because the test Mac's window is often reported `hidden` (which throttles rAF). |
| `gpu-exp.sh "ENV=1 ..." [secs]` | restart `chromium-gpu` with `MCL_STATS=1` plus extra env, run `gpubench.py`, print the guest-side GPU profile lines. |
| `jsbench.sh <suffix>` | same, with `~/bin/attesor.<suffix>` as the emulator (A/B of builds). |
| `cfgbench.py`, `chromium-cfg.sh` | Chromium command-line experiments: time to DevTools, page-load time of a 4000-row DOM, process count, RSS, idle CPU, JS timing. |
| `vscode-run.sh`, `typebench.py` | VS Code launch + keystroke-latency driver (DevTools on :9222) + graceful close. |
| `closebrowser.py` | `Browser.close` over DevTools: a graceful exit saves the AOT profile. |

JIT correctness tests are built into the binary, see [../docs/DEBUGGING.md](../docs/DEBUGGING.md).

Run the complete toggle campaign only after correctness jobs are idle:

```sh
python3 bench/serial-acceptance.py --attesor /absolute/path/to/attesor --output-dir /tmp/attesor-serial-results --runs 7
```

The output directory must be new or empty. This entry point builds ARM64 and native reference fixtures, then runs serial alternating OFF/ON comparisons for next-use eviction (pressure, short CLI, mixed CPU kernels), direct branches, TBI masks and opt-in region caching. It finishes with private-cache short CLI prewarm comparison. Counts are configurable with `--pressure-count`, `--branch-count`, `--tbi-count` and `--region-count`; use identical counts when comparing reports. The before side disables the selected default-on optimization; the after side enables it. Region caching is explicitly opt-in on the after side. Other toggles use the translator's defaults. No production defaults change.

Each comparison uses `--reference`; semantic output is required for every checksum-bearing fixture. Ordinary mixed kernels print only timings, so their native comparison cannot establish numerical equivalence; this limitation is explicitly recorded in `manifest.json`. Prewarm uses its own native output reference and proves actual cache loads. Raw JSON, exact commands, build logs, fixture/source hashes and translator/driver hashes remain in the output directory. The runner stops on the first failure and records failed status. It clears inherited translator toggles and bridge injection settings, preserves explicit `ATTESOR_SYSROOT` / `ATTESOR_DYLD`, and uses temporary private caches. Do not run it concurrently with compilation or correctness testing. It measures these workloads only, not general app responsiveness or pure JIT compiler throughput.


`electron-probe.py` is a read-only real-application startup/GPU diagnostic, not a
benchmark. It starts an owned universal app entry with private runtime data,
checks a requested DevTools target URL and browser GPU metadata, and cleans up
only its owned session/observed descendants. See
[the probe guide](../docs/ELECTRON-PROBE.md) for renderer gates, explicit cache
copies, `{data}` argument isolation and scope limits. Do not run it concurrently
with CPU/performance measurements.
