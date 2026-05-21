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

**Why range scan is very fast:** `RangeIter` re-traverses from the root to locate
each successive leaf after exhausting the current one. With CoW, `next_leaf` pointers
cannot be maintained consistently (a split copies the leaf, leaving the previous leaf's
pointer stale). Tree retraversal costs O(log n) per leaf boundary crossing — at 3–4
levels deep this is 3–5 page reads, all already in the OS page cache for sequential
workloads. The deferred `next_leaf` fast path (Phase 7) will reduce this to O(1) per
boundary once epoch-based reclamation is available.

## `SELECT ... WHERE pk = literal` After `6.16`

Phase `6.16` fixes the planner gap that still prevented single-table `SELECT`
from using the PRIMARY KEY B+Tree. The executor already supported `IndexLookup`
and `IndexRange`; the missing piece was planner eligibility plus a forced path
for PK equality.

Measured with:

```bash
python3 benches/comparison/local_bench.py --scenario select_pk --rows 5000 --table
```

| Operation | MariaDB 12.1 | MySQL 8.0 | AxiomDB |
|---|---|---|---|
| `SELECT * FROM bench_users WHERE id = literal` | 12.7K lookups/s | 13.4K lookups/s | **11.1K lookups/s** |

This closes the old "full scan on PK lookup" debt. The remaining gap is no
longer planner-side; it is now in SQL/wire overhead after the PK B+Tree path is
already active.

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
| Page alloc (1-thread batch vs Mutex) | **28.8×** ✅ | ≥5× | ≥5× | 40.9 |
| Page alloc (8-thread batch vs Mutex) | **703×** ✅ | ≥5× | ≥5× | 40.9 |

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

- **SELECT 185 q/s:** each `COM_QUERY` runs a full parse + analyze cycle (~1.5 µs) plus
  one wire protocol round-trip (~40 µs on localhost). The dominant cost is the round-trip.
  For prepared statements (`COM_STMT_EXECUTE`), Phase 5.13 plan cache eliminates the
  parse/analyze step entirely — the cached AST is reused and only a ~1 µs parameter
  substitution pass runs before execution. The remaining bottleneck for higher throughput
  is WAL transaction overhead per statement (BEGIN/COMMIT I/O); this will be addressed
  by Phase 6 indexed reads (eliminating full-table scans) and the Phase 8 batch API.
- **INSERT 58 q/s:** one `fdatasync` per autocommit statement is required for durability.

## Phase 5.21 — Transactional INSERT staging

Measured with `python3 benches/comparison/local_bench.py --scenario insert --rows 50000 --table`
against a release AxiomDB server and local MariaDB/MySQL instances on the same
machine. Workload: **50,000 separate one-row INSERT statements inside one
explicit transaction**.

| Benchmark | MariaDB 12.1 | MySQL 8.0 | AxiomDB | Notes |
|---|---|---|---|---|
| `insert` (single-row INSERTs in 1 txn) | 28.0K rows/s | 26.7K rows/s | **23.9K rows/s** | one `BEGIN`, 50K INSERT statements, one `COMMIT` |

What changed in `5.21`:

- the session now buffers consecutive eligible `INSERT ... VALUES` rows for the
  same table instead of writing heap/WAL immediately
- barriers such as `SELECT`, `UPDATE`, `DELETE`, DDL, `COMMIT`, table switch,
  or ineligible INSERT shapes force a flush
- the flush uses `insert_rows_batch_with_ctx(...)` plus grouped post-heap index
  maintenance, persisting each changed index root once per flush

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Borrowed Technique</span>
AxiomDB borrows the “produce rows first, write them later” pattern from PostgreSQL
<code>heap_multi_insert()</code> and DuckDB's appender, but keeps SQL semantics intact by
flushing before the next statement savepoint whenever the batch cannot continue.
</div>
</div>

