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
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release && \
    cp /app/target/release/wayback-rpki /usr/local/bin/wayback-rpki

# ------------------------------------------------------------------------------
# Runtime image
# ------------------------------------------------------------------------------
FROM debian:bookworm-slim
COPY --from=builder /usr/local/bin/wayback-rpki /usr/local/bin/wayback-rpki

WORKDIR /wayback-rpki

ENTRYPOINT ["/usr/local/bin/wayback-rpki", "serve", "--bootstrap", "--host", "0.0.0.0", "--port", "40065"]
