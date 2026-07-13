#!/usr/bin/env bash
# rollout.sh — this machine is the canary. Every update to `main` gets built,
# tested, and smoke-booted in isolation HERE; only if all gates pass does it go
# live (with automatic rollback if the live restart doesn't come up healthy).
#
#   scripts/rollout.sh            # roll out only if origin/main advanced
#   scripts/rollout.sh --force    # rebuild + redeploy current HEAD regardless
#
# Safe to run on a timer: it takes a lock, no-ops when there's nothing new, and
# never leaves the daemon down — a failed gate leaves the running binary
# untouched, and a failed live restart rolls back to the previous one.
set -euo pipefail

REPO="${REVENANT_REPO:-$HOME/revenant}"
RHOME="${REVENANT_HOME:-$HOME/.revenant}"
BIN_DIR="$RHOME/bin"
LOG="$RHOME/logs/rollout.log"
SERVICE="dev.revenant.agent"
LOCKDIR="$RHOME/rollout.lock.d"
FORCE="${1:-}"

# Self-sufficient PATH: launchd runs with a minimal environment, so name every
# toolchain dir explicitly (cargo, node/nvm, homebrew, system).
NODE_BIN="$(ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null | sort -V | tail -1)"
export PATH="$HOME/.cargo/bin:${NODE_BIN:-}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"
mkdir -p "$RHOME/logs" "$BIN_DIR"

# --- single-flight lock -------------------------------------------------------
# `mkdir` is atomic on POSIX (works on macOS, unlike flock), so it's the lock.
# If the holder is dead (crashed without cleanup), reclaim the stale lock so a
# one-off crash can't wedge the canary forever. Concurrency matters here: the
# 300s timer must never race a manual --force run onto the fixed smoke ports.
if ! mkdir "$LOCKDIR" 2>/dev/null; then
  oldpid="$(cat "$LOCKDIR/pid" 2>/dev/null || echo)"
  if [ -n "$oldpid" ] && kill -0 "$oldpid" 2>/dev/null; then
    echo "$(date '+%F %T') | rollout skipped — held by pid $oldpid" >>"$LOG"; exit 0
  fi
  echo "$(date '+%F %T') | reclaiming stale rollout lock (pid ${oldpid:-?} gone)" >>"$LOG"
  rm -rf "$LOCKDIR"
  mkdir "$LOCKDIR" 2>/dev/null || { echo "$(date '+%F %T') | lock race lost — skip" >>"$LOG"; exit 0; }
fi
echo "$$" >"$LOCKDIR/pid"
trap 'rm -rf "$LOCKDIR"' EXIT

exec >>"$LOG" 2>&1
say() { echo "$(date '+%F %T') | $*"; }
fail() { say "ROLLOUT ABORTED: $*"; exit 1; }
say "=== rollout start (force=${FORCE:-no}) ==="

cd "$REPO"

# --- 1. fetch + fast-forward main; converge the DEPLOYED binary to HEAD ------
# The baseline is what's actually deployed (the marker), not local HEAD — so if
# the running binary drifts behind HEAD, we still redeploy. Self-healing.
git fetch --quiet origin main || fail "git fetch failed"
git merge --ff-only origin/main 2>/dev/null || true   # no-op if already at/ahead
target="$(git rev-parse HEAD)"
deployed="$(cat "$RHOME/deployed-sha" 2>/dev/null || echo none)"
if [ "$target" = "$deployed" ] && [ "$FORCE" != "--force" ]; then
  say "deployed already at ${target:0:12} — nothing to roll out"; exit 0
fi
say "rolling out ${deployed:0:12} -> ${target:0:12}"

# --- 2. build web only when its sources changed since the deployed sha -------
web_changed=1
if [ "$FORCE" != "--force" ] && git cat-file -e "$deployed" 2>/dev/null; then
  git diff --quiet "$deployed" "$target" -- web/src web/index.html web/package.json 2>/dev/null && web_changed=0
fi
if [ "$web_changed" = "1" ]; then
  say "rebuilding web dist"
  ( cd web && npm ci --silent && npm run build >/dev/null ) || fail "web build failed"
fi

# --- 3. compile + logic gates (a failure here never touches the live binary) -
say "cargo build --release"
cargo build --release || fail "build failed"
say "cargo test --release"
cargo test --release --quiet || fail "tests failed"

