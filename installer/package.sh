#!/bin/sh
# Package a revenant release tarball. Used by CI and testable locally.
#
#   installer/package.sh <version> <target-triple> [--zig]
#
# Builds the web UI (embedded into the binary), compiles release binaries for
# the target, and produces dist/revenant-<version>-<triple>.tar.gz. With
# --zig, cross-compiles via cargo-zigbuild (CI path for musl/linux).
set -eu

VERSION="${1:?usage: package.sh <version> <triple> [--zig]}"
TRIPLE="${2:?usage: package.sh <version> <triple> [--zig]}"
USE_ZIG="${3:-}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
DIST="$ROOT/dist"
mkdir -p "$DIST"

say() { printf '\033[1;35mpackage\033[0m %s\n' "$1"; }

# 1. Build the web UI so rust-embed bakes it into the binary.
if [ -d web ]; then
  say "building web UI"
  ( cd web && npm ci --silent && npm run build >/dev/null )
  [ -f web/dist/index.html ] || { echo "web build produced no dist/index.html" >&2; exit 1; }
fi

# 2. Compile release binaries.
if [ "$USE_ZIG" = "--zig" ]; then
  say "cargo zigbuild --release --target $TRIPLE"
  cargo zigbuild --release --target "$TRIPLE" --bin revenant --bin revenant-tui
else
  say "cargo build --release --target $TRIPLE"
  rustup target add "$TRIPLE" >/dev/null 2>&1 || true
  cargo build --release --target "$TRIPLE" --bin revenant --bin revenant-tui
fi

BIN_DIR="target/$TRIPLE/release"
[ -x "$BIN_DIR/revenant" ] || { echo "missing $BIN_DIR/revenant" >&2; exit 1; }

# 3. Stage + tar (flat layout: the installer extracts binaries directly).
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp "$BIN_DIR/revenant" "$BIN_DIR/revenant-tui" "$STAGE/"
cp README.md LICENSE "$STAGE/" 2>/dev/null || true

TARBALL="revenant-$VERSION-$TRIPLE.tar.gz"
say "packaging $TARBALL"
tar -czf "$DIST/$TARBALL" -C "$STAGE" .

# 4. Per-file checksum line (aggregated into SHA256SUMS by the release job).
( cd "$DIST" && (sha256sum "$TARBALL" 2>/dev/null || shasum -a 256 "$TARBALL") > "$TARBALL.sha256" )
say "done: dist/$TARBALL"
