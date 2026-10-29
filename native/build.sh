#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
OUT=${OUT:-out}
mkdir -p "$OUT"
clang -arch arm64 -O2 -dynamiclib -lz -install_name @rpath/libattesornative.dylib zlib-shim.c -o "$OUT/libattesornative.dylib"
codesign -s - -f "$OUT/libattesornative.dylib"
clang -arch arm64 -O2 -dynamiclib -framework Accelerate -install_name @rpath/libattesorvimage.dylib vimage-shim.c -o "$OUT/libattesorvimage.dylib"
codesign -s - -f "$OUT/libattesorvimage.dylib"

build_shim() {
    local lib=$1
    shift
    clang -arch arm64 -O2 -Wall -Wextra -Werror -dynamiclib "$@" -install_name "@rpath/libattesor${lib}.dylib" "$lib-shim.c" -o "$OUT/libattesor${lib}.dylib"
    codesign -s - -f "$OUT/libattesor${lib}.dylib"
}
build_shim vdsp -framework Accelerate
build_shim compression -lcompression
build_shim crypto
build_shim imageio
