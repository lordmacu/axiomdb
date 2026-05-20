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
| TOAST/BLOB read 128KB (storage only) | **20.355 us / 5.997 GiB/s** | no formal target | no formal max | ✅ |
| TOAST/BLOB incref+free shared 128KB (storage only) | **25.241 us / 39.618K ops/s** | no formal target | no formal max | ✅ |
| Sequential scan 1M rows   | **0.72 s**    | 0.8 s        | 1.2 s          | ✅     |
| Concurrent reads ×16      | **linear**    | linear       | <2× degradation| ✅     |

The TOAST/BLOB rows are Phase `11.2d` Criterion measurements over
`MemoryStorage`. They validate the refcounted overflow-chain path directly; full
SQL throughput still includes row codec, planner/executor, WAL, and wire costs.

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
- INSERT (single connection): one `fdatasync` per autocommit statement; enable Group Commit
  for concurrent workloads (see below)

### Primary-Key Lookups After `6.16`

Phase `6.16` removes the planner blind spot that still treated `WHERE id = ...`
as a scan on PK-only tables. The PRIMARY KEY B+Tree is now used for single-table
equality and range lookups.

Measured with `python3 benches/comparison/local_bench.py --scenario select_pk --rows 5000 --table`
on the same machine:

| Operation | MariaDB 12.1 | MySQL 8.0 | AxiomDB |
|---|---|---|---|
| `SELECT * FROM bench_users WHERE id = literal` | 12.7K lookups/s | 13.4K lookups/s | **11.1K lookups/s** |

The old debt was "planner never reaches the PK B+Tree". That is now closed.
The remaining gap is smaller and sits after planning: row materialization and
MySQL packet serialization still cost more than MariaDB/MySQL on this path.

### DELETE WHERE / UPDATE After `5.20`

Phase `5.19` removed the old-key delete bottleneck for `DELETE ... WHERE` and the
old-key half of `UPDATE`. Phase `5.20` finishes the real `UPDATE` fix for the
benchmark schema by preserving the heap `RecordId` when the new row fits in the
same slot, which makes selective index skipping correct.

Measured with `python3 benches/comparison/local_bench.py --scenario all --rows 50000 --table`
on the same machine:

| Operation | MariaDB 12.1 | MySQL 8.0 | AxiomDB | PostgreSQL 16 |
|---|---|---|---|---|
| `DELETE WHERE id > 25000` | 652K rows/s | 662K rows/s | **1.13M rows/s** | 3.76M rows/s |
| `UPDATE ... WHERE active = TRUE` | 662K rows/s | 404K rows/s | **648K rows/s** | 270K rows/s |

Compared to the `4.6K rows/s` pre-`5.19` DELETE-WHERE baseline that originally
triggered this work, AxiomDB now stays in the same order of magnitude as MySQL
and MariaDB on the same local benchmark. More importantly, compared to the
`52.9K rows/s` post-`5.19` / pre-`5.20` UPDATE baseline, the stable-RID path
raises AxiomDB UPDATE throughput to `648K rows/s` on the same 50K-row benchmark.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Faster Than MySQL On DELETE WHERE</span>
At 50K rows, AxiomDB `DELETE WHERE id > 25000` reaches <strong>1.13M rows/s</strong> vs
MySQL 8.0 at <strong>662K rows/s</strong>. The gain comes from eliminating the old
one-`delete_in(...)`-per-row loop and replacing it with one ordered batch delete per index.
</div>
</div>

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">12× UPDATE Gain</span>
`5.20` lifts AxiomDB `UPDATE ... WHERE active = TRUE` from <strong>52.9K rows/s</strong>
to <strong>648K rows/s</strong> by preserving heap `RecordId`s on same-slot rewrites and
skipping PK maintenance when only non-indexed columns change.
</div>
</div>

The main remaining write-path bottleneck is now `INSERT`, not `UPDATE`.

### Indexed `UPDATE ... WHERE` After `6.20`

Phase `6.17` removed the old full-scan candidate discovery path for indexed
UPDATE predicates. Phase `6.20` then removed the dominant apply-side costs on
the default PK-range benchmark: candidate heap reads are batched by page,
no-op rows skip physical mutation, stable-RID rewrites batch their WAL append,
and index maintenance only runs when a key, predicate membership, or RID really
changes.

