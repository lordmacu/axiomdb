#!/bin/bash
# Start MySQL and PostgreSQL benchmark containers.
# AxiomDB runs natively — no Docker needed until Phase 8.
#
# Usage:
#   ./setup.sh          # start MySQL + PostgreSQL
#   ./setup.sh --all    # start MySQL + PostgreSQL + AxiomDB (Phase 8+)

set -euo pipefail
cd "$(dirname "$0")"

PROFILE="db"
if [[ "${1:-}" == "--all" ]]; then
    PROFILE="all"
    echo "Starting MySQL + PostgreSQL + AxiomDB..."
else
    echo "Starting MySQL + PostgreSQL (AxiomDB runs natively)..."
fi

docker compose --profile "$PROFILE" up -d

echo ""
echo "Waiting for databases to be ready..."

# Wait for MySQL
echo -n "MySQL  "
for i in $(seq 1 40); do
    if docker exec axiomdb_bench_mysql mysqladmin ping -h 127.0.0.1 -uroot -pbench \
        --silent 2>/dev/null; then
        echo " ready ✓"
        break
    fi
    echo -n "."
    sleep 2
done

# Wait for PostgreSQL
echo -n "PostgreSQL "
for i in $(seq 1 40); do
    if docker exec axiomdb_bench_pg pg_isready -U postgres --quiet 2>/dev/null; then
        echo " ready ✓"
        break
    fi
    echo -n "."
    sleep 2
done

echo ""
echo "=== Benchmark databases ready ==="
echo ""
echo "  MySQL:      mysql://root:bench@127.0.0.1:3310/bench"
echo "  PostgreSQL: postgresql://postgres:bench@127.0.0.1:5433/bench"
if [[ "$PROFILE" == "all" ]]; then
echo "  AxiomDB:    axiomdb://root:bench@127.0.0.1:3311/bench  (Phase 8+)"
fi
echo ""
echo "  Resource limits per container: 2 CPU cores, 2 GB RAM"
echo "  Durability: full fsync ON (honest comparison mode)"
echo ""
echo "To run benchmarks:  python3 bench_runner.py"
echo "To stop:            ./teardown.sh"
