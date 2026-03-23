# AxiomDB Comparison Benchmarks

Measures AxiomDB vs MySQL 8.0 vs PostgreSQL 16 under identical conditions.

## Architecture

```
MySQL 8.0    ─── Docker (2 CPU, 2 GB) ─── port 3310
PostgreSQL 16 ── Docker (2 CPU, 2 GB) ─── port 5433
AxiomDB      ─── Docker (2 CPU, 2 GB) ─── port 3311  ← Phase 8+
```

All three use **full durability** (fsync ON):
- MySQL:      `innodb_flush_log_at_trx_commit=1`, `sync_binlog=1`
- PostgreSQL: `fsync=on`, `synchronous_commit=on`
- AxiomDB:    `fsync=true` in axiomdb.toml

## Quick Start

```bash
# 1. Start MySQL + PostgreSQL
./setup.sh

# 2. Install Python dependencies (one time)
pip install pymysql psycopg2-binary

# 3. Run benchmarks (MySQL + PostgreSQL)
python3 bench_runner.py

# 4. Run with more rows
python3 bench_runner.py --rows 100000

# 5. Stop and clean up
./teardown.sh
```

## Including AxiomDB (Phase 8+)

Once Phase 8 (MySQL wire protocol) is complete:

```bash
# Build AxiomDB image
docker compose build axiomdb

# Start all three
./setup.sh --all

# Run full comparison
python3 bench_runner.py --all
```

## Benchmarks included

| Benchmark | What it measures |
|---|---|
| `insert_batch_Nk` | Bulk insert — 1 transaction, N rows |
| `insert_autocommit_500` | Worst-case durability — 1 txn per row |
| `select_* full scan` | Sequential heap read throughput |
| `select_where active=1` | Filtered scan (~50% selectivity) |
| `point_lookup PK × 100` | 100 index lookups by primary key |
| `range_scan 10%` | Contiguous range by id |
| `count(*)` | Aggregation: full scan + count |
| `group by age + avg(score)` | Hash aggregation |

## Configuration files

| File | Purpose |
|---|---|
| `conf/mysql/my.cnf` | MySQL tuned config (buffer pool, fsync, O_DIRECT) |
| `conf/postgres/postgresql.conf` | PostgreSQL tuned config (shared_buffers, WAL) |
| `conf/axiomdb/axiomdb.toml` | AxiomDB benchmark config (Phase 8+) |
| `docker-compose.yml` | All three containers with equal resource limits |

## Changing dataset size

```bash
python3 bench_runner.py --rows 1000    # 1K rows (quick smoke test)
python3 bench_runner.py --rows 10000   # 10K rows (default)
python3 bench_runner.py --rows 100000  # 100K rows (thorough)
```