Measured with `python3 benches/comparison/local_bench.py --scenario update_range --rows 5000 --table`
on the same machine:

| Operation | MariaDB 12.1 | MySQL 8.0 | AxiomDB |
|---|---|---|---|
| `UPDATE bench_users SET score = score + 1 WHERE id BETWEEN ...` | 618K rows/s | 291K rows/s | **369.9K rows/s** |

Compared to the `6.17` result (`85.2K rows/s`), the `6.20` apply fast path is a
`4.3x` improvement on the same benchmark and now exceeds the documented local
MySQL result. The remaining gap is specifically MariaDB's tighter clustered-row
update path, not AxiomDB's old discovery-side O(n) scan.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
On the default PK-only `update_range` benchmark, AxiomDB now reaches <strong>369.9K rows/s</strong> vs MySQL 8.0 at <strong>291K rows/s</strong> because `6.20` keeps the whole statement inside a batched heap/WAL apply path instead of paying per-row reads and per-row `UpdateInPlace` appends.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Reuse WAL Format</span>
MariaDB and PostgreSQL both optimize UPDATE by changing how batches are applied before inventing a new log record type. AxiomDB follows that rule here: `6.20` keeps the existing `UpdateInPlace` WAL format for rollback and recovery, but batches normal entries through one `reserve_lsns + write_batch` call per statement.
</div>
</div>

### INSERT in Explicit Transactions After `5.21`

Phase `5.21` adds transactional INSERT staging for consecutive
`INSERT ... VALUES` statements inside one explicit transaction. Instead of
writing heap + WAL + index roots per statement, AxiomDB now buffers eligible
rows and flushes them together on `COMMIT` or the next barrier statement.

Measured with `python3 benches/comparison/local_bench.py --scenario insert --rows 50000 --table`
on the same machine:

| Operation | MariaDB 12.1 | MySQL 8.0 | AxiomDB |
|---|---|---|---|
| `50K` single-row `INSERT`s in `1` explicit txn | 28.0K rows/s | 26.7K rows/s | **23.9K rows/s** |

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Stage, Then Flush</span>
PostgreSQL's <code>heap_multi_insert()</code> and DuckDB's appender both separate row
production from physical write. AxiomDB adapts that idea to SQL-visible transactions:
the connection keeps staged INSERT rows in memory, then flushes them in one grouped
heap/index pass when SQL semantics require visibility.
</div>
</div>

This path targets one specific workload: many separate INSERT statements inside
`BEGIN ... COMMIT`. Autocommit throughput remains a different problem and
depends on the server-side fsync path.

### Multi-row INSERT on Indexed Tables After `6.18`

Phase `6.18` fixes the immediate multi-row VALUES path for indexed tables. A
statement such as:

```sql
INSERT INTO bench_users VALUES
  (1, 'u1', 18, TRUE, 100.0, 'u1@b.local'),
  (2, 'u2', 19, FALSE, 100.1, 'u2@b.local'),
  (3, 'u3', 20, TRUE, 100.2, 'u3@b.local');
```

now uses grouped heap/index apply even when the target table has a PRIMARY KEY
or secondary indexes. Before `6.18`, that path still fell back to per-row
maintenance on indexed tables.

Measured with `python3 benches/comparison/local_bench.py --scenario insert_multi_values --rows 5000 --table`
on the benchmark schema with `PRIMARY KEY (id)`:

| Operation | MariaDB 12.1 | MySQL 8.0 | AxiomDB |
|---|---|---|---|
| `insert_multi_values` on PK table | 160,581 rows/s | 259,854 rows/s | **321,002 rows/s** |

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">2× Faster Than MariaDB</span>
On the PK-only multi-row INSERT benchmark, AxiomDB reaches <strong>321,002 rows/s</strong> vs MariaDB 12.1 at <strong>160,581 rows/s</strong>. The speedup comes from one grouped heap/index apply per VALUES statement instead of per-row maintenance on the indexed table.
</div>
</div>

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Prefer Multi-row VALUES</span>
If your application already knows several rows up front, send one <code>INSERT ... VALUES (...), (...)</code> statement instead of many one-row INSERTs. This now benefits indexed tables too, while still rejecting duplicate PRIMARY KEY / UNIQUE values inside the same statement.
</div>
</div>

