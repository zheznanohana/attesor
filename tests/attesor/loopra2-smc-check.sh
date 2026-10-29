#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/../.."
export TMPDIR=/private/tmp
root="$PWD/target/loopra2/smc"
mkdir -p "$root/cache"
clang -O2 -arch arm64 tests/attesor/loopra2-smc.c -o "$root/guest"
for on in '' loopra2; do
    for mode in --interp --no-aot; do
        out=$(ATTESOR_CACHE="$root/cache" ATTESOR_JIT_ON="$on" ATTESOR_JIT_OFF= ATTESOR_JIT_THRESHOLD=1 "${1:-target/release/attesor}" "$mode" --no-aot "$root/guest")
        [[ "$out" == 'loopra2 SMC PASS checksum=20000' ]]
        printf 'on=%s mode=%s %s\n' "${on:-none}" "$mode" "$out"
    done
done