# --- 4. isolated smoke boot on alt ports (live daemon untouched) -------------
SB="$(mktemp -d)"; trap 'rm -rf "$SB" "$LOCKDIR"' EXIT
SBH="$SB/home"; mkdir -p "$SBH/gateway/bin" "$SBH/models" "$SBH/logs"
ln -sf "$RHOME/gateway/bin/"agentgateway-* "$SBH/gateway/bin/" 2>/dev/null || true
for m in "$RHOME/models/"*; do [ -e "$m" ] && ln -sf "$m" "$SBH/models/"; done
cp "$RHOME/secrets.env" "$SBH/secrets.env" 2>/dev/null || true
REVENANT_HOME="$SBH" ./target/release/revenant init </dev/null >/dev/null 2>&1 || true
python3 - "$SBH/config.toml" <<'PY'
import sys, re
p = sys.argv[1]; s = open(p).read()
for k, v in [("llm_port", 42001), ("readiness_port", 19901), ("stats_port", 19902),
             ("mcp_port", 42002), ("admin_port", 15900)]:
    s = re.sub(rf'{k} = \d+', f'{k} = {v}', s)
# never let the canary smoke test auto-update or phone the network
s = re.sub(r'auto = "\w+"', 'auto = "off"', s)
open(p, 'w').write(s)
PY

say "smoke: booting new binary on alt ports"
REVENANT_HOME="$SBH" REVENANT_BIND="127.0.0.1:7817" \
  ./target/release/revenant up >"$SBH/logs/up.log" 2>&1 &
SPID=$!
smoke_ok=0
for i in $(seq 1 60); do
  if ! kill -0 "$SPID" 2>/dev/null; then say "smoke: daemon exited early"; break; fi
  tok="$(cat "$SBH/token" 2>/dev/null || true)"
  if [ -n "$tok" ] && curl -fsS -m 3 -H "authorization: Bearer $tok" \
        "http://127.0.0.1:7817/v1/health" >/dev/null 2>&1 \
     && curl -fsS -m 3 "http://127.0.0.1:7817/" >/dev/null 2>&1; then
    smoke_ok=1; say "smoke: healthy after $((i))x0.5s (control + web UI)"; break
  fi
  sleep 0.5
done
kill -TERM "$SPID" 2>/dev/null || true
wait "$SPID" 2>/dev/null || true
# free the alt gateway port before we continue
for i in $(seq 1 20); do lsof -nP -iTCP:42001 -sTCP:LISTEN >/dev/null 2>&1 || break; sleep 0.3; done
[ "$smoke_ok" = "1" ] || { echo "--- smoke log ---"; tail -20 "$SBH/logs/up.log" 2>/dev/null; fail "smoke boot did not become healthy"; }

# --- 5. deploy: back up, swap, restart the live service, verify, rollback ----
tag="$(git rev-parse --short HEAD)"
cp -f "$BIN_DIR/revenant" "$BIN_DIR/revenant.bak" 2>/dev/null || true
cp -f target/release/revenant "$BIN_DIR/revenant"
[ -f target/release/revenant-tui ] && cp -f target/release/revenant-tui "$BIN_DIR/revenant-tui"

# Keep the PATH binary pointed at the installed one — otherwise the CLI you
# type (`revenant …`) drifts behind the canary-updated daemon. Re-assert the
# symlink each deploy (it can get replaced by a stale copy over time).
for lb in "$HOME/.local/bin/revenant" "$HOME/.local/bin/revenant-tui"; do
  target="$BIN_DIR/$(basename "$lb")"
  if [ -e "$target" ] && [ ! -L "$lb" -o "$(readlink "$lb" 2>/dev/null)" != "$target" ]; then
    ln -sf "$target" "$lb"
  fi
done

restart_live() {
  if launchctl print "gui/$(id -u)/$SERVICE" >/dev/null 2>&1; then
    launchctl kickstart -k "gui/$(id -u)/$SERVICE"
  else
    # foreground/nohup fallback: stop the old one, start the new detached.
    pkill -TERM -f "revenant up" 2>/dev/null || true
    for i in $(seq 1 20); do lsof -nP -iTCP:41001 -sTCP:LISTEN >/dev/null 2>&1 || break; sleep 0.5; done
    ( cd "$REPO" && nohup "$BIN_DIR/revenant" up >"$RHOME/logs/daemon.out.log" 2>&1 & )
  fi
}

live_healthy() {
  local tok; tok="$(cat "$RHOME/token" 2>/dev/null || true)"
  [ -n "$tok" ] || return 1
  curl -fsS -m 3 -H "authorization: Bearer $tok" "http://127.0.0.1:7717/v1/health" >/dev/null 2>&1
}

say "deploying $tag → restarting live daemon"
restart_live
deployed=0
for i in $(seq 1 60); do live_healthy && { deployed=1; break; }; sleep 0.5; done

if [ "$deployed" = "1" ]; then
  echo "$tag" >"$RHOME/deployed-sha"
  say "=== rollout OK: live on $tag ==="
else
  say "live restart did NOT become healthy — ROLLING BACK"
  if [ -f "$BIN_DIR/revenant.bak" ]; then
    cp -f "$BIN_DIR/revenant.bak" "$BIN_DIR/revenant"
    restart_live
    for i in $(seq 1 60); do live_healthy && break; sleep 0.5; done
    say "rolled back to previous binary"
  fi
  fail "deploy failed; previous binary restored"
fi