This is deliberately **not** the same as autocommit group commit. The benchmark
already uses one explicit transaction, so `5.21` attacks per-statement heap/WAL/index
work rather than fsync batching across multiple commits.

---

## Phase 6.19 — WAL fsync pipeline

Measured with:

```bash
python3 benches/comparison/local_bench.py --scenario insert_autocommit --rows 1000 --table --engines axiomdb
```

Workload: **one INSERT per transaction over the MySQL wire**.

| Benchmark | AxiomDB | Target | Status |
|---|---|---|---|
| `insert_autocommit` | **224 ops/s** | `>= 5,000 ops/s` | ❌ |

What changed in `6.19`:

- the old timer-based `CommitCoordinator` and its config knobs were removed
- server DML commits now hand deferred durability to an always-on
  leader-based `FsyncPipeline`
- queued followers can piggyback on a leader fsync when their `commit_lsn`
  is already covered

What the benchmark taught us:

- the implementation is correct and wire-visible semantics remain intact
- but the target workload is **sequential request/response autocommit**
- the handler still waits for durability before it sends `OK`
- therefore the next statement cannot arrive while the current fsync is in
  flight, so single-connection piggyback never materializes

`6.19` is closed as an implementation subphase, but this benchmark remains a
documented performance gap rather than a solved target.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Borrowed Technique, Different Constraint</span>
AxiomDB borrowed MariaDB's leader/follower fsync idea, but MariaDB's win depends on overlapping arrivals. The local benchmark uses a strictly sequential MySQL client, so the server never has the next autocommit statement in hand while the current fsync is still running.
</div>
</div>

---

## Phase 6.18 — Indexed multi-row INSERT batch path

Measured with:

```bash
python3 benches/comparison/local_bench.py --scenario insert_multi_values --rows 5000 --table
```

Workload: **multi-row `INSERT ... VALUES (...), (... )` statements against the
benchmark schema with `PRIMARY KEY (id)`**.

| Operation | MariaDB 12.1 | MySQL 8.0 | AxiomDB |
|---|---|---|---|
| `insert_multi_values` on PK table | 160,581 rows/s | 259,854 rows/s | **321,002 rows/s** |

What changed in `6.18`:

- the immediate multi-row `VALUES` path no longer checks `secondary_indexes.is_empty()`
  before using grouped heap writes
- grouped heap/index apply was extracted into shared helpers reused by both:
  - the transactional staging flush from `5.21`
  - the immediate `INSERT ... VALUES (...), (... )` path
- the immediate path keeps strict UNIQUE semantics by **not** reusing the staged
  `committed_empty` shortcut, because same-statement duplicate keys must still
  fail without leaking partial rows

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">2× Faster Than MariaDB</span>
On the PK-only `insert_multi_values` benchmark, AxiomDB reaches <strong>321,002 rows/s</strong> vs MariaDB 12.1 at <strong>160,581 rows/s</strong>. The gain comes from one grouped heap/index apply per VALUES statement instead of falling back to one heap/index maintenance cycle per row.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Share Apply, Keep UNIQUE Strict</span>
PostgreSQL's <code>heap_multi_insert()</code> and DuckDB's appender both separate row staging from physical write. AxiomDB borrows the grouped physical apply idea, but rejects a blind bulk-load shortcut on the immediate path: duplicate keys inside one SQL statement must still be rejected before any partial batch becomes visible.
</div>
</div>

---

## Phase 6.20 — UPDATE apply fast path

Measured with `python3 benches/comparison/local_bench.py --scenario update_range --rows 5000 --table`
against a release AxiomDB server and local MariaDB/MySQL instances on the same
machine. Workload: `UPDATE bench_users SET score = score + 1 WHERE id BETWEEN ...`
on a PK-indexed table.

| Benchmark | MariaDB 12.1 | MySQL 8.0 | AxiomDB | Notes |
|---|---|---|---|---|
| `update_range` | 618K rows/s | 291K rows/s | **369.9K rows/s** | PK range UPDATE now stays on a batched read/apply path end-to-end |

