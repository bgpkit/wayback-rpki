# syntax=docker/dockerfile:1
FROM rust:1.90 AS chef
WORKDIR /app
RUN cargo install cargo-chef --locked

# ------------------------------------------------------------------------------
# Planner: extract dependency recipe from Cargo.toml + Cargo.lock + source
# structure. Does NOT compile.
# ------------------------------------------------------------------------------
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ------------------------------------------------------------------------------
# Builder: compile dependencies from recipe, then the real application source.
# The dependency-cook layer is only invalidated when the dependency tree changes.
# ------------------------------------------------------------------------------
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release && \
    cp /app/target/release/wayback-rpki /usr/local/bin/wayback-rpki

# ------------------------------------------------------------------------------
# Runtime image
# ------------------------------------------------------------------------------
FROM debian:trixie-slim

# curl is the health probe's HTTP client. The slim base image ships none, and a
# healthcheck that cannot execute reports the container unhealthy while the
# service is serving normally.
RUN apt-get update && \
    apt-get install -y --no-install-recommends curl && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/wayback-rpki /usr/local/bin/wayback-rpki

WORKDIR /wayback-rpki

# Startup loads the trie archive before the API answers, so failures only count
# after the start period.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://127.0.0.1:40065/health || exit 1

ENTRYPOINT ["/usr/local/bin/wayback-rpki", "serve", "--bootstrap", "--host", "0.0.0.0", "--port", "40065"]