### Prepared Statement Plan Cache (Phase 5.13)

`COM_STMT_PREPARE` compiles the SQL once (parse + analyze). Every subsequent
`COM_STMT_EXECUTE` reuses the compiled plan — no re-parsing, no catalog scan:

| Path | Per-execute cost |
|---|---|
| `COM_QUERY` (plain string) | parse + analyze + execute (~5 ms) |
| `COM_STMT_EXECUTE` — plan valid | substitute params + execute (~0.1 ms) — **50× faster** |
| `COM_STMT_EXECUTE` — after DDL | re-analyze once, then fast path resumes |

**Schema invalidation (correctness guarantee):** after `ALTER TABLE`, `DROP TABLE`,
`CREATE INDEX`, etc., the cached plan is re-analyzed automatically on the next execute.
The `schema_version` counter in `Database` increments on every successful DDL; each
connection polls it lock-free (`Arc<AtomicU64>`) before each execute.

**LRU eviction:** each connection caches up to `max_prepared_stmts_per_connection`
(default 1024) compiled plans. The least-recently-used plan is evicted silently when
the limit is reached. Configurable in `axiomdb.toml`.

### WAL Fsync Pipeline (6.19, closed with a documented gap)

Phase `6.19` replaced the old timer-based `CommitCoordinator` with an always-on
leader-based WAL fsync pipeline. The runtime behavior changed, but the key
single-connection autocommit benchmark remains a documented gap.

Measured with:

```bash
python3 benches/comparison/local_bench.py --scenario insert_autocommit --rows 1000 --table --engines axiomdb
```

Current result:

| Benchmark | AxiomDB | Target | Status |
|---|---|---|---|
| `insert_autocommit` | **224 ops/s** | `>= 5,000 ops/s` | ❌ |

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Good Primitive, Wrong Arrival Pattern</span>
MariaDB's <code>group_commit_lock</code> inspired the leader-based pipeline and it does remove the old timer window. But under a strict MySQL request/response client, the server still waits for durability before sending <code>OK</code>, so the next statement cannot arrive while the fsync is in flight. The batching primitive is therefore correct, but it does not solve the sequential single-client benchmark by itself.
</div>
</div>

### End-to-End INSERT Throughput

Full pipeline: parse → analyze → execute → WAL → MmapStorage. Measured with
`executor_e2e` benchmark (MmapStorage + real WAL, release build, Apple M2 Pro NVMe).

| Configuration                                   | AxiomDB         | MariaDB ~   | Status |
|-------------------------------------------------|-----------------|-------------|--------|
| INSERT 10K rows / N separate SQL strings / 1 txn| 35K rows/s      | 140K rows/s | ⚠️     |
| **INSERT 10K rows / 1 multi-row SQL string**    | **211K rows/s** | 140K rows/s | ✅ **1.5× faster** |
| INSERT autocommit (1 visible commit/stmt, wire protocol) | 224 q/s         | —           | ⚠️ (closed subphase, open perf gap) |

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

---

### Four-Engine Native Benchmark (2026-03-24)

All four engines measured locally on Apple M2 Pro, same machine, no Docker overhead,
10,000-row table (`id BIGINT AUTO_INCREMENT PRIMARY KEY`, `name VARCHAR(100)`,
`value INT`). Each engine was given equivalent hardware resources.

**Engines tested:**
- MariaDB 12.1 — port 3306
- MySQL 8.0 — port 3310
- PostgreSQL 16 — port 5433
- AxiomDB — port 3309

| Operation | MariaDB 12.1 | MySQL 8.0 | PostgreSQL 16 | AxiomDB |
|-----------|-------------|-----------|---------------|---------|
| INSERT batch (10K rows, 1 stmt) | 558 ms · 18K r/s | 628 ms · 16K r/s | 786 ms · 13K r/s | **275 ms · 36K r/s** |
| SELECT * (10K rows, full scan) | 62 ms · 162K r/s | 53 ms · 189K r/s | 4 ms · 2.3M r/s | 47 ms · 212K r/s |
| DELETE (no WHERE, 10K rows) | 31 ms · 323K r/s | 407 ms · 25K r/s | 47 ms · 212K r/s | **9.6 ms · 1M r/s** |

