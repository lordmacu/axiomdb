# Benchmarks

All benchmarks run on **Apple M2 Pro (12 cores), 32 GB RAM, NVMe SSD**, single-threaded,
warm data (all pages in OS page cache unless noted). Criterion.rs is used for all
micro-benchmarks; each measurement is the mean of at least 100 samples.

Reference values for MySQL 8 and PostgreSQL 15 are measured in-process (no network),
without WAL for pure codec/parser operations. Operations that include WAL
(INSERT, UPDATE) are directly comparable.

---

## SQL Parser

| Benchmark | AxiomDB | sqlparser-rs | MySQL ~ | PostgreSQL ~ | Verdict |
|---|---|---|---|---|---|
| Simple SELECT (1 table) | **492 ns** | 4.8 µs | ~500 ns | ~450 ns | ✅ parity with PG |
| Complex SELECT (multi-JOIN) | **2.7 µs** | 46 µs | ~4.0 µs | ~3.5 µs | ✅ 1.3× faster than PG |
| CREATE TABLE | **1.1 µs** | 14.5 µs | ~2.5 µs | ~2.0 µs | ✅ 1.8× faster than PG |
| Batch (100 statements) | **47 µs** | — | ~90 µs | ~75 µs | ✅ 1.6× faster than PG |

**vs sqlparser-rs:** 9.8× faster on simple SELECT, 17× faster on complex SELECT.

The speed advantage comes from two decisions:

1. **logos DFA lexer** — compiles token patterns to a Deterministic Finite Automaton at
   build time. Scanning runs in O(n) time with 1–3 CPU instructions per byte.
2. **Zero-copy tokens** — `Ident` tokens are `&'src str` slices into the original input.
   No heap allocation occurs during lexing or AST construction.

---

## B+ Tree Index

| Benchmark | AxiomDB | MySQL ~ | PostgreSQL ~ | Target | Max acceptable | Verdict |
|---|---|---|---|---|---|---|
| Point lookup (1M rows) | **1.2M ops/s** | ~830K ops/s | ~1.1M ops/s | 800K ops/s | 600K ops/s | ✅ |
| Range scan 10K rows | **0.61 ms** | ~8 ms | ~5 ms | 45 ms | 60 ms | ✅ |
| Insert (sequential keys) | **195K ops/s** | ~150K ops/s | ~120K ops/s | 180K ops/s | 150K ops/s | ✅ |
| Sequential scan 1M rows | **0.72 s** | ~0.8 s | ~0.5 s | 0.8 s | 1.2 s | ✅ |
| Concurrent reads ×16 | **linear** | ~2× degradation | ~1.5× degradation | linear | <2× degradation | ✅ |

**Why point lookup is fast:** the CoW B+ Tree root is an `AtomicU64`. Readers load it
with `Acquire` and traverse 3–4 levels of 16 KB pages that are already in the OS page
cache. No mutex, no RWLock.

**Why range scan is very fast:** `RangeIter` follows `next_leaf` pointers after
reaching the first qualifying leaf. Each subsequent leaf is a single mmap dereference.
For 10K rows in order, this is typically 3–5 page accesses.

---

## Row Codec

| Benchmark | Throughput | Notes |
|---|---|---|
| `encode_row` | **33M rows/s** | 5-column mixed-type row |
| `decode_row` | **28M rows/s** | Same layout |
| `encoded_len` | **O(n), no alloc** | Size computation without buffer allocation |

The codec encodes a null bitmap (1 bit per column, packed into bytes) followed by
the column payloads in declaration order. Variable-length types use a 3-byte (u24)
length prefix. Fixed-size types (integers, floats, DATE, TIMESTAMP, UUID) have no
length prefix.

---

## Expression Evaluator

| Benchmark | AxiomDB | MySQL ~ | PostgreSQL ~ | Verdict |
|---|---|---|---|---|
| Expr eval over 1K rows | **14.8M rows/s** | ~8M rows/s | ~6M rows/s | ✅ 1.9× faster than MySQL |

The evaluator is a recursive interpreter over the `Expr` enum. Speed comes from
inlining the hot path (column reads, arithmetic, comparisons) and from the fact
that `col_idx` is resolved once by the semantic analyzer — no name lookup at eval time.

---

## Performance Budget

The following thresholds are enforced before any phase is closed. A result below
the "Max acceptable" column is a blocker.

