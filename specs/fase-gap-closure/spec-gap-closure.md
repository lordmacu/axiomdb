# Spec: Gap Closure — select_pk, select_range, aggregate

## Context

After Phase 8 (SIMD) and Phase 9 (parallelism + joins), AxiomDB matches or beats
MariaDB/MySQL on 6 of 9 OLTP scenarios. Three gaps remain at 100K rows:

| Gap | AxiomDB | MariaDB | MySQL | Ratio |
|-----|---------|---------|-------|-------|
| select_pk | 9.2K q/s | 13.1K q/s | 12.3K q/s | 70% |
| select_range | 178K r/s | 226K r/s | 237K r/s | 75% |
| aggregate | 39 q/s | 78 q/s | 58 q/s | 50% |

## What to build

Six targeted optimizations, each addressing a specific root cause identified
through exhaustive research of PostgreSQL, MariaDB/InnoDB, DuckDB, SQLite,
and DataFusion source code.

---

## Optimization 1: Range scan prefetch batch

### What
B-Tree range iterator prefetches 4-8 next leaf pages when crossing a leaf
boundary, instead of blocking on `read_page()` for each page.

### Why
MariaDB `buf_read_ahead_linear()` prefetches 64 pages; PostgreSQL `ReadStream`
prefetches 4-256 pages adaptively. AxiomDB iterator blocks on every page read.
15-20% of the range scan gap is I/O latency waiting for the next page.

### Research source
- MariaDB: `storage/innobase/buf/buf0rea.cc:604-750` (64-page read-ahead)
- PostgreSQL: `src/backend/storage/aio/read_stream.c:83-146` (adaptive distance)
- PostgreSQL: `src/backend/access/nbtree/nbtsearch.c:1647` (_bt_steppage uses sibling)

### Inputs / Outputs
- Input: B-Tree range scan on index with N leaf pages
- Output: Same `(RecordId, key_bytes)` pairs, same ordering
- Behavioral change: `prefetch_hint(next_leaf, 4)` called at leaf boundary

### Acceptance criteria
- [ ] `RangeIter` calls `storage.prefetch_hint()` for next 4 leaves on boundary
- [ ] No double-read of current page (cache next_leaf on first access)
- [ ] `cargo test --workspace` passes
- [ ] select_range benchmark improves ≥10%

### Out of scope
- Adaptive distance (fixed 4 pages is sufficient for Phase 1)
- Sequential pattern detection (always prefetch for range scans)

---

## Optimization 2: Wire write batching

### What
Accumulate all result row packets in a single buffer before flushing to the
TCP socket, instead of per-packet writes. Single `write()` syscall per query.

### Why
MariaDB `net_write_buff()` accumulates in `net->buff` (16-64KB), flushes once.
PostgreSQL `printtup()` uses `pq_endmessage_reuse()` which batches into
StringInfo. AxiomDB currently writes each row packet individually.

### Research source
- MariaDB: `sql/net_serv.cc:570-610` (net_write_buff, one flush per query)
- PostgreSQL: `src/backend/access/common/printtup.c:304-383` (buffer reuse)

### Inputs / Outputs
- Input: Vec of serialized row packets from `serialize_query_result()`
- Output: Same bytes on the wire, same MySQL protocol compliance
- Behavioral change: single `write_all()` instead of N `write_all()` calls

### Acceptance criteria
- [ ] Result packets accumulated in single Vec<u8> before socket write
- [ ] Wire protocol tests pass (pymysql compatibility)
- [ ] select_pk benchmark improves ≥5%

### Out of scope
- Streaming (sending rows as produced) — requires executor redesign
- Large result set chunking (>1MB results)

---

## Optimization 3: Aggregate key reuse

### What
Eliminate per-row Vec allocation for GROUP BY key serialization. Reuse a
single buffer across rows, only reallocating when key length changes.

### Why
Current code: each row calls `group_key_bytes_session(&key_values)` which
allocates a new `Vec<u8>`. For 100K rows × 62 groups, this is 100K allocations.
DuckDB uses arena allocators; PostgreSQL uses memory contexts with bulk reset.

### Research source
- DuckDB: `src/execution/operator/aggregate/physical_hash_aggregate.cpp` (chunk-based)
- PostgreSQL: `src/backend/executor/nodeAgg.c:1-250` (memory context design)
- DataFusion: `datafusion/physical-plan/benches/aggregate_vectorized.rs` (batch pattern)

### Inputs / Outputs
- Input: `combined_rows: Vec<Row>` (100K rows)
- Output: Same `HashMap<Vec<u8>, GroupState>` groups
- Behavioral change: reuse `key_buf: Vec<u8>` across iterations

### Acceptance criteria
- [ ] Single key buffer reused across all rows (clear + extend, not allocate)
- [ ] GROUP BY results identical to current implementation
- [ ] aggregate benchmark improves ≥15%