#### INSERT batch — 2× faster than MariaDB

AxiomDB reaches 36K r/s vs MariaDB's 18K r/s (2× faster) and MySQL's 16K r/s
(2.25× faster). The gap comes from the same three optimizations described above:
`HeapChain::insert_batch()` (O(P) page writes), `record_insert_batch()` (O(1) WAL
write), and a single parse+analyze pass for all N rows.

#### SELECT * — on par with MySQL, 11× behind PostgreSQL

AxiomDB SELECT (212K r/s) is marginally faster than MySQL 8.0 (189K r/s) and on par
with the full-pipeline expectation. PostgreSQL's 2.3M r/s reflects its shared buffer
pool: after the first scan, all 10K rows fit in PostgreSQL's hot in-memory buffer and
subsequent queries never touch disk. AxiomDB's mmap approach relies on the OS page
cache for the same effect — the gap closes when pages are hot, but PostgreSQL's buffer
pool gives it an edge on repeated same-connection scans because it bypasses the OS
cache layer entirely.

#### DELETE (no WHERE) — 3× faster than MariaDB, 40× faster than MySQL

AxiomDB deletes 10,000 rows in 9.6 ms (1M r/s). MariaDB takes 31 ms; MySQL 8.0 takes
407 ms. The AxiomDB advantage comes from two optimizations working together:

1. **`WalEntry::Truncate`** — a single 51-byte WAL entry replaces 10,000 per-row
   Delete entries. MySQL InnoDB writes one undo log record per row before marking
   it deleted — for 10K rows this is 10K undo writes plus 10K page modifications.
2. **`HeapChain::delete_batch()`** — groups deletions by page, reads each page once,
   marks all slots dead, writes back once. 10K rows across 50 pages = 100 page
   operations instead of 30,000.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">3× Faster Full-Table DELETE Than MariaDB, 40× Faster Than MySQL 8.0</span>
DELETE without WHERE on 10K rows: AxiomDB 9.6 ms (1M r/s) vs MariaDB 31 ms (323K r/s) vs MySQL 8.0 407 ms (25K r/s). The gap is structural: MySQL InnoDB writes one undo log entry per row and pins each page in the buffer pool individually. AxiomDB emits one <code>WalEntry::Truncate</code> and processes all deletions in O(P) page I/O where P = number of pages ≈ 50 for 10K rows.
</div>
</div>

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

---

## Optimization Results — All-Visible Flag + Prefetch (2026-03-24)

Two storage-level optimizations implemented on branch `research/pg-internals-comparison`,
inspired by PostgreSQL internals analysis:

### All-Visible Page Flag (optim-A)

After the first sequential scan on a stable table (all rows committed, none deleted),
AxiomDB sets bit 0 of `PageHeader.flags`. Subsequent scans skip per-slot MVCC
visibility tracking for those pages — 1 flag check per page instead of N per-slot
comparisons.

**Impact on DELETE:** `scan_rids_visible()` (used before batch delete) goes faster
because most pages are all-visible after INSERT → COMMIT. Measured improvement on
10K-row DELETE: **10ms → 7ms (+30%)**.

### Sequential Scan Prefetch Hint (optim-C)

`MmapStorage` now calls `madvise(MADV_SEQUENTIAL)` before every sequential heap
scan. The OS kernel begins async read-ahead for following pages, overlapping I/O
with processing of the current page.

**Impact:** Measurable on cold-cache workloads (pages not in OS page cache).
No regression on warm cache.

### Benchmark after both optimizations (wire protocol, Apple M2 Pro)

