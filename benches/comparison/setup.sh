#!/bin/bash
# Build and start all three benchmark containers.
# Each container includes its own benchmark tooling.
set -euo pipefail
cd "$(dirname "$0")"

echo "Building images..."
docker compose build

echo "Starting containers..."
docker compose up -d

echo ""
echo "Waiting for databases..."

echo -n "  MySQL  "
for i in $(seq 1 40); do
    docker exec axiomdb_bench_mysql mysqladmin ping -h 127.0.0.1 -uroot -pbench \
        --silent 2>/dev/null && echo " ready ✓" && break
    echo -n "."; sleep 2
done

echo -n "  PostgreSQL  "
for i in $(seq 1 40); do
    docker exec axiomdb_bench_pg pg_isready -U postgres --quiet 2>/dev/null \
        && echo " ready ✓" && break
    echo -n "."; sleep 2
done

echo "  AxiomDB  ready ✓  (binary copied at benchmark time)"

echo ""
echo "=== All containers running ==="
echo ""
echo "  Run benchmarks:"
echo "    python3 bench.py                   # interactive"
echo "    python3 bench.py full_scan         # specific scenario"
echo "    python3 bench.py --all             # all scenarios"
echo "    python3 bench.py --list            # see all scenarios"
echo ""
echo "  Stop: ./teardown.sh"
