# Performance

AxiomDB is designed to outperform MySQL on specific workloads by eliminating several
layers of redundant work: double-buffering, the double-write buffer, row-by-row query
evaluation, and thread-per-connection overhead. This page presents current benchmark
numbers and guidance on how to write queries and schemas that stay fast.

---

## Benchmark Results

All benchmarks run on Apple M2 Pro (12 cores), 32 GB RAM, NVMe SSD, single-threaded,
warm data (all pages in OS page cache unless noted).

### SQL Parser Throughput

| Query type            | AxiomDB (logos lexer) | MySQL ~  | PostgreSQL ~ | Ratio vs MySQL |
|-----------------------|-----------------------|----------|--------------|----------------|
| Simple SELECT (1 tbl) | **492 ns**            | ~500 ns  | ~450 ns      | 1.0× (parity)  |
| Complex SELECT (JOINs)| **2.7 µs**            | ~4.0 µs  | ~3.5 µs      | 1.5× faster    |
| DDL (CREATE TABLE)    | **1.1 µs**            | ~2.5 µs  | ~2.0 µs      | 2.3× faster    |
| Batch (100 stmts)     | **47 µs**             | ~90 µs   | ~75 µs       | 1.9× faster    |

Compared to `sqlparser-rs` (the common Rust SQL parser library):

| Query type            | AxiomDB   | sqlparser-rs | Ratio         |
|-----------------------|-----------|--------------|---------------|
| Simple SELECT         | 492 ns    | 4.8 µs       | **9.8× faster** |
| Complex SELECT        | 2.7 µs    | 46 µs        | **17× faster**  |

The speed advantage comes from two decisions:
1. **logos DFA lexer** — compiles the token patterns to a Deterministic Finite Automaton
   at compile time. Token scanning is O(n) with a very small constant.
2. **Zero-copy tokens** — `Ident` and `QuotedIdent` tokens are `&'src str` slices into
   the original input. No heap allocation occurs during lexing.

### Storage Engine Throughput

| Operation                 | AxiomDB       | Target       | Max acceptable | Status |
|---------------------------|---------------|--------------|----------------|--------|
| B+ Tree point lookup (1M) | **1.2M ops/s**| 800K ops/s   | 600K ops/s     | ✅     |
| Range scan 10K rows       | **0.61 ms**   | 45 ms        | 60 ms          | ✅     |
| B+ Tree INSERT (storage only) | **195K ops/s** | 180K ops/s | 150K ops/s  | ✅     |
| Sequential scan 1M rows   | **0.72 s**    | 0.8 s        | 1.2 s          | ✅     |
| Concurrent reads ×16      | **linear**    | linear       | <2× degradation| ✅     |

### Wire Protocol Throughput (Phase 5.14)

End-to-end throughput measured via the MySQL wire protocol (pymysql client, autocommit
mode, 1 connection, localhost). Includes: network round-trip, protocol encode/decode,
parse, analyze, execute, WAL, MmapStorage.

| Operation                         | Throughput       | Notes                                      |
|-----------------------------------|------------------|--------------------------------------------|
| COM_PING                          | **24,865 pings/s** | Pure protocol overhead baseline           |
| SET NAMES (intercepted)           | **46,672 q/s**   | Handled in protocol layer, no SQL engine   |
| SELECT 1 (autocommit)             | **185 q/s**      | Full SQL pipeline, read-only               |
| INSERT (autocommit, 1 fsync/stmt) | **58 q/s**       | Full SQL pipeline + fsync for durability   |

