#!/bin/bash
# How much of an app's startup the translation cache covers: launch it (isolated data dirs, like
# app-startup.sh) with ATTESOR_STATS, wait for the ready line, stop it, and sum per process what was
# loaded from the cache versus translated or interpreted fresh.
#
#   bench/aot-coverage.sh <cache-dir> <ready-regex> <arm64-executable> [attesor/app args…]
set -uo pipefail
[ $# -ge 3 ] || { sed -n '2,6p' "$0"; exit 2; }
CACHE=$1 READY=$2 EXE=$3; shift 3
ATTESOR=${ATTESOR:-$HOME/bin/attesor}
pre=() post=()
for a in "$@"; do
  case "$a" in --gpu|--gpu-only|--type=gpu-process|--interp|--no-aot) pre+=("$a") ;; *) post+=("$a") ;; esac
done
if pgrep -f -- "$EXE" >/dev/null; then echo "$EXE is running; quit it first"; exit 1; fi
data=$(mktemp -d /tmp/acv.XXXX)
logs=$data/stderr; mkdir -p "$logs"
log=$data/out.log
HOME_LOGS=$HOME/Library/Logs/Attesor/stderr
before=$(ls "$HOME_LOGS" 2>/dev/null | sort)
ATTESOR_CACHE="$CACHE" ATTESOR_STATS=1 ATTESOR_STDERR_LOG=1 ATTESOR_NO_AOT_BUILD=1 DSH_HOME="$data/dsh" \
  "$ATTESOR" "${pre[@]}" "$EXE" "${post[@]}" --user-data-dir="$data/ud" > "$log" 2>&1 &
disown $!
n=0; until grep -qaE -- "$READY" "$log" || [ $n -ge 600 ]; do sleep 0.5; n=$((n+1)); done
sleep 2
pkill -9 -f -- "$data"; while pgrep -f -- "$data" >/dev/null; do sleep 0.5; done
new=$(comm -13 <(echo "$before") <(ls "$HOME_LOGS" 2>/dev/null | sort))
cat "$log" $(for f in $new; do echo "$HOME_LOGS/$f"; done) 2>/dev/null | grep -a '\[attesor\] aot: loaded' |
  awk '{ n += $4; files++ } END { printf "aot: %d blocks loaded from %d cache files (all processes)\n", n, files }'
for f in $new; do rm -f "$HOME_LOGS/$f"; done
rm -rf "$data"
