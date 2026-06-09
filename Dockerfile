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
RUN cargo build --release --bin pangolin-gateway-controller --bin badger-ext-authz-shim \
 && strip target/release/pangolin-gateway-controller target/release/badger-ext-authz-shim

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder --chown=65532:65532 --chmod=0555 \
    /work/target/release/pangolin-gateway-controller \
    /usr/local/bin/pangolin-gateway-controller
# The badger ext-authz shim ships in the same image; run it by overriding the
# container command (see deploy/badger-shim.yaml).
COPY --from=builder --chown=65532:65532 --chmod=0555 \
    /work/target/release/badger-ext-authz-shim \
    /usr/local/bin/badger-ext-authz-shim
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/pangolin-gateway-controller"]
