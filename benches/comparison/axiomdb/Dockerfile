FROM rust:latest AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /build

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