What changed in `6.20`:

- `IndexLookup` / `IndexRange` candidate rows are fetched through
  `read_rows_batch(...)` instead of one heap read per RID
- no-op UPDATE rows are filtered before heap/index mutation
- stable-RID rows batch `UpdateInPlace` WAL append with
  `reserve_lsns(...) + write_batch(...)`
- UPDATE index maintenance now uses grouped delete+insert with one root
  persistence write per affected index
- both ctx and non-ctx UPDATE paths share a statement-level index bailout

This closes the dominant apply-side debt left behind after `6.17`. The benchmark
improves by `4.3x` over the `6.17` result (`85.2K rows/s`) and now beats the
documented local MySQL result on the same workload.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
AxiomDB improves `update_range` from <strong>85.2K</strong> to <strong>369.9K rows/s</strong> after `6.20`, a <strong>4.3x</strong> gain, and overtakes MySQL 8.0 (<strong>291K rows/s</strong>) by keeping PK-range UPDATE inside one batched heap/WAL apply path.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Batch Without New WAL Type</span>
MariaDB's `row0upd.cc` and PostgreSQL's heap update path optimize clustered-row UPDATE primarily by reducing repeated heap/index work, not by inventing a new redo format first. AxiomDB follows that trade-off in `6.20`: it reuses `UpdateInPlace` for recovery compatibility and attacks the per-row apply overhead around it.
</div>
</div>

---

## Phase 5.19 / 5.20 — DELETE WHERE and UPDATE Write Paths

Measured with `python3 benches/comparison/local_bench.py --scenario all --rows 50000 --table`
on the same Apple M2 Pro machine. The benchmark uses the MySQL wire protocol and a
`bench_users` table with `PRIMARY KEY (id)`.

| Operation | MariaDB 12.1 | MySQL 8.0 | AxiomDB | PostgreSQL 16 |
|---|---|---|---|---|
| `DELETE WHERE id > 25000` | 652K rows/s | 662K rows/s | **1.13M rows/s** | 3.76M rows/s |
| `UPDATE ... WHERE active = TRUE` | 662K rows/s | 404K rows/s | **648K rows/s** | 270K rows/s |

`5.19` removed the old per-row `delete_in(...)` loop by batching exact encoded keys
per index through `delete_many_in(...)`. `5.20` finished the UPDATE recovery by
preserving the original RID whenever the rewritten row still fits in the same slot.

For UPDATE, the before/after delta is the important signal:

- Post-`5.19` / pre-`5.20`: `52.9K rows/s`
- Post-`5.20`: `648K rows/s`

That is a ~12.2× improvement on the same workload.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
After `5.20`, AxiomDB's `UPDATE ... WHERE active = TRUE` reaches <strong>648K rows/s</strong>,
beating MySQL 8 (<strong>404K</strong>) and PostgreSQL 16 (<strong>270K</strong>) on the same
50K-row local benchmark. The gain comes from avoiding RID churn and untouched-index rewrites
whenever the row still fits in its original heap slot.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Two-Step DML Recovery</span>
`5.19` and `5.20` fix different write-path costs. Batch-delete removes repeated B+Tree
descents for stale keys; stable-RID update removes heap delete+insert and makes index skipping
safe. Keeping them as separate subphases made the remaining bottleneck visible after each step.
</div>
</div>

---

## Phase 5.13 — Prepared Statement Plan Cache {#phase-513-plan-cache}

Phase 5.13 introduces an AST-level plan cache for prepared statements. The full parse +
analyze pipeline runs **once** at `COM_STMT_PREPARE` time; each subsequent
`COM_STMT_EXECUTE` performs only a tree walk to substitute parameter values (~1 µs)
and then calls `execute_stmt()` directly.

