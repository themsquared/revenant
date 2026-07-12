#!/usr/bin/env bash
# install-canary.sh — make THIS machine the canary:
#   1. run the daemon as a managed launchd service (survives reboot/crash,
#      restartable via `launchctl kickstart`),
#   2. install a timer that runs rollout.sh on an interval, so every update to
#      `main` is built, tested, and deployed HERE automatically (with rollback).
#
# Idempotent. macOS/launchd only (this is Mike's dev box).
set -euo pipefail

RHOME="${REVENANT_HOME:-$HOME/.revenant}"
BIN="$RHOME/bin/revenant"
REPO="${REVENANT_REPO:-$HOME/revenant}"
AGENTS="$HOME/Library/LaunchAgents"
ROLLOUT_LABEL="dev.revenant.rollout"
INTERVAL="${ROLLOUT_INTERVAL:-300}"

[ "$(uname -s)" = "Darwin" ] || { echo "launchd only (macOS); this box is the canary"; exit 1; }
[ -x "$BIN" ] || { echo "no installed binary at $BIN — run scripts/rollout.sh --force first"; exit 1; }

echo "▸ stopping any stray foreground/nohup daemon before installing the service"
pkill -TERM -f "revenant up" 2>/dev/null || true
for i in $(seq 1 20); do
  lsof -nP -iTCP:7717 -sTCP:LISTEN >/dev/null 2>&1 || lsof -nP -iTCP:41001 -sTCP:LISTEN >/dev/null 2>&1 || break
  sleep 0.5
done

echo "▸ installing the daemon as a launchd service (dev.revenant.agent)"
"$BIN" service install

echo "▸ installing the rollout timer ($ROLLOUT_LABEL, every ${INTERVAL}s)"
mkdir -p "$AGENTS" "$RHOME/logs"
PLIST="$AGENTS/$ROLLOUT_LABEL.plist"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$ROLLOUT_LABEL</string>
  <key>ProgramArguments</key>
  <array><string>/bin/bash</string><string>$REPO/scripts/rollout.sh</string></array>
  <key>StartInterval</key><integer>$INTERVAL</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$RHOME/logs/rollout.timer.log</string>
  <key>StandardErrorPath</key><string>$RHOME/logs/rollout.timer.log</string>
</dict></plist>
EOF
launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"

echo
echo "✓ canary is set up."
echo "  daemon:      launchd service dev.revenant.agent (auto-restart, survives reboot)"
echo "  rollout:     every ${INTERVAL}s — build + test + smoke + deploy (rollback on failure)"
echo "  rollout log: $RHOME/logs/rollout.log"
echo "  force now:   $REPO/scripts/rollout.sh --force"
echo "  stop timer:  launchctl unload $PLIST"
echo "  stop daemon: $BIN service uninstall"