| Operation | MariaDB 12.1 | MySQL 8.0 | AxiomDB | PostgreSQL 16 (warm) |
|---|---|---|---|---|
| INSERT batch 10K | 150ms · 67K r/s | 301ms · 33K r/s | **278ms · 36K r/s** | 737ms · 14K r/s |
| SELECT * 10K | 53ms · 188K r/s | 48ms · 208K r/s | **49ms · 206K r/s** | 5ms · 2.1M r/s |
| DELETE 10K (no WHERE) | 13ms · 779K r/s | 102ms · 98K r/s | **7ms · 1.4M r/s** | 6ms · 1.6M r/s |

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
AxiomDB DELETE (no WHERE) at 1.4M rows/s outperforms MariaDB (779K r/s) by 1.8×
and MySQL 8.0 (98K r/s) by 14×. The combination of <code>WalEntry::Truncate</code>
(1 WAL entry instead of N) and the all-visible flag (skips MVCC scan overhead)
eliminates the two main costs in full-table deletion.
</div>
</div>

### `SELECT COUNT(*)` Fast Path (Attack 17)

The clustered-table `COUNT(*)` path bypasses the regular row-decode pipeline:
it walks the leaf chain and visits only each cell's 8-byte `RowHeader`
via `clustered_leaf::for_each_row_header`. No key slicing, no payload parse,
no `CellRef` construction — just the visibility check inlined per cell.

| Scenario | Pre-A17 | Post-A17 | Speedup |
|---|---:|---:|---:|
| `SELECT COUNT(*) FROM t` (10K rows, clustered) | 1.6K ops/s | **2.3K ops/s** | 1.5× |

The remaining ~60× gap vs SQLite lives in the SQL pipeline overhead (parse +
analyze + plan per autocommit query), not in the count itself.

### Clustered Range Scan (Attack 18)

`SELECT * FROM t WHERE id BETWEEN X AND Y` on a clustered (PRIMARY KEY) table
now uses `clustered_tree::range_callback` — a zero-alloc range iterator that
yields page-resident byte slices via a closure. For rows without overflow
tails (the common case on bench-sized rows), `decode_row` runs directly on
the page slice — no `reconstruct_row_data` clone, no `key.to_vec()`, no
per-row `ClusteredRow` struct.

| Scenario | Pre-A18 | Post-A18 | Speedup |
|---|---:|---:|---:|
| `range_scan` 1K-of-10K (clustered PK) | 361K ops/s | **1.42M ops/s** | **4×** |

This closes ~75% of the previous gap vs SQLite (10.4M ops/s).

### Clustered Point Lookup Primitive (Attack 19)

Same zero-alloc pattern applied to point lookups via the new
`clustered_tree::lookup_callback{_with_hint}` API: descend, binary-search,
invoke a closure with the page-resident slice — no row_data clone, no
key.to_vec(). Used by `table::lookup_clustered_row_with_hint` and available
for any caller off the SQL pipeline (FK validation, ON CONFLICT detection,
embedded direct API).

The benchmark `point_lookup` doesn't show a measurable win because each
lookup is wrapped in an autocommit `SELECT ... WHERE id = N` SQL query —
parse + analyze + plan dominate (~200µs/query) the storage cost (~5-20µs).
Attack 20 (autocommit statement cache) addresses the real bottleneck.

### Autocommit SELECT Statement Cache (Attack 20)

Repeated autocommit `SELECT` queries with different literals (`SELECT ...
WHERE id = N`, then `WHERE id = N+100`, etc.) now share one analyzed plan
via the per-session statement cache (`statement_cache::run_cached`). The
parser still runs (cheap), then literals are extracted to params, the
shape is hashed, and the cache returns the prior analyzed AST. The
analyzer's `PlanDeps` snapshot is re-validated against the catalog on each
hit (1-3 catalog reads typically) to evict stale entries after DDL.

| Scenario | Pre-A20 | Post-A20 | Speedup |
|---|---:|---:|---:|
| `point_lookup` 100×SELECT | 4.5K ops/s | **14.5K ops/s** | **3.2×** |
| `count_star` SELECT COUNT(*) | 2.3K ops/s | **~5K ops/s** | 2.1× |
| `range_scan` 1K-of-10K | 1.42M ops/s | **2.98M ops/s** | 2.1× |

Cache scope: SELECT only (gated by `sql_starts_with_select_keyword`).
INSERT/UPDATE/DELETE keep the legacy path — they have cheap analyze and
the prior Attack 2 wiring net-regressed them. Cumulative range_scan speedup
vs original baseline: **8.3×** (4× from A18 zero-alloc × 2.1× from A20
cache).