| Path | Parse + Analyze | Param substitution | Total SQL overhead |
|---|---|---|---|
| `COM_QUERY` (text protocol) | ~1.5 µs per call | — | ~1.5 µs |
| `COM_STMT_EXECUTE` before 5.13 | ~1.5 µs per call (re-parse) | string replace | ~1.5 µs |
| `COM_STMT_EXECUTE` after 5.13 | 0 (cached) | ~1 µs AST walk | **~1 µs** |

The ~0.5 µs saving per execute is meaningful for high-frequency statement patterns
(e.g., ORM-generated queries that re-execute the same SELECT or INSERT with different
parameters on every request).

**Remaining bottleneck:** the dominant cost per `COM_STMT_EXECUTE` is now the WAL
transaction overhead (BEGIN/COMMIT I/O) rather than parse/analyze. For read-only
prepared statements, Phase 6 indexed reads will eliminate full-table scans, reducing
the per-query execution cost. For write statements, the Phase 8 batch API will coalesce
WAL entries, targeting the 180K ops/s budget.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — AST cache, not string cache</span>
The plan cache stores the analyzed <code>Stmt</code> (AST with resolved column indices)
rather than the original SQL string. This means each execute avoids both lexing and
semantic analysis, not just parsing. The trade-off is that the cached AST must be
cloned before parameter substitution to avoid mutating shared state — a shallow clone
of the expression tree is ~200 ns, well below the ~1.5 µs that parse + analyze would
cost. MySQL and PostgreSQL cache parsed + planned query trees for the same reason.
</div>
</div>

---

## Phase 40.9 — FreeList Tier-1 (LocalPageBatch)

Measured with `cargo bench --bench storage -p axiomdb-storage` on Apple M2 Pro, NVMe SSD.
Workload: 1000 alloc+free cycles (single-thread) and 8 threads × 10K alloc+free cycles
(concurrent).

| Benchmark | Time | Throughput | Speedup |
|---|---|---|---|
| single_mutex_1000 (baseline) | 36.5 ms | 27.4K elem/s | 1× |
| batch_local_1000 (LocalPageBatch) | 1.27 ms | 787.7K elem/s | **28.8×** |
| 8t_single_10k (8 threads, per-page Mutex) | 5.35 s | 187 elem/s | 1× |
| 8t_batch_10k (8 threads, LocalPageBatch) | 7.6 ms | 131.6K elem/s | **703×** |

The single-thread improvement comes from amortizing the Mutex acquisition: one lock
per 64 pages instead of one per page. The 8-thread improvement is multiplicative:
each thread has its own `LocalPageBatch`, so 8 threads run 8 independent batches
with zero cross-thread contention on the fast path.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">703× Faster Than Per-Page Mutex Under 8 Threads</span>
DuckDB's <code>SingleFileBlockManager</code> uses a single mutex per allocation. InnoDB uses
SX-latches on extent descriptors (64 pages each). AxiomDB's <code>LocalPageBatch</code>
eliminates the global lock entirely for 99% of allocations — the remaining 1% (batch refill)
amortizes the Mutex cost across 64–256 pages. Under 8 concurrent writers, the speedup is
<strong>703×</strong> because each writer's local batch is fully independent.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Lock-Free Recycle Queue</span>
Freed pages from committed transactions go to a <code>crossbeam_queue::SegQueue&lt;u64&gt;</code>
(lock-free MPMC queue) instead of immediately back to the bitmap under Mutex. The next
batch refill drains this queue first — already-initialized pages, zero bitmap overhead.
PostgreSQL's FSM serves a similar purpose (freed space available for reuse without
re-scanning); AxiomDB keeps it in-memory for simplicity, with periodic fold-back to the
bitmap during <code>flush()</code>.
</div>
</div>

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

---

## Phase 11.2d Refcounted BLOB Chains

Measured with Criterion on `axiomdb-storage` after adding the versioned `ABOB`
TOAST/BLOB overflow header. These benchmarks use `MemoryStorage`, so they isolate
overflow-chain layout, checksum, page reads/writes, and refcount operations from
disk latency.

