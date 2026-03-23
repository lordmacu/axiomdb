# AxiomDB benchmark image — builds the benchmark binary for Linux inside Docker.
# Build context must be the workspace root (../../):
#   docker compose build axiomdb
#
# Uses cargo-chef for dependency layer caching: when only source files change
# (not Cargo.toml / Cargo.lock), the compiled-dependencies layer is reused.

FROM rust:1.82-slim-bookworm AS deps-base
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-chef --locked
WORKDIR /build

FROM deps-base AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY benches/comparison/axiomdb_bench/ benches/comparison/axiomdb_bench/
RUN cargo chef prepare --recipe-path recipe.json -p axiomdb-bench-comparison

FROM deps-base AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release -p axiomdb-bench-comparison --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY benches/comparison/axiomdb_bench/ benches/comparison/axiomdb_bench/
RUN cargo build --release -p axiomdb-bench-comparison

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /bench /data
COPY --from=builder /build/target/release/axiomdb_bench /bench/axiomdb_bench
WORKDIR /data
CMD ["sleep", "infinity"]
