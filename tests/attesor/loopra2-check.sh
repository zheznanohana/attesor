#!/bin/bash
# Isolated, reproducible phase-2 correctness matrix (no user apps/caches).
set -euo pipefail
cd "$(dirname "$0")/../.."
export TMPDIR=/private/tmp
root="$PWD/target/loopra2/check"
mkdir -p "$root/cache"
export ATTESOR_CACHE="$root/cache"
unset ATTESOR_JIT_ON ATTESOR_JIT_OFF ATTESOR_JIT_VERIFY ATTESOR_JIT_DISABLE
n=${SELFTEST_N:-100000}
cargo build --release -p attesor > "$root/build.log" 2>&1
: > "$root/summary.log"
for on in '' loopra2; do
    tag=${on:-off}
    ATTESOR_JIT_ON="$on" cargo test --release --workspace -- --test-threads=1 > "$root/cargo-$tag.log" 2>&1
    grep 'test result:' "$root/cargo-$tag.log" | sed "s/^/$tag /" | tee -a "$root/summary.log"
    for class in blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks; do
        log="$root/$tag-$class.log"
        ATTESOR_JIT_ON="$on" target/release/attesor --selftest-jit "$class" "$n" 0x1234 > "$log" 2>&1
        line=$(grep 'jit selftest' "$log" | tail -1)
        [[ "$line" == *'mismatch=0'* ]]
        printf '%s %s\n' "$tag" "$line" | tee -a "$root/summary.log"
    done
    for seed in 0xdeadbeef 0xc0ffee; do
        log="$root/$tag-loops-$seed.log"
        ATTESOR_JIT_ON="$on" target/release/attesor --selftest-jit loops "$n" "$seed" > "$log" 2>&1
        line=$(grep 'jit selftest' "$log" | tail -1)
        [[ "$line" == *'mismatch=0'* ]]
        if [[ -n "$on" ]]; then [[ "$line" =~ multi_regions=[1-9] ]]; fi
        printf '%s %s\n' "$tag" "$line" | tee -a "$root/summary.log"
    done
    bash tests/attesor/loopra-aot-check.sh target/release/attesor "$on" > "$root/aot-$tag.log" 2>&1
    cat "$root/aot-$tag.log" | tee -a "$root/summary.log"
done
bash tests/attesor/loopra2-smc-check.sh target/release/attesor > "$root/smc.log" 2>&1
cat "$root/smc.log" | tee -a "$root/summary.log"
