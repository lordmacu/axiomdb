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

## Local Fair Benchmark

For apples-to-apples local comparisons across MariaDB, MySQL, and AxiomDB use
`local_bench.py`:

```bash
python3 benches/comparison/local_bench.py --scenario all --rows 50000 --table
```

Fairness rules in this harness:
- Timed `INSERT` paths avoid `executemany()`, because Python drivers batch it differently.
- The same table shape is created on every engine.
- Optional secondary indexes are created identically via `--indexes active,age,score`.
- Point lookups and range scans use deterministic key sets on all engines.
- PostgreSQL is available later via `--engines mariadb,mysql,axiomdb,postgres`.

Useful variants:

```bash
# Compare scan vs indexed filter under the same schema
python3 benches/comparison/local_bench.py --scenario select_where --rows 50000 --table
python3 benches/comparison/local_bench.py --scenario select_where --rows 50000 --indexes active --table

# Compare three write paths fairly
python3 benches/comparison/local_bench.py --scenario insert --rows 50000 --table
python3 benches/comparison/local_bench.py --scenario insert_multi_values --rows 50000 --table
python3 benches/comparison/local_bench.py --scenario insert_autocommit --rows 5000 --table

# Stress indexed reads and range updates
python3 benches/comparison/local_bench.py --scenario select_pk --rows 50000 --point-lookups 1000 --table
python3 benches/comparison/local_bench.py --scenario update_range --rows 50000 --range-rows 5000 --table

# Add PostgreSQL later when needed
python3 benches/comparison/local_bench.py --scenario all --rows 50000 --engines mariadb,mysql,axiomdb,postgres --table
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

### `local_bench.py` scenarios

| Scenario | What it measures |
|---|---|
| `insert` | N single-row `INSERT`s inside one explicit transaction |
| `insert_multi_values` | Bulk ingest using chunked `INSERT ... VALUES (...),(...)` |
| `insert_autocommit` | One `INSERT` per transaction |
| `select` | Full table scan |
| `select_where` | Filtered scan or indexed filter, depending on `--indexes` |
| `select_pk` | Primary-key point lookups |
| `select_range` | Primary-key range scan |
| `count` | Scalar aggregation |
| `aggregate` | `GROUP BY age, AVG(score)` aggregation |
| `update` | Batch update over `active = TRUE` |
| `update_range` | Batch update over a primary-key range |
| `delete` | Full-table delete |
| `delete_where` | Large selective delete (`id > N/2`) |

### `local_bench.py` knobs

| Flag | Purpose |
|---|---|
| `--engines mariadb,mysql,axiomdb[,postgres]` | Choose which engines participate in the comparison |
| `--indexes active,age,score` | Add identical secondary indexes to every engine |
| `--point-lookups N` | Number of PK lookups in `select_pk` |
| `--range-rows N` | Rows touched by `select_range` / `update_range` |
| `--multi-values-chunk N` | Rows per multi-values `INSERT` statement |
| `--autocommit-rows N` | Rows used by `insert_autocommit` |

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
