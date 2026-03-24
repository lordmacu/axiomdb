# AxiomDB — Guía de benchmarks por fase

Qué medir al cerrar cada fase. Solo los benchmarks relevantes para lo que se implementó.
No correr todos — correr los específicos de la fase en cuestión.

---

## Cómo correr cada tipo

```bash
# Criterion (micro-benchmarks)
cargo bench --bench <nombre> -p <crate>

# Nativo sin wire (axiomdb_bench)
./target/release/axiomdb_bench --scenario <scenario> --rows <N>

# Wire protocol vs MariaDB/MySQL/PG (local_bench.py)
python3 benches/comparison/local_bench.py --scenario <scenario> --rows <N> --table

# Diagnóstico detallado
./target/release/axiomdb_bench --diagnose --rows <N>
```

---

## Fase 1 — Storage (heap, páginas, free list)

**Al cerrar:** cambios en page format, heap chain, free list, mmap.

```bash
cargo bench --bench storage -p axiomdb-storage
```

| Benchmark | Qué mide | Target |
|---|---|---|
| `memory/alloc_page` | Latencia de alloc_page() | < 1µs |
| `memory/write_read` | Throughput page read+write | > 1 GB/s |
| `mmap/sequential_scan` | Scan de N páginas secuenciales | > 2 GB/s |

---

## Fase 2 — B+ Tree

**Al cerrar:** cambios en índices, rebalance, lookup, range scan.

```bash
cargo bench --bench btree -p axiomdb-index
```

| Benchmark | Qué mide | Target |
|---|---|---|
| `point_lookup/1M` | Lookup por clave en árbol de 1M entries | > 800K ops/s |
| `range_scan/10K` | Scan de 10K entradas consecutivas | < 1ms |
| `insert/sequential` | Inserción ordenada (peor caso splits) | > 200K ops/s |
| `insert/random` | Inserción aleatoria (caso real) | > 150K ops/s |

---

## Fase 3 — WAL + MVCC + TxnManager

**Al cerrar:** cambios en WAL entries, crash recovery, record_insert/delete/truncate.

```bash
cargo bench --bench executor_e2e -p axiomdb-sql
# Específicamente:
cargo bench --bench executor_e2e -- insert_sequential
```

| Benchmark | Qué mide | Target |
|---|---|---|
| `insert_sequential/axiomdb_mmap_wal` | INSERT con WAL fsync | > 100K rows/s |
| Durability tests | 5 crash recovery tests pasan | ✅ |

**Wire (si Fase 5 ya está):**
```bash
python3 benches/comparison/local_bench.py --scenario insert --rows 10000 --table
```

---

## Fase 4 — SQL Parser + Executor

**Al cerrar subfases específicas:**

### 4.2 Lexer/Parser
```bash
cargo bench --bench sql_components -p axiomdb-sql
cargo bench --bench parser_comparison -p axiomdb-sql
```

| Benchmark | Qué mide | Target |
|---|---|---|
| `lexer/simple_select` | Tokenizar SELECT simple | < 200ns |
| `parser/simple_select` | Parse SELECT simple | < 500ns |
| `parser/complex_select` | Parse SELECT con JOINs y subqueries | < 3µs |

### 4.0 Row Codec
```bash
cargo bench --bench row_codec -p axiomdb-types
```

| Benchmark | Qué mide | Target |
|---|---|---|
| `codec/encode/user_row_7cols` | Encode fila típica | < 50ns |
| `codec/decode/user_row_7cols` | Decode fila típica | < 100ns |

### 4.5 Executor básico (INSERT/SELECT/UPDATE/DELETE)
```bash
./target/release/axiomdb_bench --scenario insert_batch --rows 10000
./target/release/axiomdb_bench --scenario full_scan --rows 10000
./target/release/axiomdb_bench --diagnose --rows 10000
```

| Benchmark | Qué mide | Target |
|---|---|---|
| `insert_batch/10K` (nativo) | Throughput INSERT batch sin wire | > 150K rows/s |
| `full_scan/10K` | Throughput SELECT * heap scan | > 1M rows/s |
| diagnose parse_ns | Overhead parse por query | < 1µs |
| diagnose analyze_ns | Overhead analyze (catalog scan) | < 5µs |

### 4.16c Multi-row INSERT
```bash
./target/release/axiomdb_bench --scenario insert_batch --rows 10000
# Comparar single-row vs multi-row
```

| Benchmark | Antes (N strings) | Target (1 string) |
|---|---|---|
| INSERT 10K rows nativo | ~35K rows/s | > 150K rows/s |

---

## Fase 5 — Wire Protocol

**Al cerrar:** cambios en COM_QUERY, handshake, result set encoding.

```bash
# Arrancar servidor primero
AXIOMDB_PORT=3309 AXIOMDB_DATA=/tmp/axiomdb_bench ./target/release/axiomdb-server &

# Benchmark completo via wire
python3 benches/comparison/local_bench.py --scenario all --rows 10000 --table
```

| Scenario | MariaDB ref | AxiomDB target |
|---|---|---|
| `insert` 10K | ~100K r/s | > 30K r/s (parse overhead) |
| `select` 10K | ~200K r/s | > 150K r/s |
| `select_where` 5K | ~180K r/s | > 150K r/s |
| `update` 5K | ~400K r/s | > 200K r/s |
| `delete` 10K | ~500K r/s | > 1M r/s (WalEntry::Truncate) |
| `delete_where` 5K | ~600K r/s | > 500K r/s |

