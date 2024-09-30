# select build image
FROM rust:1.81 as build

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

#RUN DEBIAN=NONINTERACTIVE apt update; apt install -y libssl-dev libpq-dev cron ; rm -rf /var/lib/apt/lists/*

# set the startup command to run your binary
ENTRYPOINT bash -c '/usr/local/bin/wayback-rpki serve --bootstrap'
