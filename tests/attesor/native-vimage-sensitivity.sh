#!/bin/bash
# Intentionally mis-marshal only a PRIVATE COPY of the shim. No product source
# or default build output is changed. A semantic self-check alone would miss it.
set -euo pipefail
cd "$(dirname "$0")/../.."
export TMPDIR=/private/tmp
ATTESOR=${ATTESOR:-$PWD/target/release/attesor}
tmp=$(mktemp -d /private/tmp/attesor-vimage-fault.XXXXXX)
trap 'rm -rf "$tmp"' EXIT
export ATTESOR_CACHE="$tmp/cache"
python3 - "$tmp/broken.c" <<'PY'
from pathlib import Path
import sys
s = Path('native/vimage-shim.c').read_text()
needle = '(uint64_t)(int64_t)divisor'
assert s.count(needle) == 1
Path(sys.argv[1]).write_text(s.replace(needle, '(uint64_t)(int64_t)(divisor+1)'))
PY
clang -w -O2 -arch arm64 -dynamiclib -I native "$tmp/broken.c" -framework Accelerate -o "$tmp/libattesorvimage.dylib"
codesign -s - -f "$tmp/libattesorvimage.dylib"
for arch in arm64 x86_64; do clang -w -O2 -arch "$arch" tests/attesor/native-vimage.c -framework Accelerate -o "$tmp/probe-$arch"; done
"$tmp/probe-x86_64" > "$tmp/reference"
ATTESOR_NATIVE_LIBS=vimage ATTESOR_NATIVE_DIR="$tmp" "$ATTESOR" --no-aot "$tmp/probe-arm64" --bridge > "$tmp/broken" 2> "$tmp/err"
cat "$tmp/err"
python3 - "$tmp" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1])
a, b = (p/'reference').read_bytes(), (p/'broken').read_bytes()
assert len(a) == len(b), (len(a), len(b))
n = sum(x != y for x, y in zip(a, b))
assert n > 0, 'FAULT NOT DETECTED'
print(f'PASS sensitivity: divisor+1 rejected; {n}/{len(a)} bytes differ')
PY
