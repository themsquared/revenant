#!/bin/sh
# revenant installer — the agent that comes back.
#   curl -fsSL https://raw.githubusercontent.com/themsquared/revenant/main/installer/install.sh | sh
#
# Detects OS/arch, fetches the pinned revenant + agentgateway binaries into
# ~/.revenant/bin, verifies checksums, and runs `revenant init`. Keys and the
# embedding model are handled by init, not this script.
set -eu

REPO="themsquared/revenant"
BIN_DIR="${REVENANT_HOME:-$HOME/.revenant}/bin"
LOCAL_BIN="$HOME/.local/bin"

say() { printf '\033[1;35mrevenant\033[0m %s\n' "$1"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

# --- detect platform ---
os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
  Darwin) case "$arch" in
            arm64|aarch64) triple="aarch64-apple-darwin" ;;
            *) die "unsupported macOS arch: $arch (only Apple Silicon prebuilt; build from source)" ;;
          esac ;;
  Linux)  case "$arch" in
            x86_64|amd64)  triple="x86_64-unknown-linux-musl" ;;
            aarch64|arm64) triple="aarch64-unknown-linux-musl" ;;
            *) die "unsupported Linux arch: $arch" ;;
          esac ;;
  *) die "unsupported OS: $os (try the container image)" ;;
esac
say "platform: $triple"

# --- resolve version ---
VERSION="${REVENANT_VERSION:-latest}"
if [ "$VERSION" = "latest" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
  [ -n "$VERSION" ] || die "could not resolve latest release (set REVENANT_VERSION)"
fi
say "version: $VERSION"

base="https://github.com/$REPO/releases/download/$VERSION"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fetch() { # <name>
  say "downloading $1"
  curl -fSL --progress-bar "$base/$1" -o "$tmp/$1" || die "download failed: $1"
}

tarball="revenant-$VERSION-$triple.tar.gz"
fetch "$tarball"
fetch "SHA256SUMS"

# --- verify checksum ---
say "verifying checksum"
( cd "$tmp" && grep " $tarball\$" SHA256SUMS | (
    if command -v sha256sum >/dev/null 2>&1; then sha256sum -c -;
    else shasum -a 256 -c -; fi
  ) ) || die "checksum verification failed"

# --- install ---
mkdir -p "$BIN_DIR"
tar -xzf "$tmp/$tarball" -C "$BIN_DIR"
chmod +x "$BIN_DIR/revenant" "$BIN_DIR/revenant-tui" 2>/dev/null || true
say "installed to $BIN_DIR"

# Symlink onto PATH if ~/.local/bin is available.
if printf '%s' "$PATH" | grep -q "$LOCAL_BIN"; then
  mkdir -p "$LOCAL_BIN"
  ln -sf "$BIN_DIR/revenant" "$LOCAL_BIN/revenant"
  ln -sf "$BIN_DIR/revenant-tui" "$LOCAL_BIN/revenant-tui"
  say "linked into $LOCAL_BIN"
else
  say "add $BIN_DIR to your PATH:  export PATH=\"$BIN_DIR:\$PATH\""
fi

# --- init (fetches agentgateway + embedding model, prompts for keys) ---
say "running setup…"
"$BIN_DIR/revenant" init

cat <<EOF

$(say "done.")
  revenant chat            # talk to it
  revenant service install # run it always-on
  revenant open            # web UI
EOF
