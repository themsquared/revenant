# Horde peer: build the revenant binary from the workspace, then run it in
# external-gateway mode (no provider keys) pointed at the deterministic mock
# LLM + the shared Necropolis. Build context is the repo root.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin revenant

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/revenant /usr/local/bin/revenant
COPY demos/horde/peer.config.toml /peer.config.toml
COPY demos/horde/suite.json /suite.json
COPY demos/horde/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENV REVENANT_HOME=/root/.revenant
ENTRYPOINT ["/entrypoint.sh"]