### Statement Cache Epoch Fast Path (Attack 23 + 23b)

Attack 20 eliminated parse+analyze on cache hits, but each hit still paid
for a `CatalogReader` object (18 meta-page reads) and `PlanDeps::is_stale`
(one HeapChain scan per dep-table) to verify the plan wasn't invalidated
by DDL. For a typical `SELECT ... WHERE pk = N` loop this was the dominant
remaining cost.

Two changes close this (Attack 23 — embedded path):

- **`epoch_plan_fast_path`**: `SessionContext` already tracks a
  `catalog_epoch` counter that increments only on DDL (`invalidate_all`).
  On each cache lookup the statement cache checks whether all dep-table
  epoch tags match `catalog_epoch`. On a match: skip `CatalogReader`
  creation and `PlanDeps::is_stale` entirely — the plan is validated in
  O(1) with zero catalog I/O.

- **`select_has_function_call` guard**: `rewrite_custom_aggregates_in_select`
  created a `CatalogReader` unconditionally to scan for custom aggregate
  definitions. A fast AST walk detects that plain `SELECT * FROM t WHERE
  pk = N` has no function calls at all, and the catalog scan is skipped.

| Scenario | Pre-A23 | Post-A23 | Speedup |
|---|---:|---:|---:|
| `point_lookup` 100×SELECT (embedded) | 5.6K ops/s | **11.8K ops/s** | **2.1×** |

(Lima VM numbers; macOS APFS native expected to be higher as seen in A20.)

**Attack 23b** extends the same fast path to the **MySQL wire server path**.
Before 23b, wire SELECT queries re-analyzed on every call. Now they route
through `run_cached`, giving them the same O(1) epoch-check path.

| Path | Wire SELECT ops/s |
|---|---:|
| Pre-23b (legacy analyze per call) | ~5,600 |
| Post-23b (`run_cached` wire path) | **~11,400** |
| Speedup | **~2×** |

(macOS APFS native; Lima VM numbers will be lower due to virtio-fs overhead.)

<div class="callout-advantage">
After A23 + A23b, both the embedded `Db` API and MySQL wire clients share
the same zero-catalog-I/O fast path for repeated SELECT shapes. The epoch
only advances on DDL, so a matching tag guarantees the cached plan is valid.
The gap vs SQLite's `point_lookup` (66.7K ops/s) tightens from 12× to 5.6×.
</div>

### Session COUNT(*) Cache (Attack 17b)

The `SELECT COUNT(*) FROM t` fast path now consults a per-session cache
keyed by `(table_id, schema_version)`. Cache hits return in O(1) without
re-scanning leaves. Validity is determined by the existing per-table
change counter on `StaleStatsTracker` — any INSERT/UPDATE/DELETE bumps
the counter and forces the next COUNT(*) to re-scan.

| Scenario | Pre-A17 | Post-A17b | Total Speedup |
|---|---:|---:|---:|
| `count_star` 10K rows (clustered) | 1.6K ops/s | **~12K ops/s** | **7.5×** |

Cache is gated to autocommit mode — inside an explicit BEGIN..ROLLBACK
the change counter doesn't unwind on rollback, so we skip the cache there
to preserve correctness. Multi-session writes from another connection
don't invalidate this session's cache; the cached count is a snapshot
value (per MVCC semantics).

### Why autocommit INSERT isn't on the statement cache yet

Attack 22 attempted to extend the statement cache (A20) to
INSERT/UPDATE/DELETE. Both repair paths regressed `insert_autocommit`
versus the cache-disabled baseline — keeping the existing per-dep
`PlanDeps.is_stale` check cost ~17%, and clearing the cache eagerly
from `invalidate_all` cost ~35% (the cache was wiped on every DML
because `invalidate_all` is overloaded for index-root changes).

For now the autocommit DML path keeps the legacy parse → analyze →
execute pipeline. Workloads that need the win can switch to the
embedded `Appender` API (a fast-path INSERT builder that bypasses the
SQL pipeline entirely) — see the
[embedded guide](embedded.md). The full investigation lives in
`docs/perf-sqlite-gap.md` "Attack 22 — deferred".