The 185 q/s SELECT result reflects a **3.3× improvement** in Phase 5.14 over the prior
56 q/s baseline. Read-only transactions (SELECT, SHOW, etc.) no longer fsync the WAL —
see [Benchmarks → Phase 5.14](../development/benchmarks.md#phase-514-wire-protocol) for
the technical explanation.

**Remaining bottlenecks:**
- SELECT: one full parse + analyze cycle per query (Phase 5.13 plan cache will amortize this)
- INSERT (single connection): one `fdatasync` per autocommit statement; enable Group Commit
  for concurrent workloads (see below)

### Group Commit — Concurrent Write Throughput (Phase 3.19)

With `group_commit_interval_ms = 0` (default), every DML commit fsyncs individually.
With Group Commit enabled, N concurrent connections share one fsync per batch window:

| Concurrency | group_commit disabled | group_commit_interval_ms=1 | Improvement |
|---|---|---|---|
| 1 connection  | 58 q/s (baseline) | ~57 q/s (+1ms latency)  | ~1× (no gain) |
| 4 connections | ~58 q/s (serialized) | ~200+ q/s (shared fsync) | ~4× |
| 8 connections | ~58 q/s             | ~400+ q/s                | ~8× |
| 16 connections| ~58 q/s             | ~800+ q/s                | ~16× |

*Theoretical upper bound — actual numbers depend on NVMe latency and connection overlap.*

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage vs MySQL InnoDB</span>
MySQL's <code>innodb_flush_log_at_trx_commit=1</code> (default, fully durable) also pays one fsync per transaction under low concurrency. MySQL's group commit kicks in automatically at high concurrency. AxiomDB's Group Commit is explicit and configurable, achieving the same batching effect without the InnoDB overhead of a separate undo tablespace write before each row mutation.
</div>
</div>

### End-to-End INSERT Throughput

Full pipeline: parse → analyze → execute → WAL → MmapStorage. Measured with
`executor_e2e` benchmark (MmapStorage + real WAL, release build, Apple M2 Pro NVMe).

| Configuration                                   | AxiomDB         | MariaDB ~   | Status |
|-------------------------------------------------|-----------------|-------------|--------|
| INSERT 10K rows / N separate SQL strings / 1 txn| 35K rows/s      | 140K rows/s | ⚠️     |
| **INSERT 10K rows / 1 multi-row SQL string**    | **211K rows/s** | 140K rows/s | ✅ **1.5× faster** |
| INSERT autocommit (1 fsync/stmt, wire protocol) | 58 q/s          | —           | — (Phase 5.14) |

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage vs MariaDB InnoDB</span>
With <code>INSERT INTO t VALUES (r1),(r2),...,(rN)</code>, AxiomDB reaches 211K rows/s
vs MariaDB's ~140K rows/s — <strong>1.5× faster</strong> on bulk inserts. The gap comes
from three combined optimizations: O(P) heap writes via <code>HeapChain::insert_batch</code>,
O(1) WAL writes via <code>record_insert_batch</code> (Phase 3.17), and a single
parse+analyze pass for all N rows (Phase 4.16c). MariaDB pays a clustered B-Tree insert
per row plus UNDO log write before each page modification.
</div>
</div>

**How to achieve this throughput in your application:**

```sql
-- Fast: one SQL string with N value rows (211K rows/s)
INSERT INTO orders (user_id, amount) VALUES
  (1, 49.99), (2, 12.50), (3, 99.00), -- ... up to thousands of rows
  (1000, 7.99);

-- Slower: N separate INSERT strings (35K rows/s — parse+analyze per row)
INSERT INTO orders VALUES (1, 49.99);
INSERT INTO orders VALUES (2, 12.50);
-- ...
```

The difference between the two approaches is 6× in throughput. The bottleneck
in the per-string case is parse + analyze overhead per SQL string (~20 µs/string),
not the storage write.

### Row Codec Throughput

| Operation     | Throughput         | Notes                               |
|---------------|--------------------|-------------------------------------|
| Encode row    | **33M rows/s**     | 5-column row, mixed types           |
| Decode row    | **28M rows/s**     | Same row layout                     |
| encoded_len() | **O(n) no alloc**  | Only computes the size, no buffer   |

Row encoding is fast because:
- The codec iterates values once with a fixed dispatch per type.
- The null bitmap is written as bytes with bit shifts — no per-column branch on NULL.
- Variable-length types (Text, Bytes) use a 3-byte length prefix that avoids the
  4-byte overhead of a full u32.

---

## Why AxiomDB Is Fast — Architecture Reasons

### 1. No Double-Buffering

MySQL InnoDB maintains its own Buffer Pool in addition to the OS page cache.
The same data lives in RAM twice.

```
MySQL:   Disk → OS page cache → InnoDB Buffer Pool → Query
                (copy 1)            (copy 2)

AxiomDB: Disk → OS page cache → Query
                (mmap — single copy)
```

AxiomDB uses `mmap` to map the `.db` file directly. The OS page cache IS the
buffer. When a page is hot, it is served from L2/L3 cache with zero copies.

### 2. No Double-Write Buffer

MySQL writes each 16 KB page to a special "doublewrite buffer" area on disk before
writing it to its actual location. This prevents torn-page corruption but costs two
disk writes per page.

AxiomDB uses a WAL + per-page CRC32c checksum. The WAL record is small (tens of bytes
for the changed key-value pair). On recovery, AxiomDB replays the WAL to reconstruct
any page that has a checksum mismatch. No doublewrite buffer needed.

### 3. Lock-Free Concurrent Reads

The Copy-on-Write B+ Tree uses an `AtomicU64` to store the root page ID. Readers
load the root pointer with `Acquire` semantics and traverse the tree without acquiring
any lock. Writers swap the root pointer with `Release` semantics after finishing the
copy chain.

A running `SELECT` does not stall any `INSERT` or `UPDATE`. Both proceed in parallel.

### 4. Async I/O with Tokio

The server mode uses Tokio async I/O. 1,000 concurrent connections run on approximately
8 OS threads. MySQL's thread-per-connection model requires 1,000 OS threads for 1,000
connections, consuming ~8 GB in stack space alone.

---

## Performance Budget

The following table defines the minimum acceptable performance for each critical
operation. Benchmarks that fall below the "acceptable maximum" column are treated as
blockers before any phase is closed.

| Operation                               | Target        | Acceptable maximum  |
|-----------------------------------------|---------------|---------------------|
| Point lookup (PK)                       | 800K ops/s    | 600K ops/s          |
| Range scan 10K rows                     | 45 ms         | 60 ms               |
| B+ Tree INSERT with WAL (storage only)  | 180K ops/s    | 150K ops/s          |
| INSERT end-to-end 10K batch (Phase 8)   | 180K ops/s    | 150K ops/s          |
| SELECT via wire protocol (autocommit)   | —             | —                   |
| INSERT via wire protocol (autocommit)   | —             | —                   |
| Sequential scan 1M rows                 | 0.8 s         | 1.2 s               |
| Concurrent reads ×16                    | linear        | <2× degradation     |
| Parser (simple SELECT)                  | 600 ns        | 1 µs                |
| Parser (complex SELECT)                 | 3 µs          | 6 µs                |

---

## Index Usage Guide

### Rules of Thumb

1. **Every foreign key column needs an index** — AxiomDB does not auto-index FK
   columns. Without an index, every FK check during DELETE/UPDATE scans the child
   table linearly.

2. **Put the most selective column first in composite indexes** — A query filtering
   `WHERE user_id = 42 AND status = 'paid'` benefits most from `(user_id, status)`
   if `user_id` is more selective (fewer distinct values match).

3. **Covering indexes eliminate heap lookups** — If all columns in a SELECT are in
   the index, AxiomDB returns results directly from the index without touching heap
   pages.

4. **Partial indexes reduce size** — `CREATE INDEX ... WHERE deleted_at IS NULL`
   indexes only active rows. If 90% of rows are soft-deleted, the partial index is
   10× smaller than a full index.

5. **BIGINT AUTO_INCREMENT beats UUID v4 for PK** — UUID v4 inserts at random
   positions in the B+ Tree, causing ~40% more page splits than sequential integers.
   Use UUID v7 if you need UUIDs (time-sortable prefix).

---

## Query Patterns to Avoid

### Unindexed range scans on large tables

```sql
-- Slow: scans every row in orders (no index on placed_at)
SELECT * FROM orders WHERE placed_at > '2026-01-01';

-- Fix: create the index
CREATE INDEX idx_orders_date ON orders (placed_at);
```

### Leading wildcard LIKE

```sql
-- Slow: cannot use index on 'name' (leading %)
SELECT * FROM users WHERE name LIKE '%smith%';

-- Better: full-text search index (planned Phase 8)
-- Acceptable workaround for small tables: use LOWER() + LIKE on indexed column
```

### SELECT * with wide rows

```sql
-- Fetches all columns including large TEXT blobs for every row
SELECT * FROM documents WHERE category_id = 5;

-- Better: select only what the UI needs
SELECT id, title, created_at FROM documents WHERE category_id = 5;
```

### NOT IN with nullable subquery

```sql
-- Returns 0 rows if the subquery contains a single NULL
SELECT * FROM orders WHERE user_id NOT IN (SELECT id FROM banned_users);

-- Fix: filter NULLs explicitly
SELECT * FROM orders WHERE user_id NOT IN (
    SELECT id FROM banned_users WHERE id IS NOT NULL
);
```

---

## Measuring Performance

### EXPLAIN (planned)

```sql
EXPLAIN SELECT * FROM orders WHERE user_id = 42 ORDER BY placed_at DESC;
```

### Running the Built-in Benchmarks

```bash
# B+ Tree benchmarks
cargo bench --bench btree -p axiomdb-index

# Storage engine benchmarks
cargo bench --bench storage -p axiomdb-storage

# Compare before/after an optimization
cargo bench -- --save-baseline before
# ... make change ...
cargo bench -- --baseline before
```

Benchmarks use Criterion.rs and report mean, standard deviation, and throughput
in a format compatible with `critcmp` for historical comparison.
