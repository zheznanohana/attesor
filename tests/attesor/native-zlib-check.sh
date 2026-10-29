#!/bin/bash
# End-to-end bridge validation; timings are deliberately not collected.
set -euo pipefail
cd "$(dirname "$0")/../.."
ATTESOR=${ATTESOR:-$PWD/target/release/attesor}
tmp=$(mktemp -d /tmp/attesor-native-check.XXXXXX)
cleanup() {
    result=$?
    if [ "$result" = 0 ]; then rm -rf "$tmp"; else echo "native-zlib failure logs: $tmp" >&2; fi
}
trap cleanup EXIT
mkdir -p "$tmp/native" "$tmp/host"
OUT="$tmp/native" native/build.sh
clang -w -O2 -arch x86_64 tests/attesor/native-zlib.c -lz -o "$tmp/probe-x86"
clang -w -O2 -arch arm64 tests/attesor/native-zlib.c -lz -o "$tmp/probe"
clang -w -O2 -arch x86_64 tests/attesor/native-helper.c -o "$tmp/helper"
clang -w -O2 -arch x86_64 -dynamiclib gpu/attesorexec.c -o "$tmp/host/libattesorexec.dylib"
"$tmp/probe-x86" > "$tmp/reference" 2> "$tmp/reference-err"
for mode in 0 1; do
    flag=--off; [ "$mode" = 0 ] || flag=--bridge
    ATTESOR_NATIVE_LIBS=$mode ATTESOR_NATIVE_DIR="$tmp/native" "$ATTESOR" --no-aot "$tmp/probe" "$flag" > "$tmp/result" 2> "$tmp/err"
    diff -u "$tmp/reference" "$tmp/result"
    cat "$tmp/err"; tail -1 "$tmp/result"
done
for mode in --chain --chain-off; do
    ATTESOR_NATIVE_LIBS=1 ATTESOR_NATIVE_DIR="$tmp/native" ATTESOR_GPU_DIR="$tmp/host" \
      "$ATTESOR" --no-aot "$tmp/probe" "$mode" "$tmp/helper" > "$tmp/chain" 2> "$tmp/chain-err"
    cat "$tmp/chain-err"; tail -2 "$tmp/chain"
done
# Explicit acceleration requests fail clearly when the shim cannot be located.
if ATTESOR_NATIVE_LIBS=1 ATTESOR_NATIVE_DIR="$tmp/missing-dir" "$ATTESOR" --no-aot "$tmp/probe" > "$tmp/missing" 2>&1; then
    echo 'FAIL missing-shim request unexpectedly succeeded'; exit 1
fi
grep -q 'libattesornative.dylib missing' "$tmp/missing"
echo 'native-zlib suite: PASS'
