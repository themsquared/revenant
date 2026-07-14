#!/usr/bin/env bash
# Prove the quorum + Vault against the LIVE Necropolis (necropolis.revenantai.dev).
# Three agents (bound to the owner's verified account so production accepts their
# writes) publish a molt, each reproduce it, and inscribe a Scroll — appending
# real, permanent entries to the public hash-linked ledger.
#
# Eval is deterministic (mock LLM) so honest peers agree → quorum; the point
# here is the NETWORK protocol on the real horde, not a model's answer.
set -uo pipefail
cd "$(dirname "$0")"
REV=~/revenant/target/release/revenant
BASE=https://necropolis.revenantai.dev
ACCT=~/.revenant/account.key
MOCK_PORT=19099
PEER_PORTS=(27811 27821 27831)
TITLE="live-proof-molt-$(date -u +%Y%m%dT%H%M%SZ)"
PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; pkill -9 -f mock_llm.py 2>/dev/null; rm -rf /tmp/livepeer1 /tmp/livepeer2 /tmp/livepeer3; }
trap cleanup EXIT

[ -f "$ACCT" ] || { echo "no account.key — the owner's account must be verified first"; exit 1; }

echo "── live Necropolis head (before):"; curl -fsS -m6 "$BASE/ledger/head"; echo
echo "── mock LLM (:$MOCK_PORT)"; MOCK_PORT=$MOCK_PORT python3 mock_llm.py & PIDS+=($!); sleep 1

echo "── raising 3 agents, binding each to your account"
for idx in 0 1 2; do
  i=$((idx+1)); H=/tmp/livepeer$i; mkdir -p "$H"
  sed -e "s#http://mock:9000#http://127.0.0.1:$MOCK_PORT#" -e "s#http://necropolis:8080#$BASE#" peer.config.toml > "$H/config.toml"
  cp "$ACCT" "$H/account.key"
  python3 -c "import secrets;print(secrets.token_hex(32))" > "$H/token"
  REVENANT_HOME="$H" REVENANT_BIND="127.0.0.1:${PEER_PORTS[$idx]}" nohup "$REV" up >"$H/up.log" 2>&1 & PIDS+=($!)
done
for idx in 0 1 2; do
  i=$((idx+1)); H=/tmp/livepeer$i
  for _ in $(seq 1 60); do
    tok=$(cat "$H/token" 2>/dev/null || true)
    [ -n "$tok" ] && curl -fsS -m2 -H "authorization: Bearer $tok" "http://127.0.0.1:${PEER_PORTS[$idx]}/v1/health" >/dev/null 2>&1 && break
    sleep 0.5
  done
  REVENANT_HOME="$H" "$REV" net bind 2>&1 | sed "s/^/  peer$i: /"
done

echo "── peer1 publishes a molt to the live horde"
REVENANT_HOME=/tmp/livepeer1 "$REV" net publish improvement suite.json "$TITLE" 2>&1 | sed 's/^/  /'
MOLT=$(curl -fsS -m6 "$BASE/artifacts?kind=improvement" | python3 -c "import sys,json; a=[x for x in json.load(sys.stdin) if x.get('title')=='$TITLE']; print(a[0]['id'] if a else '')")
[ -n "$MOLT" ] && echo "  molt: $MOLT" || { echo "  molt not found on live (publish gated?)"; exit 1; }

echo "── each agent reproduces it and attests to the live ledger"
for idx in 0 1 2; do
  i=$((idx+1))
  REVENANT_HOME="/tmp/livepeer$i" REVENANT_URL="http://127.0.0.1:${PEER_PORTS[$idx]}" "$REV" reproduce "$MOLT" 2>&1 | sed "s/^/  peer$i: /"
done

echo "── peer2 inscribes a Scroll to the live Vault"
REVENANT_HOME=/tmp/livepeer2 REVENANT_URL="http://127.0.0.1:${PEER_PORTS[1]}" "$REV" net scroll "Live proof: reproduced $TITLE across 3 agents." "$MOLT" 2>&1 | sed 's/^/  /'

echo "── VERDICT (from the live Necropolis)"
REPS=$(curl -fsS -m6 "$BASE/artifacts/$MOLT/reproductions" | python3 -c "import sys,json; d=json.load(sys.stdin); print(sum(1 for a in d if a.get('reproduced')))")
echo "  verified reproductions on the live ledger: $REPS (quorum bar = 3)"
echo "── live ledger head (after):"; curl -fsS -m6 "$BASE/ledger/head"; echo
[ "$REPS" -ge 3 ] && echo "  ✅ QUORUM REACHED ON THE LIVE HORDE" || echo "  ❌ not yet at the bar"
