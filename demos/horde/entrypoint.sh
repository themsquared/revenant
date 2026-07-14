#!/bin/sh
# Peer entrypoint: drop in the external-mode config + a control token (so `up`
# doesn't demand `init`, which would download the embedding model), then run.
set -e
mkdir -p "$REVENANT_HOME"
cp -f /peer.config.toml "$REVENANT_HOME/config.toml"
[ -f "$REVENANT_HOME/token" ] || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$REVENANT_HOME/token"
exec revenant up
