# Lean image for the standalone Necropolis directory (the horde's muster point).
# Builds ONLY the revenant-net `necropolis` bin, not the whole harness.
#
# NOTE: not yet validated against a live Fly.io deploy — run `fly launch` /
# `fly deploy` from deploy/necropolis and confirm before relying on it.
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p revenant-net --bin necropolis

FROM debian:stable-slim
RUN useradd -m revenant && mkdir -p /data && chown revenant /data
COPY --from=build /src/target/release/necropolis /usr/local/bin/necropolis
USER revenant
ENV PORT=8080 NECROPOLIS_DB=/data/necropolis.db
EXPOSE 8080
CMD ["necropolis"]