**Solo si se modifica el parser o SchemaCache:**
```bash
./target/release/axiomdb_bench --diagnose --rows 10000
```

---

## Fase 6 — Índices secundarios + FK

**Al cerrar:** CREATE INDEX, index scan planner, index maintenance.

```bash
# Benchmark de índice en lookup puntual
python3 benches/comparison/local_bench.py --scenario select_where --rows 100000 --table
# Antes (sin índice): full scan O(N)
# Después (con índice en col): O(log N) — esperado 50-100× mejora

# Point lookup con índice
./target/release/axiomdb_bench --scenario point_lookup --rows 100000
```

| Benchmark | Sin índice | Con índice | Target |
|---|---|---|---|
| `point_lookup` 100K rows | ~50ms (full scan) | < 1ms (B-tree) | > 10K lookups/s |
| `select_where` 100K rows | ~500ms | < 10ms | > 10× mejora |
| `delete WHERE id = X` | ~500ms | < 5ms | equivalente a MariaDB |
| `update WHERE id = X` | ~500ms | < 5ms | equivalente a MariaDB |

---

## Fase 7 — MVCC completo + VACUUM + Group Commit

**Al cerrar:** snapshot isolation, VACUUM de slots muertos, group commit.

```bash
# INSERT throughput mejora con group commit
python3 benches/comparison/local_bench.py --scenario insert --rows 50000 --table

# VACUUM: verificar que espacio se recupera
# Insertar 10K filas, eliminar 5K, medir tamaño del archivo antes/después VACUUM
```

| Benchmark | Antes (Fase 5) | Target (post group commit) |
|---|---|---|
| `insert` 10K wire | ~267ms | < 100ms (amortized fsync) |
| Concurrencia 16 lectores | N/A | < 1.5× degradación vs 1 lector |

---

## Fase 8 — Query optimizer con estadísticas

**Al cerrar:** selectividad, EXPLAIN, planner mejorado.

```bash
# Verificar que el planner elige el plan correcto
# SELECT WHERE sin índice vs con índice: planner debe seleccionar index scan
python3 benches/comparison/local_bench.py --scenario select_where --rows 100000 --table
python3 benches/comparison/local_bench.py --scenario point_lookup --rows 100000 --table
```

| Benchmark | Target |
|---|---|
| `point_lookup` 100K | < 0.5ms (index always chosen) |
| EXPLAIN output | Shows "IndexScan" not "SeqScan" |

---

## Fase 9 — Hash join + Sort-merge join

**Al cerrar:** nuevos algoritmos de join.

```bash
# Benchmark específico de JOIN (actualmente O(N×M), después O(N+M))
python3 -c "
# Script de benchmark JOIN: ver docs/benchmark-guide.md para el script completo
# 10K users × 50K orders, INNER JOIN ON user_id
"
```

| Benchmark | Antes (nested loop) | Target (hash join) |
|---|---|---|
| INNER JOIN 500×2500 | ~66ms | < 5ms |
| INNER JOIN 10K×50K | > 5000ms | < 50ms |
| JOIN vs MariaDB ratio | 38× más lento | < 2× diferencia |

---

## Benchmarks de regresión — siempre al cerrar cualquier fase

Estos deben correr en TODA fase para detectar regresiones:

```bash
# 1. Tests — must always pass
cargo test --workspace

# 2. Micro regresiones (rápido, < 30s)
cargo bench --bench sql_components -- parser/simple_select
cargo bench --bench btree -- point_lookup/100K
cargo bench --bench row_codec -- encode

# 3. Wire regresión (si se tocó executor, WAL o wire)
python3 benches/comparison/local_bench.py --scenario select --rows 10000
```

**Thresholds de regresión (blocker si se excede > 10%):**

| Métrica | Valor actual | Máx aceptable |
|---|---|---|
| Parser simple SELECT | ~500ns | 600ns |
| Parser complex SELECT | ~2.7µs | 3.5µs |
| Row codec encode | ~30ns | 40ns |
| B+ Tree point lookup 1M | > 800K ops/s | 600K ops/s |
| SELECT wire 10K | ~50ms | 70ms |
| UPDATE wire 5K | ~22ms | 30ms |
| DELETE wire 10K | ~7ms | 15ms |

---

## Script completo de benchmark de JOIN (para Fase 9)

```python
# Añadir a benches/comparison/local_bench.py cuando se implemente hash join:
# Scenario: "join_inner" — 10K users × 50K orders, INNER JOIN ON user_id
# Setup: CREATE INDEX idx_orders_user ON bench_orders (user_id)
# Query: SELECT u.id, COUNT(o.id) FROM bench_users u
#        JOIN bench_orders o ON u.id = o.user_id
#        WHERE u.active = TRUE GROUP BY u.id
```

---

## Historial de benchmarks conocidos

| Fecha | Fase | Métrica | Valor |
|---|---|---|---|
| 2026-03-24 | 4.16b | INSERT nativo 10K | 211K rows/s |
| 2026-03-24 | 4.16c | INSERT wire 10K | ~37K rows/s |
| 2026-03-24 | research | UPDATE wire 5K | 22ms · 424K r/s (= MariaDB) |
| 2026-03-24 | research | DELETE wire 10K | 7ms · 1.37M r/s (2.8× MariaDB) |
| 2026-03-24 | research | DELETE WHERE 5K | 8ms · 601K r/s (2.6× MariaDB) |
| 2026-03-24 | research | JOIN INNER 500×2500 | 66ms (38× más lento que MariaDB) |
| 2026-03-24 | research | WAL PageWrite compact | 820KB → 20KB por 10K batch |