```bash
cargo bench -p axiomdb-storage --bench storage overflow/refcounted_blob
```

| Benchmark | Mean | Throughput | Verdict |
|---|---:|---:|---|
| `overflow/refcounted_blob/write_12kb` | 89.896 us | 130.36 MiB/s | ✅ |
| `overflow/refcounted_blob/read_128kb` | 20.355 us | 5.997 GiB/s | ✅ |
| `overflow/refcounted_blob/incref_free_shared_128kb` | 25.241 us | 39.618 Kops/s | ✅ |

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Isolated Storage Bench</span>
MySQL InnoDB and PostgreSQL TOAST expose large values through full SQL engines, while this Phase 11.2d benchmark isolates AxiomDB's overflow-chain storage path so refcount/layout regressions are visible before SQL, WAL, or network overhead hides them.
</div>
</div>

---

## Attack 23 — Statement Cache Epoch Fast Path

Measured with `axiomdb_bench --scenario point_lookup --rows 10000` on Lima VM
(AlmaLinux 10 / virtio-fs). `point_lookup` runs 100 autocommit `SELECT * FROM
bench_users WHERE id = N` queries (different N each call, same plan shape).

```bash
cargo run -p axiomdb-bench-comparison --release -- --scenario point_lookup --rows 10000
```

| Scenario | Pre-A23 | Post-A23 | Speedup |
|---|---:|---:|---:|
| `point_lookup` 100×SELECT | 5,572 ops/s | **11,753 ops/s** | **2.11×** |

SQLite reference on same hardware: 66.7K ops/s (gap 12× → 5.6× after A23).

Two micro-optimizations combine:

1. **`epoch_plan_fast_path`** in `run_cached`: skips `CatalogReader::new` (18
   meta-page reads) + `PlanDeps::is_stale` (1 HeapChain scan/dep-table) when
   all dep-table epoch tags match `catalog_epoch`. The epoch counter only
   advances on DDL — so a match guarantees plan validity in O(1).

2. **`select_has_function_call` guard** in `rewrite_custom_aggregates_in_select`:
   skips the second `CatalogReader::new` for plain `SELECT` queries that contain
   no function calls. `point_lookup` queries have no function calls, so this
   fires 100% of the time after the warm-up query.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Advantage — Zero Catalog I/O on Hot SELECT Paths</span>
After A20+A23, a repeated <code>SELECT * FROM t WHERE pk = ?</code> pays only for: one parser pass, one literal extraction, one shape hash lookup, one O(1) epoch check, and the actual B-Tree descent. No CatalogReader objects, no HeapChain scans. This is structurally equivalent to SQLite's prepared-statement fast path.
</div>
</div>

## Attack 23b — Epoch Fast Path Extended to MySQL Wire Path

Attack 23 applied the statement-cache epoch fast path only to the embedded
`Db` API. MySQL wire clients still re-analyzed every SELECT on every call.
Attack 23b routes wire SELECT queries through `run_cached`, giving them the
same O(1) cache-hit path as embedded queries.

Measured with `tools/bench-wire-ab.py` on the **Lima VM** (Linux), 3
interleaved rounds of 12 timed runs (the macOS-native wire bench is too noisy
— ±60% run-to-run — to show this signal):

| Scenario | Pre-23b (no wire cache) | Post-23b (wire cache) | Speedup |
|---|---:|---:|---:|
| point_lookup (100 same-shape) | ~2,554 ops/s | ~4,531 ops/s | **~1.78×** |
| count_star | ~1,901 ops/s | ~3,022 ops/s | **~1.6×** |
| range_scan / select_where | — | — | ~1.0× (neutral) |