### Out of scope
- Vectorized aggregate (batch processing of 1024 rows at once)
- Parallel aggregate (Rayon over row chunks)
- Spill-to-disk for aggregates

---

## Optimization 4: Adaptive Hash Index

### What
In-memory hash table mapping CRC-32C(key_prefix) → leaf page RecordId for
hot B-Tree pages. Bypasses B-Tree traversal for repeated PK lookups.

### Why
MariaDB AHI (`btr0sea.cc`) achieves 3-5 CPU instructions per lookup vs 20-50
for B-Tree traversal. Built automatically after 100 consecutive hash-able
accesses on a leaf page. Reduces select_pk latency by 15-20%.

### Research source
- MariaDB: `storage/innobase/btr/btr0sea.cc:54-65` (ahi_node struct)
- MariaDB: `storage/innobase/btr/btr0sea.cc:1098-1271` (btr_search_guess_on_hash)

### Inputs / Outputs
- Input: PK lookup key bytes
- Output: Same RecordId as B-Tree lookup, or None (fallback to B-Tree)
- Behavioral change: hash check before `BTree::lookup_in()`

### Acceptance criteria
- [ ] `AdaptiveHashIndex` struct with `lookup()` and `insert_on_access()`
- [ ] Auto-builds after N consecutive accesses on same leaf page
- [ ] Falls back to B-Tree on miss or collision
- [ ] select_pk benchmark improves ≥10%

### Out of scope
- AHI invalidation on page splits (deferred — rebuild lazily)
- Partitioned latches (single-threaded for now)
- AHI for non-unique indexes

---

## Optimization 5: Vectorized aggregate chunks

### What
Process GROUP BY + accumulators in batches of 1024 rows instead of per-row.
Extract GROUP BY column values for entire batch, hash all keys at once,
batch-update accumulators.

### Why
DuckDB processes 2048 rows per DataChunk. Per-row overhead (function call,
HashMap lookup, accumulator update) is amortized over the batch. DuckDB's
COUNT(*) processes 64 rows per validity word using bit operations.

### Research source
- DuckDB: `src/execution/operator/aggregate/physical_hash_aggregate.cpp:356-410`
- DuckDB: `src/function/aggregate/distributive/count.cpp:27-100`
- DataFusion: `datafusion/physical-plan/benches/aggregate_vectorized.rs`

### Inputs / Outputs
- Input: `combined_rows` split into 1024-row chunks
- Output: Same grouped aggregation results
- Behavioral change: batch column extraction + batch hash + batch accumulate

### Acceptance criteria
- [ ] Aggregate loop processes rows in 1024-row chunks
- [ ] Column values extracted as `Vec<Value>` per chunk (not per row)
- [ ] HashMap batch-lookup with pre-computed hashes
- [ ] aggregate benchmark improves ≥25% vs optimization 3

### Out of scope
- SIMD for accumulator updates (future — needs columnar Value layout)
- Parallel aggregate across cores (future)

---

## Optimization 6: Read-ahead 8-16 pages

### What
Extend prefetch from 4 pages (optimization 1) to 8-16 pages for large
sequential range scans, matching PostgreSQL's adaptive pattern.

### Why
MariaDB prefetches 64 pages; PostgreSQL up to 256. Our optimization 1 uses
4 pages. For scans crossing >20 pages, the additional prefetch depth
reduces I/O stalls further.

### Research source
- MariaDB: `storage/innobase/buf/buf0rea.cc:604` (READ_AHEAD_PAGES=64)
- PostgreSQL: `src/backend/storage/aio/read_stream.c:574` (effective_io_concurrency)

### Inputs / Outputs
- Input: Range scan crossing many leaf pages
- Output: Same results, lower latency
- Behavioral change: adaptive prefetch distance based on scan length

### Acceptance criteria
- [ ] Prefetch distance increases after 4 consecutive leaf-boundary crossings
- [ ] Maximum prefetch depth: 16 pages
- [ ] select_range benchmark improves ≥5% vs optimization 1

### Out of scope
- OS-level madvise() hints (future)
- Async I/O (io_uring) for true overlapped reads

---

## Dependencies

```
Optimization 1 (range prefetch) → no dependencies
Optimization 2 (wire batching)  → no dependencies
Optimization 3 (agg key reuse)  → no dependencies
Optimization 4 (AHI)            → no dependencies
Optimization 5 (vectorized agg) → depends on 3 (key reuse foundation)
Optimization 6 (read-ahead 16)  → depends on 1 (prefetch infrastructure)
```

## Sprint

```
Sprint: Gap Closure
├── Opt 1: Range prefetch batch          — no dependencies
├── Opt 2: Wire write batching           — no dependencies
├── Opt 3: Aggregate key reuse           — no dependencies
├── Opt 4: Adaptive Hash Index           — no dependencies
├── Opt 5: Vectorized aggregate chunks   — depends on Opt 3
└── Opt 6: Read-ahead 8-16 pages        — depends on Opt 1
```
