#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
OUT=${OUT:-out}
mkdir -p "$OUT"
clang -arch arm64 -O2 -dynamiclib -lz -install_name @rpath/libattesornative.dylib zlib-shim.c -o "$OUT/libattesornative.dylib"
codesign -s - -f "$OUT/libattesornative.dylib"
