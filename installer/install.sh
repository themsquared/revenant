#!/bin/sh
# revenant installer — the agent that comes back.
#   curl -fsSL https://raw.githubusercontent.com/themsquared/revenant/main/installer/install.sh | sh
#
# Detects OS/arch, fetches the pinned revenant binaries into ~/.revenant/bin,
# verifies the checksum, and puts `revenant` on your PATH. It does NOT prompt for
# anything (stdin is the pipe) — the guided setup (keys, gateway, first chat)
# happens when you run `revenant`, in your real terminal.
set -eu

REPO="themsquared/revenant"
BIN_DIR="${REVENANT_HOME:-$HOME/.revenant}/bin"
LOCAL_BIN="$HOME/.local/bin"

# Colour only when writing to a terminal (not when piped to a file/log).
if [ -t 1 ]; then
  P='\033[1;35m'; G='\033[1;32m'; Y='\033[1;33m'; R='\033[1;31m'; B='\033[1m'; D='\033[2m'; X='\033[0m'
else
  P=''; G=''; Y=''; R=''; B=''; D=''; X=''
fi
line() { printf '%b\n' "$1"; }
step() { printf '  %b▸%b %b\n' "$P" "$X" "$1"; }
ok()   { printf '  %b✓%b %b\n' "$G" "$X" "$1"; }
warn() { printf '  %b!%b %b\n' "$Y" "$X" "$1"; }
die()  { printf '\n  %b✗ %s%b\n\n' "$R" "$1" "$X" >&2; exit 1; }

line ""
line "  ${P}🜁  R E V E N A N T${X}"
line "  ${D}the agent that comes back${X}"
line ""

# --- detect platform ---------------------------------------------------------
os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
  Darwin) case "$arch" in
            arm64|aarch64) triple="aarch64-apple-darwin" ;;
            *) die "unsupported macOS arch '$arch' — only Apple Silicon is prebuilt (build from source)." ;;
          esac ;;
  Linux)  case "$arch" in
            x86_64|amd64)  triple="x86_64-unknown-linux-musl" ;;
            aarch64|arm64) triple="aarch64-unknown-linux-musl" ;;
            *) die "unsupported Linux arch '$arch'." ;;
          esac ;;
  *) die "unsupported OS '$os' — try the container image." ;;
esac
step "platform    ${B}$triple${X}"

# --- resolve version ---------------------------------------------------------
VERSION="${REVENANT_VERSION:-latest}"
if [ "$VERSION" = "latest" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
    | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
  [ -n "$VERSION" ] || die "couldn't reach GitHub to resolve the latest release (offline? rate-limited? set REVENANT_VERSION)."
fi
step "version     ${B}$VERSION${X}"
[ -x "$BIN_DIR/revenant" ] && step "${D}(upgrading an existing install)${X}"

base="https://github.com/$REPO/releases/download/$VERSION"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
tarball="revenant-$VERSION-$triple.tar.gz"

# --- download ----------------------------------------------------------------
step "downloading ${D}$tarball${X}"
curl -fSL --progress-bar "$base/$tarball" -o "$tmp/$tarball" || die "download failed — is $VERSION built for $triple yet?"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "couldn't fetch SHA256SUMS."

# --- verify (never trust a byte we didn't checksum) --------------------------
( cd "$tmp" && grep " $tarball\$" SHA256SUMS | (
    if command -v sha256sum >/dev/null 2>&1; then sha256sum -c - >/dev/null
    else shasum -a 256 -c - >/dev/null; fi
  ) ) || die "checksum verification FAILED — refusing to install. Try again or report this."
ok "checksum verified"

# --- install -----------------------------------------------------------------
mkdir -p "$BIN_DIR"
tar -xzf "$tmp/$tarball" -C "$BIN_DIR"
chmod +x "$BIN_DIR/revenant" "$BIN_DIR/revenant-tui" 2>/dev/null || true
ok "installed → ${D}$BIN_DIR${X}"

# --- put it on PATH ----------------------------------------------------------
on_path=0
case ":$PATH:" in *":$LOCAL_BIN:"*) on_path=1 ;; esac
linked=0
if [ "$on_path" = "1" ]; then
  mkdir -p "$LOCAL_BIN"
  ln -sf "$BIN_DIR/revenant" "$LOCAL_BIN/revenant"
  ln -sf "$BIN_DIR/revenant-tui" "$LOCAL_BIN/revenant-tui"
  ok "linked → ${D}$LOCAL_BIN/revenant${X}"
  linked=1
fi

# --- done: hand off to the guided first run ----------------------------------
line ""
line "  ${G}${B}✓ Revenant is on your machine.${X}"
line ""
if [ "$linked" = "1" ]; then
  line "  ${B}Next — run it:${X}"
  line ""
  line "      ${P}revenant${X}"
else
  # Not on PATH: give the exact one-liner for their shell, then the run command.
  rc="$HOME/.profile"
  case "${SHELL:-}" in *zsh) rc="$HOME/.zshrc" ;; *bash) rc="$HOME/.bashrc" ;; esac
  warn "${LOCAL_BIN} isn't on your PATH yet."
  line ""
  line "  ${B}Add it, then run revenant:${X}"
  line ""
  line "      ${P}echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> $rc${X}"
  line "      ${P}. $rc && revenant${X}"
  line ""
  line "  ${D}(or run it directly: $BIN_DIR/revenant)${X}"
fi
line ""
line "  ${D}A one-time guided setup — how it'll think, then you're chatting.${X}"
line "  ${D}No config files, no docs.  ·  https://revenantai.dev${X}"
line ""