The cache wins on **small-result, high-frequency** queries (point lookup,
`COUNT(*)`), where the per-call `analyze` is a meaningful fraction of query
cost. On large scans it's neutral — row encoding dominates. The first cut of
23b also introduced 12 wire-test regressions (shape-hash collisions, dropped
`SKIP LOCKED` locks, a read-your-own-commit snapshot race), all fixed; see
`docs/perf-sqlite-gap.md` for the full post-merge regression hunt.

**Correctness work**: the shape hash (`hash_stmt`) and literal extraction
(`walk_stmt_extract`) were extended to cover all expression positions that
can differ between otherwise-identical queries:

- `DISTINCT` / `DISTINCT ON` expressions
- FROM-clause SRF variant and args (UNNEST array count, GENERATE_SERIES bounds)
- JOIN type and table shape
- GROUP BY key expressions (not just discriminant)
- ORDER BY expressions + direction
- LIMIT and OFFSET (full shape, not just presence)

`substitute_params` was correspondingly extended so params in FROM-clause
SRFs, JOINs, LIMIT, and OFFSET are restored correctly on cache hits.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Advantage — Wire Path Now Equals Embedded Path</span>
After A23b, both the embedded <code>Db</code> API and MySQL wire clients share the same statement-cache fast path. A repeated <code>SELECT * FROM t WHERE pk = ?</code> over the wire pays only for: network parse, one literal extraction, one shape hash, one epoch check, and the B-Tree descent — no CatalogReader, no HeapChain scan per query.
</div>
</div>

## Embedded scan vs SQLite — engine, binding, and fair measurement

Two corrections to earlier embedded numbers:

1. **The ctypes Python binding, not the engine, dominated the embedded gap.**
   Engine-to-engine (pure Rust, SQLite via `rusqlite`), AxiomDB scans at ~4M
   rows/s; the `ctypes` binding throttles it to ~120K (~37× overhead). For the
   embedded-Python use case, a native (PyO3) binding is the biggest lever.

2. **Fair comparison: both engines must materialize columns.** The harness
   originally let SQLite step rows without extracting columns (its lazy
   `OP_Column` skips unaccessed TEXT). With both materializing:

   | Scenario | AxiomDB | SQLite | Ratio |
   |---|---:|---:|---:|
   | full_scan | 3.92M | 3.84M | **0.98× (faster)** |
   | crud_flow/select | 3.24M | 3.92M | 1.21× |
   | count_star | 127K | 170K | 1.34× |
   | range_scan | 1.87M | 3.64M | 1.95× |
   | select_where | 2.14M | 7.37M | 3.45× |
   | point_lookup | 16.3K | 117K | 7.23× |

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Advantage — Full-scan engine parity with SQLite</span>
After the scan Tier 1 optimization (skipping the redundant <code>SELECT *</code>
projection clone), AxiomDB's full-table-scan engine throughput (~3.9M rows/s)
matches SQLite's when both engines materialize every column. The remaining read
gaps are point lookups (per-query overhead) and filtered scans (decode-then-filter).
</div>
</div>

## select_where: clustered BatchPredicate (filter before decode)

The clustered scan now compiles the `WHERE` into a raw-byte `BatchPredicate`
(like the heap scan) and filters each row on its encoded bytes BEFORE decoding —
rows that fail the predicate are never decoded (no `Vec<Value>`, no per-TEXT
`String`). Falls back to the scalar `eval()` for predicates that can't be
compiled (OR / LIKE / IN / TEXT / subquery).

| Scenario (10K rows, fair --compare) | Before | After | SQLite | Gap |
|---|---:|---:|---:|---|
| select_where | 2.14M | **~5.9M ops/s** | ~7.1M | 3.45× → ~1.2× |

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Advantage — Predicate pushdown before decode</span>
Filtering on encoded bytes before materialization (SQLite's lazy-column idea,
applied as a compiled predicate) lifts <code>SELECT * ... WHERE</code> on a
clustered table ~2.75× and brings it within ~1.2× of SQLite.
</div>
</div>