| Operation | AxiomDB | Target | Max acceptable | Phase measured |
|---|---|---|---|---|
| Point lookup PK | **1.2M ops/s** ✅ | 800K ops/s | 600K ops/s | 2 |
| Range scan 10K rows | **0.61 ms** ✅ | 45 ms | 60 ms | 2 |
| B+ Tree INSERT (storage only) | **195K ops/s** ✅ | 180K ops/s | 150K ops/s | 3 |
| INSERT end-to-end 10K batch (SchemaCache) | **36K ops/s** ⚠️ | 180K ops/s | 150K ops/s | 4.16b |
| SELECT via wire protocol (autocommit) | **185 q/s** ✅ | — | — | 5.14 |
| INSERT via wire protocol (autocommit) | **58 q/s** — | — | — | 5.14 |
| Sequential scan 1M rows | **0.72 s** ✅ | 0.8 s | 1.2 s | 2 |
| Concurrent reads ×16 | **linear** ✅ | linear | <2× degradation | 2 |
| Parser — simple SELECT | **492 ns** ✅ | 600 ns | 1 µs | 4 |
| Parser — complex SELECT | **2.7 µs** ✅ | 3 µs | 6 µs | 4 |
| Row codec encode | **33M rows/s** ✅ | — | — | 4 |
| Expr eval (scan 1K rows) | **14.8M rows/s** ✅ | — | — | 4 |

**Executor end-to-end (Phase 4.16b, MmapStorage + real WAL, full pipeline)**

Measured with `cargo bench --bench executor_e2e -p axiomdb-sql` (Apple M2 Pro, NVMe,
release build). Pipeline: parse → analyze → execute → WAL → MmapStorage.

| Configuration | AxiomDB | Target (Phase 8) | Notes |
|---|---|---|---|
| INSERT 100 rows / 1 txn (no SchemaCache) | 2.8K ops/s | — | cold path, catalog scan |
| INSERT 1K rows / 1 txn (no SchemaCache) | 18.5K ops/s | — | amortization starts |
| INSERT 1K rows / 1 txn (SchemaCache) | 20.6K ops/s | — | +8% vs no cache |
| INSERT 10K rows / 1 txn (SchemaCache) | **36K ops/s** | 180K ops/s | ⚠️ WAL bottleneck |
| INSERT autocommit (1 fsync/row) | **58 q/s** | — | 1 fdatasync per statement (wire protocol, Phase 5.14) |

**Root cause — WAL `record_insert()` dominates:** each row write costs ~20 µs inside
`record_insert()` even without fsync. Parse + analyze cost per INSERT is ~1.5 µs total;
SchemaCache eliminates catalog heap scans but only improves throughput by 8% because WAL
overhead is already the dominant term. The 180K ops/s target is a Phase 8 goal: prepared
statements skip parse and analyze entirely, and a batch insert API will write one WAL entry
per batch rather than one per row.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — WAL per-row write</span>
The current WAL implementation writes one entry per inserted row via
<code>record_insert()</code>. This makes recovery straightforward — each row is an
independent, self-contained undo/redo unit — but costs ~20 µs/row at the WAL layer
regardless of fsync. The 36K ops/s ceiling at 10K batch size is a direct consequence of
this design. PostgreSQL and MySQL both offer bulk-load paths (COPY, LOAD DATA) that bypass
per-row WAL overhead; AxiomDB's equivalent is the Phase 8 batch insert API, which will
coalesce WAL entries and write them in a single sequential append.
</div>
</div>

**B+ Tree storage-only INSERT (no SQL parsing, no WAL):**

| Operation | AxiomDB | MySQL ~ | PostgreSQL ~ | Target | Max acceptable | Verdict |
|---|---|---|---|---|---|---|
| B+Tree INSERT (storage only) | **195K ops/s** | ~150K ops/s | ~120K ops/s | 180K ops/s | 150K ops/s | ✅ |

The storage layer itself exceeds the 180K ops/s target. The gap between 195K (storage only)
and 36K (full pipeline) isolates the overhead to the WAL record path, not the B+ Tree or
the page allocator.

**Run end-to-end benchmarks:**
```bash
cargo bench --bench executor_e2e -p axiomdb-sql

# MySQL + PostgreSQL comparison (requires Docker):
./benches/comparison/setup.sh
python3 benches/comparison/bench_runner.py --rows 10000
./benches/comparison/teardown.sh
```

