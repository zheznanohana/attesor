#!/bin/bash
# Debugger stub check: an arm64 program under `attesor --gdb`, driven by lldb in batch mode (symbols,
# breakpoint, arguments, expression, register write, finish). Exits non-zero on any missing result.
#   tests/attesor/gdbstub.sh [attesor-binary]
set -u
ATTESOR=${1:-$HOME/bin/attesor}
dir=$(mktemp -d /tmp/gdbstub.XXXX)
trap 'rm -rf "$dir"' EXIT
cat > "$dir/t.c" <<'C'
#include <stdio.h>
__attribute__((noinline)) long add3(long a, long b, long c) { return a + b * 2 + c * 3; }
int main(void) { long s = 0; for (int i = 0; i < 5; i++) s += add3(i, 10, 100); printf("sum %ld\n", s); return 7; }
C
clang -O1 -g -arch arm64 "$dir/t.c" -o "$dir/t" || exit 1
port=$((20000 + RANDOM % 20000))
"$ATTESOR" --gdb $port "$dir/t" > "$dir/out" 2> "$dir/err" &
for _ in $(seq 50); do grep -q "slide" "$dir/err" && break; sleep 0.1; done
slide=$(grep -o -- '--slide 0x[0-9a-f]*' "$dir/err" | awk '{print $2}')
lldb -b -o "target create $dir/t" -o "gdb-remote $port" -o "target modules load --file t --slide $slide" \
  -o 'b add3' -o 'c' -o 'p a + b' -o 'register write x1 5' -o 'finish' -o 'breakpoint delete' -o 'c' > "$dir/lldb" 2>&1
wait
fail=0
check() { grep -q -- "$2" "$dir/$1" && echo "ok   $3" || { echo "FAIL $3"; fail=1; }; }
check lldb 'add3(a=0, b=10, c=100)' 'breakpoint by symbol, arguments'
check lldb '(long) 10' 'expression a + b'
check lldb 'Return value: (long) $0 = 310' 'register write + finish'
check out 'sum 1600' 'program result reflects the register write'
[ $fail = 0 ] || { echo "--- lldb"; cat "$dir/lldb"; echo "--- stderr"; cat "$dir/err"; }
exit $fail
