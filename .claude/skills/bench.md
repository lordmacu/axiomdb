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

# Primera vez — build images + arrancar los 3 containers
./setup.sh

# Correr escenarios específicos (los 3 en paralelo)
python3 bench.py full_scan
python3 bench.py insert_batch count_star
python3 bench.py --all
python3 bench.py --all --rows 100000
python3 bench.py --list    # ver escenarios disponibles

# Después de cambiar código de AxiomDB — rebuild y restart solo AxiomDB
docker compose build axiomdb
docker compose up -d axiomdb

# Parar todo
./teardown.sh
```

## Cómo funciona internamente

```
bench.py (host)
  ├── docker exec axiomdb_bench_mysql   python3 /bench/bench.py --scenario X  ┐
  ├── docker exec axiomdb_bench_pg      python3 /bench/bench.py --scenario X  ├── en paralelo
  └── docker exec axiomdb_bench_axiomdb /bench/axiomdb_bench  --scenario X   ┘
       ↓
  Cada container ejecuta contra su propio localhost (sin overhead de red)
  Output: JSON → bench.py lo recoge y muestra tabla comparativa
```

El binario de AxiomDB **se compila dentro del container de AxiomDB** (Linux nativo).
Cuando hay cambios de código: `docker compose build axiomdb` recompila todo para Linux.

## Available scenarios

| Scenario | What it measures | AxiomDB status |
|---|---|---|
| `insert_batch` | N rows in 1 txn | ✅ 6.4× faster than MySQL |
| `insert_autocommit` | 1 txn per row | — |
| `full_scan` | SELECT * heap scan | ⚠️ slower than MySQL inside Docker (mmap page cache pressure) |
| `select_where` | SELECT WHERE active=TRUE (~50%) | ❌ full scan, no index |
| `point_lookup` | 100 PK lookups | ❌ full scan until Phase 5 |
| `range_scan` | WHERE id BETWEEN X AND Y | ❌ full scan until Phase 5 |
| `count_star` | SELECT COUNT(*) | ❌ full scan until Phase 5 |
| `group_by` | GROUP BY age + COUNT(*) | ❌ full scan until Phase 5 |

## Benchmark results — 3 Dockers, 10K rows, fsync ON, 2CPU/2GB (2026-03-23)

| Scenario | MySQL 8.0 | PostgreSQL 16 | AxiomDB | Veredicto |
|---|---|---|---|---|
| `insert_batch` | 1,980/s | 26,875/s | **12,672/s** | AxiomDB **6.4× > MySQL** ✅ |
| `full_scan` | 267K/s | 2,258K/s | 141K/s | ⚠️ Docker mmap pressure |
| `select_where` | 263K/s | 1,759K/s | 51K/s | ❌ full scan |
| `count_star` | 3,102/s | 4,739/s | 21/s | ❌ full scan |
| `group_by` | 300/s | 479/s | 23/s | ❌ full scan |

**Note:** full_scan nativo (sin Docker) = 616K/s → ganaba a MySQL (267K/s). El Docker con 2GB limita el page cache del OS que usa mmap.

## Known issues and which phase fixes them

| Problem | Root cause | Fixed in |
|---|---|---|
| Point lookup, range scan, COUNT, GROUP BY muy lentos | No query planner — full scan en vez de B+Tree index | **Phase 5** |
| full_scan más lento que MySQL DENTRO de Docker | mmap depende del OS page cache; Docker con 2GB lo presiona | Phase 5 (buffer pool propio) o aumentar RAM del container |
| INSERT: parse+analyze overhead por cada fila | Cada SQL string se parsea individualmente | **Phase 8** (prepared statements / wire protocol) |
| AxiomDB no conecta vía red todavía | Sin wire protocol | **Phase 8** |
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
