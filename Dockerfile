# select build image
FROM rust:1.85 as build

# create a new empty shell project
RUN USER=root cargo new --bin my_project
WORKDIR /my_project

# copy your source tree
COPY ./src ./src
COPY ./Cargo.toml .

# build for release
RUN cargo build --release


# our final base
FROM debian:bookworm-slim

# copy the build artifact from the build stage
COPY --from=build /my_project/target/release/wayback-rpki /usr/local/bin/wayback-rpki

WORKDIR /wayback-rpki

ENTRYPOINT bash -c '/usr/local/bin/wayback-rpki serve --bootstrap --host 0.0.0.0 --port 40065'
