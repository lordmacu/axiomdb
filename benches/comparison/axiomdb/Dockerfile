FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /bench /data
WORKDIR /data
# Binary is copied at runtime by bench.py via docker cp
# Container stays alive; bench.py calls: docker exec ... /bench/axiomdb_bench --scenario X
CMD ["sleep", "infinity"]
