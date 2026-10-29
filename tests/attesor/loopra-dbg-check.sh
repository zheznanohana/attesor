#!/bin/bash
# Reproduce the loopra correctness matrix without user apps or shared AOT caches.
# Optional: CASES=100000 BATCHES=10 (30 independent seeded loop batches).
set -euo pipefail
cd "$(dirname "$0")/../.."
mkdir -p target/loopra-dbg/cache
export ATTESOR_CACHE="$PWD/target/loopra-dbg/cache"
unset ATTESOR_JIT_ON ATTESOR_JIT_DISABLE ATTESOR_JIT_VERIFY
cases=${CASES:-100000}
batches=${BATCHES:-10}
cargo build --release -p attesor > target/loopra-dbg/build.log 2>&1
cargo test --workspace -- --test-threads=1 > target/loopra-dbg/cargo-test.log 2>&1
cargo test --release --workspace -- --test-threads=1 > target/loopra-dbg/cargo-test-release.log 2>&1
: > target/loopra-dbg/selftests.log
check() {
    local off=$1 class=$2 seed=$3 log=$4
    ATTESOR_JIT_OFF="$off" target/release/attesor --selftest-jit "$class" "$cases" "$seed" > "$log" 2>&1
    local summary
    summary=$(grep '^jit selftest ' "$log" | tail -1)
    printf 'off=%s %s\n' "${off:-none}" "$summary" | tee -a target/loopra-dbg/selftests.log
    [[ "$summary" == *' mismatch=0 '* ]]
    ! grep -q '^MISMATCH' "$log"
}
for off in '' loopra; do
    for class in blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops fpnan tbiblocks calls; do
        check "$off" "$class" 0x1234 "target/loopra-dbg/${off:-on}-$class.log"
    done
done
for base in 0x1234 0xdeadbeef 0x100f5e1f7e57cafe; do
    for ((batch=0; batch<batches; batch++)); do
        seed=$((base + batch))
        check '' loops "$seed" "target/loopra-dbg/stress-$base-$batch.log"
    done
done
echo "PASS: both optimization modes, all classes; $((3 * batches * cases)) extra seeded loop cases"
