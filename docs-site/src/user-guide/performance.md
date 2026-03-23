# Performance

NexusDB is designed to outperform MySQL on specific workloads by eliminating several
layers of redundant work: double-buffering, the double-write buffer, row-by-row query
evaluation, and thread-per-connection overhead. This page presents current benchmark
numbers and guidance on how to write queries and schemas that stay fast.

---

## Benchmark Results

All benchmarks run on Apple M2 Pro (12 cores), 32 GB RAM, NVMe SSD, single-threaded,
warm data (all pages in OS page cache unless noted).

### SQL Parser Throughput

| Query type            | NexusDB (logos lexer) | MySQL ~  | PostgreSQL ~ | Ratio vs MySQL |
|-----------------------|-----------------------|----------|--------------|----------------|
| Simple SELECT (1 tbl) | **492 ns**            | ~500 ns  | ~450 ns      | 1.0× (parity)  |
| Complex SELECT (JOINs)| **2.7 µs**            | ~4.0 µs  | ~3.5 µs      | 1.5× faster    |
| DDL (CREATE TABLE)    | **1.1 µs**            | ~2.5 µs  | ~2.0 µs      | 2.3× faster    |
| Batch (100 stmts)     | **47 µs**             | ~90 µs   | ~75 µs       | 1.9× faster    |

Compared to `sqlparser-rs` (the common Rust SQL parser library):

| Query type            | NexusDB   | sqlparser-rs | Ratio         |
|-----------------------|-----------|--------------|---------------|
| Simple SELECT         | 492 ns    | 4.8 µs       | **9.8× faster** |
| Complex SELECT        | 2.7 µs    | 46 µs        | **17× faster**  |

The speed advantage comes from two decisions:
1. **logos DFA lexer** — compiles the token patterns to a Deterministic Finite Automaton
   at compile time. Token scanning is O(n) with a very small constant.
2. **Zero-copy tokens** — `Ident` and `QuotedIdent` tokens are `&'src str` slices into
   the original input. No heap allocation occurs during lexing.

### Storage Engine Throughput

| Operation                 | NexusDB       | Target       | Max acceptable | Status |
|---------------------------|---------------|--------------|----------------|--------|
| B+ Tree point lookup (1M) | **1.2M ops/s**| 800K ops/s   | 600K ops/s     | ✅     |
| Range scan 10K rows       | **0.61 ms**   | 45 ms        | 60 ms          | ✅     |
| INSERT with WAL           | **195K ops/s**| 180K ops/s   | 150K ops/s     | ✅     |
| Sequential scan 1M rows   | **0.72 s**    | 0.8 s        | 1.2 s          | ✅     |
| Concurrent reads ×16      | **linear**    | linear       | <2× degradation| ✅     |

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

## Why NexusDB Is Fast — Architecture Reasons

### 1. No Double-Buffering

MySQL InnoDB maintains its own Buffer Pool in addition to the OS page cache.
The same data lives in RAM twice.

```
MySQL:   Disk → OS page cache → InnoDB Buffer Pool → Query
                (copy 1)            (copy 2)

NexusDB: Disk → OS page cache → Query
                (mmap — single copy)
```

NexusDB uses `mmap` to map the `.db` file directly. The OS page cache IS the
buffer. When a page is hot, it is served from L2/L3 cache with zero copies.

### 2. No Double-Write Buffer

MySQL writes each 16 KB page to a special "doublewrite buffer" area on disk before
writing it to its actual location. This prevents torn-page corruption but costs two
disk writes per page.

NexusDB uses a WAL + per-page CRC32c checksum. The WAL record is small (tens of bytes
for the changed key-value pair). On recovery, NexusDB replays the WAL to reconstruct
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

| Operation               | Target        | Acceptable maximum  |
|-------------------------|---------------|---------------------|
| Point lookup (PK)       | 800K ops/s    | 600K ops/s          |
| Range scan 10K rows     | 45 ms         | 60 ms               |
| INSERT with WAL         | 180K ops/s    | 150K ops/s          |
| Sequential scan 1M rows | 0.8 s         | 1.2 s               |
| Concurrent reads ×16    | linear        | <2× degradation     |
| Parser (simple SELECT)  | 600 ns        | 1 µs                |
| Parser (complex SELECT) | 3 µs          | 6 µs                |

---

## Index Usage Guide

### Rules of Thumb

1. **Every foreign key column needs an index** — NexusDB does not auto-index FK
   columns. Without an index, every FK check during DELETE/UPDATE scans the child
   table linearly.

2. **Put the most selective column first in composite indexes** — A query filtering
   `WHERE user_id = 42 AND status = 'paid'` benefits most from `(user_id, status)`
   if `user_id` is more selective (fewer distinct values match).

3. **Covering indexes eliminate heap lookups** — If all columns in a SELECT are in
   the index, NexusDB returns results directly from the index without touching heap
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
cargo bench --bench btree -p nexusdb-index

# Storage engine benchmarks
cargo bench --bench storage -p nexusdb-storage

# Compare before/after an optimization
cargo bench -- --save-baseline before
# ... make change ...
cargo bench -- --baseline before
```

Benchmarks use Criterion.rs and report mean, standard deviation, and throughput
in a format compatible with `critcmp` for historical comparison.
