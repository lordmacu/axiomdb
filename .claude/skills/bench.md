# /bench — Measure performance correctly

Never optimize without measuring. Never merge without verifying there was no regression.

---

## Two types of benchmarks

### 1. Micro-benchmarks (Criterion) — component-level, always available

```bash
cargo bench --bench btree          # B+ Tree: lookup, range scan, insert
cargo bench --bench parser_comparison  # Parser vs sqlparser-rs
cargo bench --bench row_codec      # Codec encode/decode throughput
cargo bench --bench storage        # MmapStorage page I/O
```

Baseline workflow:
```bash
cargo bench --bench btree -- --save-baseline before
# make change
cargo bench --bench btree -- --baseline before
```

### 2. Comparison benchmarks (3 Dockers) — end-to-end, MySQL vs PG vs AxiomDB

Infrastructure at `benches/comparison/`. Three Docker containers with **identical** resource limits (2 CPU, 2 GB RAM, fsync ON):

```
MySQL 8.0     → port 3310  (Docker, via pymysql)
PostgreSQL 16 → port 5433  (Docker, via psycopg2)
AxiomDB       → port 3311  (Docker, via docker exec + JSON until Phase 8 wire protocol)
```

---

## Running comparison benchmarks

```bash
cd benches/comparison

# Start containers (first time or after teardown)
./setup.sh          # MySQL + PostgreSQL only (AxiomDB runs natively for now)
./setup.sh --all    # + AxiomDB container (Phase 8+)

# Run specific scenarios
python3 bench.py insert_batch
python3 bench.py point_lookup range_scan
python3 bench.py --all
python3 bench.py --all --rows 100000

# Run AxiomDB native benchmark (until Phase 8)
cargo run --release -p axiomdb-bench-comparison -- --rows 10000

# Stop containers
./teardown.sh
```

## Available scenarios

| Scenario | What it measures | Known issue |
|---|---|---|
| `insert_batch` | N rows in 1 txn | AxiomDB: uses DROP+CREATE between runs (not DELETE) |
| `insert_autocommit` | 1 txn per row | — |
| `full_scan` | SELECT * (heap scan throughput) | AxiomDB already wins 3× vs MySQL |
| `select_where` | SELECT WHERE active=TRUE (~50%) | — |
| `point_lookup` | 100 PK lookups | AxiomDB: full scan until Phase 5 query planner |
| `range_scan` | WHERE id BETWEEN X AND Y | AxiomDB: full scan until Phase 5 |
| `count_star` | SELECT COUNT(*) | AxiomDB: full scan until Phase 5 |
| `group_by` | GROUP BY age + AVG(score) | — |

## Known issues and which phase fixes them

| Problem | Root cause | Fixed in |
|---|---|---|
| Point lookup 192× slower than MySQL | No query planner — full scan instead of B+Tree index | Phase 5 |
| Range scan 38× slower | Same — no index scan | Phase 5 |
| COUNT(*) 17× slower | Same | Phase 5 |
| INSERT 6× slower than MySQL | Parse+analyze overhead per row + DELETE scan | Phase 5 (bulk insert) + benchmark fix |
| AxiomDB needs docker exec | No wire protocol yet | Phase 8 |

## Benchmark results so far (2026-03-23, 10K rows, fsync ON)

| Benchmark | MySQL 8.0 | PostgreSQL 16 | AxiomDB (native) |
|---|---|---|---|
| INSERT batch 10K | 260ms / 38K/s | 1130ms / 8.8K/s | 1577ms / 6.3K/s* |
| SELECT * full scan | 49ms / 203K/s | 9ms / 1.1M/s | **16ms / 616K/s ✅** |
| SELECT WHERE ~50% | 27ms / 374K/s | 4ms / 2.3M/s | 16ms / 312K/s |
| Point lookup ×100 | 10ms / 10K/s | 5ms / 21K/s | 1923ms / 52/s* |
| Range scan 10% | 5ms / 2.1M/s | 1ms / 18M/s | 18ms / 55K/s* |
| COUNT(*) | 1ms | 0.4ms | 17ms* |
| GROUP BY + AVG | 2ms | 1ms | 15ms |

*These numbers will improve dramatically with Phase 5 (query planner + index scan).
INSERT will improve with Phase 8 (wire protocol prepared statements).

## AxiomDB advantages already visible

- **Full scan** — 3× faster than MySQL, 40% slower than PostgreSQL
- **B+Tree point lookup** (cargo bench) — 147ns = 6.8M ops/s, 9× over target
- **Parser** — 9.3× faster than sqlparser-rs
- **Row codec** — 25M rows/s encode, 11.5M rows/s decode

## Performance budget (do not regress)

| Operation | Target | Max acceptable |
|---|---|---|
| Point lookup PK (with index, Phase 5+) | 800K ops/s | 600K ops/s |
| Range scan 10K rows | 45ms | 60ms |
| INSERT with WAL | 180K ops/s | 150K ops/s |
| Seq scan 1M rows | 0.8s | 1.2s |
| Parser simple SELECT | — (553ns) | — |
| Row codec encode | — (25M rows/s) | — |
