#!/usr/bin/env bash
# Drive the dockerized horde: build + start the services, seed a molt, have each
# peer reproduce it on its own box, write a Vault scroll, and print the quorum.
set -uo pipefail
cd "$(dirname "$0")"
dc() { docker compose "$@"; }

echo "── building + starting the horde (necropolis + mock + 3 peers)…"
dc up -d --build

echo "── waiting for the network…"
for _ in $(seq 1 90); do dc exec -T peer1 curl -fsS -m2 http://necropolis:8080/health >/dev/null 2>&1 && break; sleep 1; done
for p in peer1 peer2 peer3; do
  for _ in $(seq 1 90); do
    dc exec -T "$p" sh -c 'curl -fsS -m2 -H "authorization: Bearer $(cat $REVENANT_HOME/token)" http://127.0.0.1:7717/v1/health' >/dev/null 2>&1 && { echo "  $p up"; break; }
    sleep 1
  done
done

echo "── peer1 publishes a molt (payload = an eval suite)"
MOLT=$(dc exec -T peer1 revenant net publish improvement /suite.json "latency-molt" | awk '{print $NF}' | tr -d '\r')
echo "  molt: $MOLT"

echo "── each peer reproduces it independently"
for p in peer1 peer2 peer3; do dc exec -T "$p" revenant reproduce "$MOLT"; done

echo "── peer2 inscribes a Scroll to the Vault"
dc exec -T peer2 revenant net scroll "Reproduced latency-molt — 1/1 pass on my box." "$MOLT"

echo "── QUORUM"
dc exec -T peer1 revenant net reproductions "$MOLT"
echo "── VAULT FEED"
dc exec -T peer1 revenant net feed

echo
echo "teardown with:  docker compose -f $(pwd)/docker-compose.yml down"
