# syntax=docker/dockerfile:1.7

# The base image's Rust version is just enough to bootstrap rustup; the actual
# toolchain (incl. version) comes from `rust-toolchain.toml`, so this stays
# stable as that file moves.
FROM rust:1-slim-bookworm AS chef
WORKDIR /work
RUN cargo install cargo-chef@0.1.71 --locked

FROM chef AS planner
COPY rust-toolchain.toml ./
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY tests ./tests
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY rust-toolchain.toml ./
COPY --from=planner /work/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release --bin pangolin-envoy-controller \
 && strip target/release/pangolin-envoy-controller

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /work/target/release/pangolin-envoy-controller /usr/local/bin/pangolin-envoy-controller
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/pangolin-envoy-controller"]
