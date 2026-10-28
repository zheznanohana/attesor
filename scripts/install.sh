#!/bin/bash
# Build attesor + the GPU bridge and install them into $PREFIX (default ~/bin), plus the app launcher
# (~/attesor-launcher, `attesor-apps`) and its login agent, which wraps arm64-only apps so they open on double-click.
# Files are replaced atomically (install -S), so apps already running under attesor keep working.
#   NO_LAUNCH_AGENT=1 scripts/install.sh            # install everything but the login agent
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
install -S -m 755 "$BIN" "$PREFIX/attesor"
for l in libmclbridge libmclmetal libattesorui libattesorexec; do install -S -m 755 "gpu/out/$l.dylib" "$PREFIX/gpu/"; done
for s in gpu/chromium-gpu scripts/vscode-gpu scripts/etcher-gpu; do install -S -m 755 "$s" "$PREFIX/$(basename "$s")"; done
ln -sf chromium-gpu "$PREFIX/chromium"; ln -sf vscode-gpu "$PREFIX/code"; ln -sf etcher-gpu "$PREFIX/etcher"

# app launcher
LAUNCHER=$HOME/attesor-launcher
mkdir -p "$LAUNCHER/bin" "$HOME/Library/Logs/Attesor"
install -S -m 755 launcher/attesor-apps launcher/fix-dock.py "$LAUNCHER/"
install -S -m 644 launcher/badge.swift "$LAUNCHER/"
swiftc -O launcher/badge.swift -o "$LAUNCHER/bin/badge.new" && mv -f "$LAUNCHER/bin/badge.new" "$LAUNCHER/bin/badge"
ln -sf "$LAUNCHER/attesor-apps" "$PREFIX/attesor-apps"

# login agent: rescans /Applications and ~/Applications (every 30 s) and wraps new arm64-only apps
AGENT=$HOME/Library/LaunchAgents/local.attesor-apps.plist
if [ -z "${NO_LAUNCH_AGENT:-}" ]; then
  mkdir -p "$(dirname "$AGENT")"
  new=$(sed -e "s|@HOME@|$HOME|g" -e "s|@ATTESOR@|$PREFIX/attesor|g" launcher/local.attesor-apps.plist)
  if [ "$new" != "$(cat "$AGENT" 2>/dev/null)" ]; then
    printf '%s\n' "$new" > "$AGENT"
    launchctl bootout "gui/$(id -u)/local.attesor-apps" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$AGENT"
    echo "login agent local.attesor-apps (re)loaded"
  fi
fi
echo "installed to $PREFIX (make sure it is on PATH). Next: scripts/fetch-sysroot.sh"
