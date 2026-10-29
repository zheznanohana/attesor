#!/bin/bash
# Complete crypto differential smoke suite. Does not measure performance.
set -euo pipefail
cd "$(dirname "$0")/../.."
ATTESOR=${ATTESOR:-$PWD/target/release/attesor}
N=${ATTESOR_CRYPTO_TEST_COUNT:-12000}
mkdir -p tests/attesor/build
check_class() {
    local mode=$1 class=$2 result
    result=$(ATTESOR_JIT_OFF="$mode" "$ATTESOR" --selftest-jit "$class" "$N")
    echo "$result"
    [[ "$result" =~ mismatch=0([[:space:]]|$) ]] || exit 1
    [[ "$result" =~ native=([1-9][0-9]*)([[:space:]]|$) ]] || exit 1
}
for mode in '' shani; do
    check_class "$mode" ffffcc00:4e284800 # AES, including Rd=Rn
    check_class "$mode" bfe0fc00:0ee0e000 # PMULL and PMULL2
    check_class "$mode" ffe0ec00:5e004000 # SHA256H and SHA256H2
    check_class "$mode" fffffc00:5e282800 # SHA256SU0
    check_class "$mode" ffe0fc00:5e006000 # SHA256SU1, including all input aliases
 done
clang -w -arch x86_64 -O2 tests/attesor/crypto-kat.c -o tests/attesor/build/crypto-kat-x86
clang -w -arch arm64 -O2 tests/attesor/crypto-kat.c -o tests/attesor/build/crypto-kat
codesign -s - -f tests/attesor/build/crypto-kat 2>/dev/null
./tests/attesor/build/crypto-kat-x86
"$ATTESOR" --no-aot tests/attesor/build/crypto-kat
ATTESOR_JIT_OFF=shani "$ATTESOR" --no-aot tests/attesor/build/crypto-kat
clang -arch arm64 -march=armv8-a+crypto -O2 tests/attesor/sha-schedule.c -o tests/attesor/build/sha-schedule
clang -arch x86_64 -O2 tests/attesor/sha-schedule.c -o tests/attesor/build/sha-schedule-x86
reference=$(./tests/attesor/build/sha-schedule-x86 17 | tail -1)
for mode in '' shani; do
    result=$(ATTESOR_JIT_OFF="$mode" "$ATTESOR" --no-aot tests/attesor/build/sha-schedule 17 | tail -1)
    echo "$result"
    [ "$result" = "$reference" ] || exit 1
done
