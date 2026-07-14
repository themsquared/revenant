#!/usr/bin/env bash
# Host smoke of the whole horde flow with the real binary, before dockerizing:
# a mock LLM, a Necropolis, and THREE independent revenant daemons that each
# reproduce a seeded molt and post to the quorum + Vault. Self-contained: starts
# everything, runs the flow, verifies, and tears down.
set -uo pipefail
cd "$(dirname "$0")"
REV=~/revenant/target/debug/revenant
NECRO=~/revenant-necropolis/target/debug/necropolis
MOCK_PORT=19099
NECRO_PORT=18899
BASE="http://127.0.0.1:$NECRO_PORT"
PEER_PORTS=(27811 27821 27831)
PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; pkill -9 -f mock_llm.py 2>/dev/null; rm -rf /tmp/hordepeer1 /tmp/hordepeer2 /tmp/hordepeer3; }
trap cleanup EXIT

echo "── starting mock LLM (:$MOCK_PORT) + Necropolis (:$NECRO_PORT)"
MOCK_PORT=$MOCK_PORT python3 mock_llm.py & PIDS+=($!)
NECROPOLIS_OPEN_PUBLISH=1 NECROPOLIS_DB=:memory: PORT=$NECRO_PORT "$NECRO" >/tmp/horde-necro.log 2>&1 & PIDS+=($!)
sleep 2
curl -fsS -m3 "$BASE/health" >/dev/null && echo "  necropolis up" || { echo "necropolis FAILED"; exit 1; }
curl -fsS -m3 "http://127.0.0.1:$MOCK_PORT/" >/dev/null && echo "  mock up" || { echo "mock FAILED"; exit 1; }

echo "── raising 3 peers (external gateway → mock, no keys)"
for idx in 0 1 2; do
  i=$((idx+1)); H=/tmp/hordepeer$i; mkdir -p "$H"
  sed -e "s#http://mock:9000#http://127.0.0.1:$MOCK_PORT#" \
      -e "s#http://necropolis:8080#http://127.0.0.1:$NECRO_PORT#" peer.config.toml > "$H/config.toml"
  python3 -c "import secrets;print(secrets.token_hex(32))" > "$H/token"  # control bearer (else `up` wants `init`)
  REVENANT_HOME="$H" REVENANT_BIND="127.0.0.1:${PEER_PORTS[$idx]}" nohup "$REV" up >"$H/up.log" 2>&1 & PIDS+=($!)
done
for idx in 0 1 2; do
  i=$((idx+1)); H=/tmp/hordepeer$i
  for t in $(seq 1 60); do
    tok=$(cat "$H/token" 2>/dev/null || true)
    [ -n "$tok" ] && curl -fsS -m2 -H "authorization: Bearer $tok" "http://127.0.0.1:${PEER_PORTS[$idx]}/v1/health" >/dev/null 2>&1 && { echo "  peer$i up"; break; }
    sleep 0.5
  done
done

echo "── peer1 publishes a molt (payload = eval suite)"
REVENANT_HOME=/tmp/hordepeer1 "$REV" net publish improvement suite.json "latency-molt" 2>&1 | sed 's/^/  /'
MOLT=$(curl -fsS -m3 "$BASE/artifacts" | python3 -c "import sys,json; a=json.load(sys.stdin); print(a[0]['id'] if a else '')")
[ -n "$MOLT" ] && echo "  molt id: ${MOLT:0:16}…" || { echo "  no molt published"; exit 1; }

echo "── each peer reproduces it on its own box"
for idx in 0 1 2; do
  i=$((idx+1)); H=/tmp/hordepeer$i
  REVENANT_HOME="$H" REVENANT_URL="http://127.0.0.1:${PEER_PORTS[$idx]}" "$REV" reproduce "$MOLT" 2>&1 | sed "s/^/  peer$i: /"
done

echo "── peer2 inscribes a Scroll to the Vault"
REVENANT_HOME=/tmp/hordepeer2 REVENANT_URL="http://127.0.0.1:${PEER_PORTS[1]}" "$REV" net scroll "Reproduced latency-molt — 1/1 pass on my box." "$MOLT" 2>&1 | sed 's/^/  /'

echo "── VERDICT"
REPS=$(curl -fsS -m3 "$BASE/artifacts/$MOLT/reproductions" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))")
SCROLLS=$(curl -fsS -m3 "$BASE/scrolls" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))")
echo "  reproductions on record: $REPS (quorum bar = 3)"
echo "  scrolls in the Vault:     $SCROLLS"
if [ "$REPS" -ge 3 ] && [ "$SCROLLS" -ge 1 ]; then echo "  ✅ HORDE QUORUM REACHED + VAULT WRITTEN"; else echo "  ❌ did not reach the bar"; fi