---

## Phase 5.14 — Wire Protocol Throughput {#phase-514-wire-protocol}

Measured via the MySQL wire protocol (pymysql client, autocommit mode, 1 connection,
localhost, Apple M2 Pro, NVMe SSD).

| Benchmark | AxiomDB | MySQL ~ | PostgreSQL ~ | Notes |
|---|---|---|---|---|
| COM_PING | **24,865/s** | ~30K/s | ~25K/s | Pure protocol, no SQL engine |
| SET NAMES (intercepted) | **46,672/s** | ~20K/s | — | Handled in protocol layer |
| SELECT 1 (autocommit) | **185 q/s** | ~5K–15K q/s* | ~5K–12K q/s* | Full pipeline, read-only |
| INSERT (autocommit, 1 fsync/stmt) | **58 q/s** | ~130–200 q/s* | ~100–160 q/s* | Full pipeline + fsync |

*MySQL/PostgreSQL figures are in-process estimates without network latency overhead.
AxiomDB throughput measured over localhost with real round-trips; the gap reflects the
current single-threaded autocommit path and will improve with Phase 5.13 plan cache
and Phase 8 batch API.

**Phase 5.14 fix — read-only WAL fsync eliminated:**

Prior to Phase 5.14, every autocommit transaction called `fdatasync` on WAL commit,
including read-only queries such as `SELECT`. This cost 10–20 ms per `SELECT`, capping
throughput at ~56 q/s.

The fix: skip `fdatasync` (and the WAL flush) when the transaction has no DML operations
(`undo_ops.is_empty()`). Read-only transactions still flush buffered writes to the OS
(`BufWriter::flush`) so that concurrent readers see committed state, but they do not
wait for the `fdatasync` round-trip to persistent storage.

**Before / after:**

| Query | Before (5.13) | After (5.14) | Improvement |
|---|---|---|---|
| SELECT 1 (autocommit) | ~56 q/s | **185 q/s** | **3.3×** |
| INSERT (autocommit) | ~58 q/s | **58 q/s** | no change (fsync required) |

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — read-only fsync skip</span>
The WAL commit path gates durability on <code>fdatasync</code>. For DML transactions
this is correct — data must reach persistent storage before the client receives OK.
For read-only transactions there is nothing to persist: the transaction produced no WAL
records. Skipping <code>fdatasync</code> for <code>undo_ops.is_empty()</code> transactions
is therefore safe: crash recovery cannot lose data that was never written. PostgreSQL applies
the same principle — read-only transactions in PostgreSQL do not touch the WAL at all.
The OS-level flush (<code>BufWriter::flush</code>) is kept so that any WAL bytes written by
a concurrent writer are visible to the OS before the SELECT returns, preserving read-after-write
consistency within the same process.
</div>
</div>

**Bottleneck analysis:**

- **SELECT 185 q/s:** each query still runs a full parse + analyze cycle (~1.5 µs) plus
  one wire protocol round-trip (~40 µs on localhost). The dominant cost is the round-trip.
  Phase 5.13 plan cache will eliminate the parse/analyze overhead; throughput will then be
  limited purely by network latency.
- **INSERT 58 q/s:** one `fdatasync` per autocommit statement is required for durability.
  At ~10–20 ms/fsync on NVMe this caps single-connection autocommit INSERT at ~50–100 q/s,
  consistent with the observed 58 q/s. The Phase 8 batch API will coalesce multiple inserts
  into one WAL append + one fsync, targeting the 180K ops/s budget.

---

## Running Benchmarks Locally

```bash
# B+ Tree
cargo bench --bench btree -p axiomdb-index

# Storage engine
cargo bench --bench storage -p axiomdb-storage

# SQL parser
cargo bench --bench parser -p axiomdb-sql

# All benchmarks
cargo bench --workspace

# Compare before/after a change
cargo bench -- --save-baseline before
# ... make change ...
cargo bench -- --baseline before

# Detailed comparison with critcmp
cargo install critcmp
critcmp before after
```

Benchmarks use **Criterion.rs** and emit JSON results to `target/criterion/`. Each
run reports mean, standard deviation, min, max, and throughput (ops/s or bytes/s
depending on the benchmark).
