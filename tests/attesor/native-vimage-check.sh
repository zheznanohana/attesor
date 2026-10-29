#!/bin/bash
# Native x86 vs guest off/on: compare every output byte, including padding.
set -euo pipefail
cd "$(dirname "$0")/../.."
export TMPDIR=/private/tmp
ATTESOR=${ATTESOR:-$PWD/target/release/attesor}
tmp=$(mktemp -d /private/tmp/attesor-vimage.XXXXXX)
trap 'result=$?; if [ "$result" = 0 ]; then rm -rf "$tmp"; else echo "native-vimage failure logs: $tmp" >&2; fi' EXIT
export ATTESOR_CACHE="$tmp/cache"
mkdir -p "$tmp/native" "$tmp/host"
OUT="$tmp/native" native/build.sh
for arch in arm64 x86_64; do clang -w -O2 -arch "$arch" tests/attesor/native-vimage.c -framework Accelerate -o "$tmp/probe-$arch"; done
clang -w -O2 -arch x86_64 tests/attesor/native-helper.c -o "$tmp/helper"
clang -w -O2 -arch x86_64 -dynamiclib gpu/attesorexec.c -o "$tmp/host/libattesorexec.dylib"
for seed in 1 42 0xdeadbeef; do
    "$tmp/probe-x86_64" --off "$seed" > "$tmp/reference"
    for libs in 0 vimage zlib zlib,vimage; do
        flag=--off; case "$libs" in *vimage*) flag=--bridge;; esac
        ATTESOR_NATIVE_LIBS=$libs ATTESOR_NATIVE_DIR="$tmp/native" "$ATTESOR" --no-aot "$tmp/probe-arm64" "$flag" "$seed" > "$tmp/result" 2> "$tmp/err"
        cat "$tmp/err"; cmp "$tmp/reference" "$tmp/result"
        echo "PASS bytes seed=$seed libraries=$libs"
    done
done
"$tmp/probe-x86_64" > "$tmp/reference"
for mode in --chain --chain-off; do
    ATTESOR_NATIVE_LIBS=vimage ATTESOR_NATIVE_DIR="$tmp/native" ATTESOR_GPU_DIR="$tmp/host" \
        "$ATTESOR" --no-aot "$tmp/probe-arm64" "$mode" "$tmp/helper" > "$tmp/result" 2> "$tmp/err"
    cat "$tmp/err"; cmp "$tmp/reference" "$tmp/result"; echo "PASS $mode"
done
for libs in vimage zlib,vimage unknown; do
    if ATTESOR_NATIVE_LIBS=$libs ATTESOR_NATIVE_DIR="$tmp/missing" "$ATTESOR" --no-aot "$tmp/probe-arm64" > "$tmp/error" 2>&1; then
        echo "FAIL invalid request succeeded: $libs";exit 1
    fi
    grep -Eq 'libattesorvimage.dylib missing|libattesornative.dylib missing|expected 0' "$tmp/error"
done
echo 'native-vimage suite: PASS'
