#!/bin/bash
# Startup benchmark for an arm64 app under attesor: wall time until a "ready" line appears in its output,
# and CPU seconds spent by all of its attesor processes up to that point.
#
#   bench/app-startup.sh [--runs N] [--warm] [--cache DIR] [--timeout S] <ready-regex> <arm64-executable> [attesor/app args…]
#
# Default is a cold run: an empty translation cache per run and no AOT build, so the numbers measure the
# interpreter + JIT (what JIT work changes). --warm uses one cache dir kept across the runs (first run
# fills it; the AOT build only happens on a graceful exit, which this script does not do). --cache DIR uses
# (and keeps) an existing translation cache, e.g. one filled by a normal session that quit gracefully.
# Each run gets its own HOME-independent data dirs (Electron --user-data-dir, DSH_HOME), and the script
# refuses to start while the app is already running, so it never touches an instance someone is using.
# Example (DeepSeek Harness, GPU bridge as the app launcher uses it):
#   bench/app-startup.sh --runs 3 'dsh web: http' \
#     "/Applications/DeepSeek Harness.app/Contents/Resources/Original/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness" \
#     --gpu --gpu-only --type=gpu-process --no-sandbox --disable-gpu-sandbox --disable-gpu-watchdog --use-angle=metal \
#     --use-gl=angle --enable-gpu --ignore-gpu-blocklist --disable-angle-features=disableMetalOnNvidia
set -uo pipefail
RUNS=1 WARM=0 TIMEOUT=300 CACHE=
while [ $# -gt 0 ]; do
  case "$1" in
    --runs) RUNS=$2; shift 2 ;;
    --warm) WARM=1; shift ;;
    --cache) CACHE=$2; WARM=1; shift 2 ;;
    --timeout) TIMEOUT=$2; shift 2 ;;
    *) break ;;
  esac
done
[ $# -ge 2 ] || { sed -n '2,13p' "$0"; exit 2; }
READY=$1 EXE=$2; shift 2
ATTESOR=${ATTESOR:-$HOME/bin/attesor}
# attesor options go before the program, everything else after it
pre=() post=()
for a in "$@"; do
  case "$a" in --gpu|--gpu-only|--type=gpu-process|--interp|--no-aot) pre+=("$a") ;; *) post+=("$a") ;; esac
done

cpu_of() {   # summed CPU seconds of every attesor process running $EXE
  ps -axo time=,command= | grep -F -- "$EXE" | grep -F "$ATTESOR" | grep -v grep | awk '{
    n = split($1, t, ":"); s = 0; for (i = 1; i <= n; i++) s = s * 60 + t[i]; sum += s } END { printf "%.1f", sum }'
}
stop_all() {   # only this run's processes: each mentions its private data dir (--user-data-dir, DSH_HOME, crashpad db)
  pkill -9 -f -- "$1" 2>/dev/null
  while pgrep -f -- "$1" >/dev/null; do sleep 0.5; done
}

if pgrep -f -- "$EXE" >/dev/null; then
  echo "$EXE is running; quit it first (the benchmark must not share its profile or single-instance lock)"; exit 1
fi
warm_cache=${CACHE:-$(mktemp -d /tmp/attesor-bench-cache.XXXX)}
walls=() cpus=()
for r in $(seq "$RUNS"); do
  if [ $WARM = 1 ]; then cache=$warm_cache; else cache=$(mktemp -d /tmp/attesor-bench-cache.XXXX); fi
  log=$(mktemp /tmp/attesor-bench-log.XXXX)
  data=$(mktemp -d /tmp/abd.XXXX)   # short: Electron's IPC socket path must stay under 104 bytes
  env_extra=(ATTESOR_CACHE="$cache" DSH_HOME="$data/dsh")
  [ $WARM = 0 ] && env_extra+=(ATTESOR_NO_AOT_BUILD=1)
  start=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  env "${env_extra[@]}" "$ATTESOR" "${pre[@]}" "$EXE" "${post[@]}" --user-data-dir="$data/ud" > "$log" 2>&1 &
  disown $!
  ok=0
  while :; do
    if grep -qaE -- "$READY" "$log"; then ok=1; break; fi
    now=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
    awk -v a="$now" -v b="$start" -v t="$TIMEOUT" 'BEGIN { exit !(a - b > t) }' && break
    sleep 0.2
  done
  now=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  wall=$(awk -v a="$now" -v b="$start" 'BEGIN { printf "%.1f", a - b }')
  cpu=$(cpu_of)
  stop_all "$data"
  if [ $ok = 1 ]; then
    echo "run $r: ready in ${wall}s, cpu ${cpu}s"
    walls+=("$wall") cpus+=("$cpu")
  else
    echo "run $r: TIMEOUT after ${wall}s (cpu ${cpu}s); last output:"; tail -5 "$log" | sed 's/^/    /'
  fi
  [ $WARM = 0 ] && rm -rf "$cache"
  rm -rf "$log" "$data"
done
[ -z "$CACHE" ] && rm -rf "$warm_cache"
median() { printf '%s\n' "$@" | sort -n | awk '{ v[NR] = $1 } END { if (NR) print v[int((NR + 1) / 2)] }'; }
[ ${#walls[@]} -gt 0 ] && echo "median: ready $(median "${walls[@]}")s, cpu $(median "${cpus[@]}")s (${#walls[@]}/$RUNS runs, $([ $WARM = 1 ] && echo warm || echo cold))"
