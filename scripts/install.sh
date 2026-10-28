#!/bin/bash
# Build attesor + the GPU bridge and install them into $PREFIX (default ~/bin).
#   TARGET=x86_64-apple-darwin scripts/install.sh   # when building on an Apple Silicon Mac (cross-compile)
set -euo pipefail
cd "$(dirname "$0")/.."
PREFIX=${PREFIX:-$HOME/bin}
TARGET=${TARGET:-}
command -v cargo >/dev/null || { echo "Rust is required: https://rustup.rs"; exit 1; }
if [ -n "$TARGET" ]; then
  rustup target add "$TARGET" >/dev/null 2>&1 || true
  cargo build --release --target "$TARGET" -p attesor
  BIN=target/$TARGET/release/attesor
else
  [ "$(uname -m)" = x86_64 ] || { echo "not an Intel Mac; set TARGET=x86_64-apple-darwin to cross-compile"; exit 1; }
  cargo build --release -p attesor
  BIN=target/release/attesor
fi
gpu/build.sh
mkdir -p "$PREFIX/gpu"
install -m 755 "$BIN" "$PREFIX/attesor"
install -m 755 gpu/out/libmclbridge.dylib gpu/out/libmclmetal.dylib gpu/out/libattesorui.dylib "$PREFIX/gpu/"
for s in gpu/chromium-gpu scripts/vscode-gpu scripts/etcher-gpu; do install -m 755 "$s" "$PREFIX/$(basename "$s")"; done
ln -sf chromium-gpu "$PREFIX/chromium"; ln -sf vscode-gpu "$PREFIX/code"; ln -sf etcher-gpu "$PREFIX/etcher"
echo "installed to $PREFIX (make sure it is on PATH). Next: scripts/fetch-sysroot.sh"
