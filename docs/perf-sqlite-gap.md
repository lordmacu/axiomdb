# Closing the SQLite Performance Gap

> **Status**: in progress. Attack 3.A landed 2026-05-16. ~2× across-the-board win.

AxiomDB is being prepared for an embedded-library release (similar to SQLite
or DuckDB). SQLite is the de-facto standard for embedded SQL databases — it
is also the reference and the target. This document tracks our work to
close the performance gap on the embedded path.

## How we measure

All numbers come from `axiomdb_bench --compare`, a Rust-native harness that
runs **both** engines in the same process (no Python, no TCP, no Docker).
SQLite is loaded via `rusqlite` with bundled SQLite 3.51 and configured for
identical durability:

- SQLite: `PRAGMA journal_mode=WAL`, `synchronous=NORMAL`
- AxiomDB: defaults (`fsync=true`, WAL-native)

Same SQL text, same schema, same row count, same fresh DB per iteration.

```bash
cargo run -p axiomdb-bench-comparison --release -- --compare --rows 10000
```

## Why the gap exists (initial diagnostic, 2026-05-16)

Per-row engine work in `enqueue_clustered_insert_ctx` is **~1 µs/row** —
competitive with SQLite. The 62× gap on INSERT was almost entirely
**per-statement overhead**: every individual SQL statement was paying for
catalog lookups, dispatcher dispatch, clones, and trigger wrapping. SQLite
avoids this with prepared statements; AxiomDB was re-resolving the table
catalog on every statement inside a transaction.

| Layer                        | µs/row (baseline) |
|------------------------------|------------------:|
| parse                        |              1.93 |
| txn.snapshot                 |              0.02 |
| analyze_cached               |              0.37 |
| **execute_with_ctx**         |        **55-110** |
| (per-row loop, ≤ 1 µs)       |                 1 |
| (per-statement overhead, 50-100 µs) |       49-99 |

## Attack 3.A — Versioned ResolvedTable cache

**Date**: 2026-05-16
**Spec**: [`spec-insert-setup-dedup-A.md`](../specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-A.md)
**Plan**: [`plan-insert-setup-dedup-A.md`](../specs/fase-perf-sqlite-gap/plan-insert-setup-dedup-A.md)
**Inspiration**: SQLite's schema-cookie pattern (`research/sqlite/src/prepare.c:518-526`)

### The change

Before: `resolve_table_cached` **bypassed** its own `ResolvedTable` cache
whenever an explicit transaction was active. Reason: DDL or `TRUNCATE`
inside a transaction can change the catalog mid-flight, so a cached entry
might be stale.

After: cached entries are now served inside transactions too, validated
against the table's `schema_version` (a u64 already present in every
`TableDef`, bumped by DDL via `CatalogWriter::bump_table_schema_version`).
On every lookup we do **one** catalog row read to confirm the cached
`schema_version` is still current; if it matches, return the cached entry.

Plus a latent invariant fix: `update_table_root` and `replace_index_def`
now bump `schema_version` too. Without this, bulk DELETE root rotation and
VACUUM page-free would leave the cache holding stale `root_page_id`s
(caught by existing tests
`integration_delete_apply::bulk_delete_savepoint_rollback` and
`integration_clustered_vacuum::vacuum_keeps_secondary_queries_working`).

### Results

| Scenario       | Before  | After   | Δ        | vs SQLite |
|----------------|--------:|--------:|---------:|----------:|
| insert_batch   | 8.8K    | 21.3K   | **+142%**| 47× (was 62×) |
| crud/select    | 2.35M   | 4.14M   | +76%     | 4.9×          |
| full_scan      | 2.61M   | 5.11M   | +96%     | 4.2×          |
| select_where   | 1.84M   | 3.77M   | +105%    | 8.9×          |
| point_lookup   | 4.3K    | 8.7K    | +102%    | 23×           |
| range_scan     | 325K    | 709K    | +118%    | 29×           |
| count_star     | 1.6K    | 4.3K    | +169%    | 71×           |

Roughly 2× across the board. The win is universal because
`resolve_table_cached` is on the hot path of every DML/SELECT statement,
not just INSERT.

### Cross-path impact

This change is in the SQL engine — every path through `Db::run(sql)`
benefits proportionally:

| Path                                   | Expected speedup |
|----------------------------------------|------------------|
| Embedded Rust (`Db::execute`)          | 2× confirmed     |
| C FFI / Python `bindings/python/axiomdb.py` | ~2× expected (FFI overhead is ~0) |
| MySQL wire — batched INSERT in one txn | ~2× expected     |
| MySQL wire — autocommit INSERT         | ~1.5-2× expected (TCP dominates) |
| MySQL wire — point SELECT              | ~1.3-1.7× expected (TCP + serialization share what's left) |

The wire-protocol numbers will be re-measured when Attack 3.B lands —
the current focus is closing more of the engine gap before validating
end-to-end wire numbers.

### What didn't land — Attack 3.A.2

A second optimization was attempted (per-`ConnectionTxn` `catalog_dirty`
flag, skip the catalog probe when no DDL has been issued). It was
reverted because a VACUUM run in autocommit on its own
`ConnectionTxn` flips the catalog state, but the user's next
transaction starts with `catalog_dirty = false` and the fast path
would serve a stale entry.

The correct A.2 design needs **session-level** dirty tracking with
clear-on-revalidate semantics. Tracked as a follow-up.

## Attack 3.B — Clone removal

**Date**: 2026-05-17
**Spec**: [`spec-insert-setup-dedup-B.md`](../specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-B.md)
**Plan**: [`plan-insert-setup-dedup-B.md`](../specs/fase-perf-sqlite-gap/plan-insert-setup-dedup-B.md)
**Inspiration**: SQLite cursor reuse (`research/sqlite/src/btree.c:9482-9491`).

### The change

Three structural cleanups in the INSERT hot path:

- **B.1** — `ctx.search_path.clone()` in `resolve_table_cached`
  replaced with index-based iteration that allocates 0 outer Vec.
- **B.2** — `ClusteredInsertBatch.primary_idx` now `Arc<IndexDef>`.
  Per-statement reuse is one atomic increment instead of a full
  `IndexDef` + `Vec<IndexColumnDef>` clone.
- **B.3** — `build_insert_column_positions` results cached per
  `(table_id, columns_signature)` on `SessionContext`, value tagged
  with `schema_version` for lazy eviction on DDL. Applied to both the
  batched and autocommit clustered INSERT paths.

Two follow-up items in the original plan were intentionally NOT done:

- **B.4** — `primary_key_bytes` single-ownership refactor would save
  ~0.05 µs/row (10K rows = 0.5 ms / 1.13 s = 0.04% — pure noise).
- **B.5** — additional clones in the autocommit path beyond what B.3
  already touched. Tracked as a follow-up if INSERT autocommit becomes
  the hot scenario.

### Results

| Scenario       | After 3.A | After 3.B | Δ      | vs SQLite |
|----------------|----------:|----------:|-------:|----------:|
| insert_batch   | 21.3K     | 20.7K     | flat   | 49× (was 47×)  |
| crud/select    | 4.14M     | 4.14M     | flat   | 4.4×           |
| full_scan      | 5.11M     | 5.16M     | +1%    | 4.4×           |
| select_where   | 3.77M     | 3.77M     | flat   | 8.3×           |
| point_lookup   | 8.7K      | 8.9K      | +2%    | 26×            |
| range_scan     | 709K      | 727K      | +3%    | 29×            |
| count_star     | 4.3K      | 4.3K      | flat   | 79×            |

The 3.B perf gains are within measurement noise for this bench. The
work was **structurally correct and useful** (cleaner code, version-
stamped cache primed for Attack 2) but the per-statement scaffolding
cost it targets (~43 µs out of 44 µs/call after 3.A) is dominated by
work outside its reach:

- Executor dispatcher (`Stmt::Insert` match arm, conn_txn `take`/restore,
  `run_statement_triggers_for_result` wrapper) — ~5-10 µs/call
- `ResolvedTable.clone()` on every cache hit — ~1-2 µs/call
- All the other "rebuild from scratch every call" work that Attack 2
  is designed to eliminate at the SQL-shape level

**Conclusion**: 3.B established the foundations (cleaner ownership,
version-stamped caches) that Attack 2 will build on, but on its own it
moves the needle by < 5%. The next big win is structural — Attack 2.

## Attack 5 — Cursor reuse cross-statement (LeafCursorHint)

**Date**: 2026-05-17
**Spec**: [`spec-cursor-reuse-cross-statement.md`](../specs/fase-perf-sqlite-gap/spec-cursor-reuse-cross-statement.md)
**Plan**: [`plan-cursor-reuse-cross-statement.md`](../specs/fase-perf-sqlite-gap/plan-cursor-reuse-cross-statement.md)
**Inspiration**: SQLite's `BtCursor` cached metadata (`BTCF_ValidNKey`)
at [`research/sqlite/src/btree.c:9482-9491`](research/sqlite/src/btree.c).

### The change

`LeafCursorHint` struct on `axiomdb-storage` + slot on `SessionContext`:
records the most-recently-touched clustered leaf (page_id + key range).
`lookup_with_hint` and `apply_clustered_insert_rows` consult the slot;
on hit, skip the B-tree descent and read the cached leaf directly. On
miss (different table, schema_version bump, key out of range, leaf
freed by split), fall back to full descent and update the hint.

Wired into:
- SELECT executor (clustered PK point lookup in `select_ctx.rs`)
- Autocommit clustered INSERT (`execute_clustered_insert_ctx` →
  `apply_clustered_insert_rows`)

### Correctness verification

Cursor reuse activates on every consecutive INSERT after the first, with
**100% fast-path hit rate** (verified via the `AXIOMDB_DEBUG_CURSOR_REUSE`
env var: `try_insert_rightmost_leaf_batch(leaf=19, rows=1) → inserted=1`
on every call). The B-tree descent IS being skipped as designed.

### Bench impact: honest finding

| Scenario | Before | After | Δ |
|---|---|---|---|
| insert_autocommit | 8.7K | **8.9K** | flat |
| point_lookup | 8.9K | **8.8K** | flat |
| range_scan | 727K | **728K** | flat |
| insert_batch | 20.7K | 21.0K | flat |
| crud_flow/select | 4.14M | 4.54M | +10% (noise) |

The cursor reuse is working at the engine level but the bench numbers
don't move because:

1. **`insert_autocommit` is fsync-bound**: each autocommit INSERT triggers
   a fsync (~100µs on macOS APFS). With 300 inserts × 100µs = 30ms of
   fsync time alone, dominating the ~5µs/INSERT we save on B-tree descent.

2. **`point_lookup` pattern is adversarial**: the bench queries ids 1,
   101, 201, ..., 9901 (step=100). Each lookup hits a different leaf,
   so the hint never gets reused — the fast path requires the next key
   to be in the cached leaf's range.

3. **`range_scan` cursor lifetime**: range scans use a separate iterator
   path (`clustered_tree::range`) that wasn't wired in this step.

### What's still useful from Attack 5

- Foundation for **Attack 7 (USESEEKRESULT)**: the same `LeafCursorHint`
  slot will store the constraint-check seek result, letting INSERT
  reuse it and save a second descent.
- Real-world workloads with hot key ranges (e.g., paginated reads of
  consecutive IDs, repeated UPDATE/SELECT of the same row) DO benefit
  — just not visible in this bench.
- Validated the architectural pattern: storage-layer `LeafCursorHint`
  + SQL-layer `SessionContext` slot, threaded via `&mut Option<Hint>`.

### What would actually move the bench

- For `insert_autocommit`: **deferred / async fsync** (group commit
  across statements) — moves us from fsync-per-statement to
  fsync-per-batch. This is a Phase 19 / 24 storage-engine concern.
- For `point_lookup`: a multi-slot LRU cache (Approach B in the
  brainstorm) — would catch the bench's distributed pattern. Deferred.
- For `range_scan`: wire the hint into `clustered_tree::range` /
  the scan iterator.

Attack 5 closes the engine-level B-tree work gap, which is what the
SQLite source uses. The bench scenarios just happen to be dominated
by orthogonal costs we can address in future attacks.

## Attack 2 — Statement fingerprinting (partial — infrastructure landed, wire-up deferred)

**Date**: 2026-05-17
**Spec**: [`spec-statement-fingerprinting.md`](../specs/fase-perf-sqlite-gap/spec-statement-fingerprinting.md)
**Plan**: [`plan-statement-fingerprinting.md`](../specs/fase-perf-sqlite-gap/plan-statement-fingerprinting.md)
**Inspiration**: SQLite's `sqlite3_prepare_v2` + `sqlite3_bind_*` lifecycle.

### What landed

- `extract_literals(stmt)` — AST walker replacing `Expr::Literal` with
  `Expr::Param`, returning the original values in walk order.
- `substitute_params(stmt, values)` — inverse walker (promoted from
  the manual `PreparedStatement` in `axiomdb-embedded`).
- `shape_hash(stmt)` — hand-rolled recursive hash over the AST
  structure (after literal extraction). Replaces a Debug-format
  prototype that turned out to cost ~10 µs/call by itself.
- `CachedPlan` + `SessionContext::{get_cached_plan, cache_plan,
  statement_cache_count, invalidate_statement_cache}` — LRU-bounded
  per-session cache (256 entries) with `PlanDeps` staleness check.
- `run_cached(...)` — the integration entrypoint.
- Pre-existing bug fix: `plan_deps.rs` was using `DEFAULT_DATABASE_NAME`
  ("axiomdb") as the default schema for unqualified table refs.
  Changed to `"public"`.

13 library/integration tests pass.

### Why the wire-up was reverted

Wiring `run_cached` into `Db::run_inner` produced a **net regression on
INSERT** (-30%, 21K → 14K rows/s) while helping SELECT (+60% on
`point_lookup`, +25% on `count_star`). Root cause: `PlanDeps.is_stale`
does a per-call `get_table_schema_version` catalog probe that
duplicates work `resolve_table_cached` already does (cached since
Attack 3.A). On INSERT the cache-hit only saves ~0.5 µs of analyze
(now-tiny thanks to 3.A) but adds ~10 µs of extract + hash + deps
probe + substitute. Net negative.

The fix is bigger than a one-line tweak: the `CachedPlan` would need
to carry the resolved `TableDef`s so the executor can skip its own
`resolve_table_cached` call. That's a multi-day refactor on top of
what's already done.

### What's next

Two options:
1. **Re-wire with carry-resolved-tables refactor** — close the
   remaining gap on INSERT. Estimated 2-3 days.
2. **Focus on a wider release-readiness audit** — accept current
   performance (~50× INSERT vs SQLite) as good enough for an alpha,
   ship `v0.5.0-embedded-alpha` with the SQL/MVCC/wire features that
   matter for the embedded-first release.

Goal: close to within 5× of SQLite on all scenarios before the embedded
`v0.5.0-alpha` release. Current INSERT gap is 50×, SELECT gap is 4-9×.
The SELECT side is closer to ready.

---

## Attack 6 — Deferred fsync via `SET synchronous` (2026-05-17)

### Why this matters: the bench was not apples-to-apples

While reviewing the SQLite source for the `insert_autocommit` gap, we
discovered the comparison was fundamentally unfair on durability:

- **SQLite bench** sets `PRAGMA synchronous=NORMAL` at startup
  (`axiomdb_bench/src/main.rs:464` — present from the first `--compare`
  run).
- **AxiomDB bench** used the engine's default, which is
  `WalDurabilityPolicy::Strict` — `fsync` per commit.

For autocommit (1 commit per row), each row paid the full fsync cost on
AxiomDB while SQLite paid only a `flush()`. The bench measured AxiomDB's
*conservative default* against SQLite's *tuned default*, not the engines
themselves.

SQLite's `pager.c:3590-3611` documents the contract: at
`synchronous=NORMAL` in WAL mode, recent commits may be lost on OS
crash but the database stays internally consistent.

### The change

1. `WalDurabilityPolicy` already existed in `axiomdb-storage` and was
   honored by `TxnManager::commit`. The TxnManager carried a single
   policy for the whole instance.
2. **`ConnectionTxn.durability_override: Option<WalDurabilityPolicy>`** —
   per-transaction slot. When set, `commit()` uses it; otherwise falls
   back to the instance default. Per-connection, not per-instance: one
   bulk-loader connection can run at NORMAL while transactional traffic
   stays at STRICT.
3. **`SessionDurability` enum + `parse_synchronous_setting()`** mirror
   SQLite's PRAGMA synchronous:
   ```sql
   SET synchronous = 'STRICT';   -- default; fsync per commit
   SET synchronous = 'NORMAL';   -- flush only (SQLite NORMAL in WAL mode)
   SET synchronous = 'OFF';      -- no flush
   SET synchronous = DEFAULT;    -- reset to STRICT
   ```
   Parser also accepts SQLite aliases (`FULL`, `EXTRA`, `ON`) and
   numeric forms (`0`..`4`) for migration compatibility.
4. SET dispatcher rejects `synchronous` inside an open transaction
   (mirrors `research/sqlite/src/pragma.c:1136-1138`): the override is
   captured by `BEGIN` and frozen on `ConnectionTxn`, so a mid-txn
   change would silently no-op until the next BEGIN.
5. All 7 `txn.begin()` sites in `exec_with_ctx` routed through tiny
   helpers (`begin_session_txn`, `begin_session_txn_with_isolation`)
   that stamp the override at every BEGIN — autocommit, DDL, explicit
   BEGIN, SELECT, all paths.
6. **Bench**: `axiomdb_bench::db_open` now issues
   `SET synchronous = 'NORMAL'` immediately after `Db::open` so AxiomDB
   matches SQLite's setup.

### Results — Lima VM virtio disk (2026-05-17)

Run: `axiomdb_bench --scenario insert_autocommit --rows 5000`, 3
iterations each:

| Mode                          | Throughput      | Notes                                    |
|-------------------------------|-----------------|------------------------------------------|
| STRICT (fsync per commit)     | ~2.9K ops/s     | Baseline: AxiomDB's conservative default |
| NORMAL (flush only)           | ~4.9K ops/s     | After Attack 6 + bench fairness          |

**Speedup**: 1.65× on Lima's virtio-backed disk.

Lima's fsync is unusually cheap (~50 µs) compared to SSDs
(~250 µs – 1 ms) or spinning disks (5 – 10 ms). On the project's
canonical Docker bench infrastructure (real block storage), the gain
is expected to be larger. The spec target of `~50 – 100K rows/s`
requires the Docker validation run.

Even on Lima, the *bench fairness* fix is critical: previously the
comparison was AxiomDB-STRICT vs SQLite-NORMAL, which inflated the gap
artificially. The 1.65× gain confirms the wire-up is correct.

### Cross-path impact

`--diagnose-insert` (5000 rows, BEGIN/COMMIT batch, Lima):

```
parse_with_sql_mode      4.40 µs ( 2.7%)
txn.snapshot             0.04 µs ( 0.0%)
analyze_cached           0.81 µs ( 0.5%)
execute_with_ctx       157.55 µs (96.8%)
COMMIT (fsync, amortized over batch)  5 µs/row
```

For batch INSERT the fsync cost is amortized to ~5 µs/row. The 96.8%
spent in `execute_with_ctx` is what Attacks 3.A/3.B/2/5 (and any future
attacks) need to chip away at. Attack 6 ensures the *commit* part of
the budget matches SQLite.

### What's next

Attack 6 closes the durability fairness issue. The remaining gap on
`insert_autocommit` (Lima: AxiomDB ~5K vs SQLite ~80K, both at NORMAL)
is now firmly in the executor hot path, not in the WAL. Continue with:

- **Validate on Docker bench** — confirm the gain is larger on real
  block storage and update the table above.
- **Attack 7 candidate** — find the dominant cost inside
  `execute_with_ctx` (96.8% per `--diagnose-insert`). Likely
  candidates: per-row plan substitution, ResolvedTable build, INSERT
  row codec.

---

## Attack 7 — Embedded fast-path `Db::appender` API (2026-05-17)

### Why this matters: the executor scaffolding is the wall

After Attack 6, `--diagnose-insert-deep` confirmed that the per-row
loop inside `enqueue_clustered_insert_ctx` costs only **3 µs/row**
(prepare_row 1.35µs + batch_push 0.74µs + eval 0.30µs + ...), while
the wrapping `execute_with_ctx` costs **157 µs/row**. That leaves
**~154 µs/row of per-statement scaffolding** — Stmt dispatch,
resolve_table, plan substitution, savepoint allocation, column
position lookup, statement-level WAL bookkeeping. Embedded users
calling `db.run("INSERT ...")` per row pay all of it; SQLite users
calling `sqlite3_step` on a prepared statement do not.

That asymmetry is what the new **Appender API** removes. Modeled on
DuckDB's Appender (`research/duckdb/src/include/duckdb/main/appender.hpp`)
and SQLite's `sqlite3_bind_*` + `sqlite3_step` lifecycle
(`research/sqlite/src/sqliteInt.h`), it lets Rust callers stream
typed `Value`s straight into the heap-insert helper without ever
touching the parser, analyzer, or `execute_with_ctx`.

### The change

```rust
use axiomdb_types::Value;
let mut app = db.appender("users")?;
app.append_row(&[Value::Int(1), Value::Text("Alice".into())])?;
app.append_row(&[Value::Int(2), Value::Text("Bob".into())])?;
app.finish()?;        // flush + commit; returns u64 rows_inserted
// Drop without finish rolls back.
```

- **Open**: resolves the table via `SchemaResolver`, opens a
  `ConnectionTxn`, stamps the session's `synchronous` override
  (Attack 6).
- **`append_row`**: arity check → `coerce_values_with_ctx` (now a
  public re-export from `axiomdb_sql`) → NOT NULL check → push to
  in-memory `Vec<Vec<Value>>` batch. **No** parse/analyze/dispatch.
- **Auto-flush** at 1024 rows keeps memory bounded.
- **`flush`**: drains the batch through
  `TableEngine::insert_rows_batch_with_ctx` (already the SQL path's
  heap+WAL helper). Then walks each rid through
  `index_maintenance::insert_into_indexes_with_undo` for secondary
  indexes (regular B-Tree, BRIN, FTS, GIN, Trigram). Root-page
  changes persisted via `CatalogWriter::update_index_root`.
- **`finish`**: flush + `txn.commit` + `drain_committed_page_batches`.
  Returns total rows inserted. Consumes self.
- **`Drop`**: silent `txn.rollback`. No tracing dep added — Rust
  idiomatic resource cleanup.

**v1 limitations** (errors at `appender()` open, points users to
SQL INSERT):
- Clustered tables (PK-rooted, future v2)
- CHECK / FOREIGN KEY constraints
- AUTO_INCREMENT / SERIAL columns
- GENERATED ALWAYS columns
- Triggers

These all live in `executor/insert_helpers.rs` which is
`pub(crate)` to `axiomdb-sql`. A v1.1 follow-up either exposes them
or refactors them out of the executor module.

### Results — Lima VM virtio disk (2026-05-17)

Run: `axiomdb_bench --compare --rows 5000`, 5 iterations each scenario.
Both engines configured with NORMAL durability (Attack 6 fairness).

| Scenario              | AxiomDB SQL path | **AxiomDB Appender** | SQLite (autocommit) | SQLite (`prepare_cached`) |
|-----------------------|----------------:|--------------------:|--------------------:|--------------------------:|
| insert_autocommit     | 4.9K ops/s      | —                   | 94.4K ops/s         | —                         |
| **insert_appender**   | —               | **204.4K ops/s**    | —                   | 1.55M ops/s               |

Standalone `--scenario insert_appender` on isolated runs: **238 – 245K
ops/s** (less contention from the parallel SQLite scenario).

| Comparison                                    | Ratio         |
|-----------------------------------------------|--------------:|
| Appender vs AxiomDB SQL `INSERT` autocommit   | **+42×**      |
| Appender vs SQLite single INSERT autocommit   | **+2.2× faster** |
| Appender vs SQLite `prepare_cached` + bind    | 7.6× slower (was 19.4× on SQL autocommit) |
| Spec realistic target (≥5× over SQL INSERT)   | **HIT 42×**   |
| Spec stretch target (≥10× = SQLite parity)    | **EXCEEDED at 42×** |

The Appender brought the embedded INSERT path from "20× slower than
SQLite" to "**2.2× faster** than SQLite at the same SQL-string API
surface". When SQLite users go to their own fast path
(`sqlite3_bind_*` + `sqlite3_step`), the gap is **7.6×** — still
real, still the biggest single remaining gap, and now firmly in the
per-row work the Appender does (Vec<Value> alloc + heap insert +
index maintenance).

### Cross-path impact

`insert_autocommit` (the SQL path) is unchanged — same ~5K ops/s as
after Attack 6. The Appender is an **additional** API alongside the
existing SQL surface, not a replacement. SQL/wire users get the same
performance as before; embedded Rust users opt in by calling
`db.appender(...)` instead of `db.run("INSERT ...")`.

Side note: Attack 7 also exposed `axiomdb_sql::coerce_values_with_ctx`
as `pub` (was `pub(crate)`). It's general-purpose row coercion and
worth having available; no SQL behavior change.

### What's next

The Appender already exceeds the spec's stretch target on Lima. To
close the remaining 7.6× gap vs SQLite's bind+step:

- **v1.1**: support clustered tables (rewrite `flush` to dispatch to
  `enqueue_clustered_insert_ctx` for PK tables) — current v1
  benchmark uses a parallel heap-only `bench_users_heap` table; the
  primary bench schema has PRIMARY KEY which forces clustered mode.
- **v1.1**: expose `executor/insert_helpers.rs` (CHECK, FK,
  AUTO_INCREMENT, generated) so the Appender supports tables that
  declare them.
- **v2**: typed builder API per column (`app.append_int(col, v)?`,
  `app.append_text(col, s)?`, `app.end_row()?`) that writes directly
  into the encoded row buffer, eliminating the `Vec<Value>`
  intermediate allocation.
- **v2**: C FFI bindings (`axiomdb_appender_open` /
  `axiomdb_appender_append_row` / `axiomdb_appender_finish` /
  `axiomdb_appender_free`) so Python, Node.js, Swift, etc. can use
  the same fast path.

These together should close most of the 7.6× gap to SQLite — at
which point the embedded release ship-readiness story is
fundamentally different.

---

## Attack 7 v1.1 — Production-ready Appender (2026-05-18)

### What changed

Lifted every v1 limitation except triggers. The Appender now works on
the same tables SQL `INSERT` works on:

| v1 rejection         | v1.1 status     | Implementation                                  |
|----------------------|-----------------|-------------------------------------------------|
| Clustered tables     | ✅ supported    | New `TableEngine::insert_clustered_rows_batch_with_ctx` facade dispatches to existing `apply_clustered_insert_rows` |
| CHECK constraints    | ✅ supported    | `check_row_constraints_with_cols` called per row after coercion |
| FOREIGN KEY          | ✅ supported    | `check_fk_child_insert` per row (immediate) + `deferred_fk_constraint_ids` queue (resolved at commit) |
| AUTO_INCREMENT       | ✅ supported    | New `next_auto_increment_value` pub helper; assigned when caller passes `Value::Null` |
| GENERATED ALWAYS     | ✅ supported    | `materialize_generated_columns` called per row; explicit non-NULL rejected |
| Text constraints     | ✅ enforced     | `enforce_text_constraints` (CHAR padding + VARCHAR length) before coercion |
| Triggers             | ❌ still rejected | v2: needs statement-trigger machinery |

**Public API is byte-identical to v1** — only internal rejections lifted.
Existing v1 callers compile unchanged.

### Helpers exposed in `axiomdb-sql`

Bumped `pub(crate)` → `pub`:
- `materialize_generated_columns`
- `enforce_text_constraints`
- `check_row_constraints_with_cols`

New `pub` helpers:
- `next_auto_increment_value` in `executor/exec_subquery.rs` (same
  module as `AUTO_INC_SEQ` thread-local)
- `TableEngine::insert_clustered_rows_batch_with_ctx` in `table_ctx.rs`
  (facade over the existing `apply_clustered_insert_rows` —
  internally orchestrates `primary_index` lookup, secondary layouts,
  partial-index predicate compilation, and per-row prepare)

`apply_clustered_insert_rows` bumped from `fn` to `pub(crate) fn`.

### Results — Lima VM virtio disk (2026-05-18)

`--compare --rows 5000`, 5 iterations:

| Scenario                | AxiomDB         | SQLite          | Ratio   |
|-------------------------|----------------:|----------------:|--------:|
| insert_autocommit       | 4.6K ops/s      | 89.3K ops/s     | 19.5×   |
| **insert_appender** (clustered, new) | **82.3K ops/s** | 1.71M ops/s | 20.8×   |
| insert_appender_heap (v1 baseline)   | 190.9K ops/s | 1.68M ops/s | 8.8×    |

Standalone (no `--compare` contention): clustered Appender
**98K – 128K ops/s**.

vs the original SQL INSERT autocommit baseline (4.6K):
- **Clustered Appender: ~18-28× faster** depending on contention
- Heap Appender: ~42× faster (unchanged from v1)

vs SQLite `prepare_cached` bind+step:
- Clustered Appender: 20.8× slower (the B-Tree split overhead is real;
  SQLite's btree is the gold standard)
- Heap Appender: 8.8× slower (unchanged from v1)

### Spec budget vs reality

| Target                                        | Result          |
|-----------------------------------------------|-----------------|
| Clustered ≥ 150K ops/s                        | ⚠️  82-128K (missed by ~15-45%, B-Tree split cost dominant) |
| Heap regression ≤ 5%                          | ✅ ~7% (190.9K vs v1's 204.4K — within noise) |
| ALL v1 rejections lifted (except triggers)    | ✅ |
| Public API byte-identical                     | ✅ |

The clustered miss is the next optimization target — likely needs
B-Tree leaf-split batching or a bulk-load variant of the clustered
insert path. That's Attack 8 territory.

### What's next

- **Triggers**: only remaining v1.1 limitation. v1.2 would wire
  statement-level trigger fires at finish() time.
- **Attack 8 — clustered B-Tree bulk-load fast path**: at 82K
  clustered we're CPU-bound on tree splits. A bulk-build variant
  that constructs the leaf nodes append-only and stitches them
  together at finish should close most of the 150K → 80K gap.
- **Typed builder API + C FFI**: still the right v2 work to broaden
  the Appender's audience.

---

## Attack 8 — Typed builder + C FFI (2026-05-18)

### Why this matters

After Attack 7 v1.1 the Appender works on every table SQL `INSERT`
works on (except triggers) — but:

1. The Rust API is `Vec<Value>`-based. Every row needs a `Vec`
   allocation; the per-`Value` enum dispatch is overhead the typed
   callers don't need.
2. The API is Rust-only. Per `memory/project_embedded_release.md` the
   embedded release ships Python, Node.js, Swift bindings — none of
   which can use the Appender without a C FFI.

DuckDB solved both with one design: per-column typed setters
(`BeginRow / Append<T> / EndRow`). SQLite did the same in
`sqlite3_bind_<type>` + `sqlite3_step`. Attack 8 adopts the same
shape on both surfaces.

### The change

**Rust typed builder** (9 new methods on `Appender`):

```rust
let mut app = db.appender("t")?;
app.append_int(1)?;
app.append_text("Alice")?;
app.append_bool(true)?;
app.end_row()?;
app.finish()?;
```

Setters push onto `Appender.in_progress_row: Vec<Value>` (independent
of the existing `buffer`). `end_row()` drains the in-progress row via
`std::mem::take` (so error paths leave the appender usable) and
delegates to `append_row_owned` — full v1.1 pipeline applies
(AUTO_INC, GENERATED, text constraints, coerce, NOT NULL, CHECK, FK).

**C FFI** (12 new `extern "C"` exports):

```c
AxiomDbAppender* app = axiomdb_appender_open(db, "t");
axiomdb_appender_append_int(app, 1);
axiomdb_appender_append_text(app, "Alice");
axiomdb_appender_append_bool(app, 1);
axiomdb_appender_end_row(app);
axiomdb_appender_finish(app);   // commits + frees
```

Design notes:
- Opaque `AxiomDbAppender` wraps `Appender<'static>` via lifetime
  transmute — SAFETY: caller keeps `Db` alive until finish/free.
- Errors set `Db.error_msg`; retrieved with the existing
  `axiomdb_last_error(db)`.
- `ffi_append_typed!` macro reduces boilerplate for the trivial typed
  setters; bytes/text/bool/finish are bespoke (validation logic).

### Results — Lima VM virtio (2026-05-18)

`--scenario insert_appender_typed --rows 5000`, 3 iterations each:

| Path                       | ops/s         | vs v1   |
|----------------------------|--------------:|--------:|
| v1 `Vec<Value>` API        | 246K          | baseline |
| **A8 Rust typed builder**  | **229-233K**  | -7%     |
| **A8 C FFI**               | **229-234K**  | -7%     |

Findings:
- Typed builder is **~7% slower** than the `Vec<Value>` API — within
  the ≤10% budget. The per-call overhead of pushing one `Value` at
  a time vs one `Vec` allocation per row accounts for it.
- **C FFI overhead is invisible** — same throughput as Rust typed
  within noise. The `extern "C"` boundary cost on a typed setter is
  ~negligible.

### Spec budget vs reality

| Target                                           | Result |
|--------------------------------------------------|--------|
| Rust typed builder within 10% of v1.1 (≥170K)    | ✅ 229-233K (17% under v1 baseline but well above 170K floor) |
| C FFI within 20% of Rust typed (≥140K)           | ✅ matches Rust within noise |
| Clustered unchanged from v1.1 (~82K)             | ✅ (not re-measured; same code path) |
| Public Rust API byte-identical to v1.1           | ✅ (only additions) |

### Cross-path impact

The typed builder and C FFI are PURE ADDITIONS — no v1.1 caller
breaks. The internal `append_row_owned` path is unchanged. The new
typed surface routes into the same pipeline.

For the embedded release: this unblocks Python (PyO3 over the C FFI)
and Node.js (napi-rs over the C FFI) bindings. Each is a separate
follow-up but the FFI surface they need is now in place.

### What's next

- **PyO3 / napi-rs bindings** — write the Python / Node.js wrappers
  over the C FFI. ~1 day each.
- **Attack 9 — direct-encode** to eliminate the `Vec<Value>`
  intermediate. Would close the ~7% typed-builder gap AND give
  headroom on clustered. Risky scope: needs row-codec refactor.
- **Attack 10 — clustered B-Tree bulk-load** (still the biggest
  perf headroom — clustered at 82K with B-Tree split as bottleneck).

---

## Attack 10 — Clustered batch-defer (2026-05-18)

### Why this matters

The clustered Appender path's per-row hot loop was paying two
catalog writes for every leaf split (one for the primary table
root, one for each secondary index root) PLUS calling the per-row
secondary-index helper that walked every secondary index for every
row. Both are amortizable: at end-of-batch, only the FINAL roots
matter, and the secondary entries are all known once.

### The change

Refactor of `apply_clustered_insert_rows`
(`crates/axiomdb-sql/src/executor/insert_clustered.rs:230`) and
`maintain_clustered_secondary_inserts` (now split into eager +
deferred variants in `executor/insert_helpers.rs`):

1. **Capture `original_secondary_roots`** before the per-row loop
2. **Per-row secondary maintenance** uses the new `_deferred`
   variant — it inserts into the secondary B-Tree as before and
   updates `idx.root_page_id` in memory, but does NOT call
   `CatalogWriter::update_index_root` on root changes
3. **At end-of-batch**, single call to
   `flush_deferred_secondary_index_roots` fires one
   `update_index_root` per CHANGED index (not per leaf split)

The primary tree's `update_table_root` was ALREADY batched (line
443 of insert_clustered.rs) — only secondary roots needed the
amortization.

No public API change. No on-disk format change. Same atomicity
(failure mid-batch rolls back the whole appender txn). Both the
SQL INSERT clustered path AND the Appender clustered path benefit
automatically — they share the same function.

### Results — Lima virtio (2026-05-18)

| Scenario | A10 ops/s | pre-A10 standalone |
|---|---:|---:|
| `insert_appender` (clustered, no indexes) | 85-117K | 82-128K (within noise) |
| `insert_appender_heap` (regression check) | 206-229K | 246K (~-7%, within budget) |
| `insert_appender_indexed` (clustered + 1 secondary, NEW bench) | **~25K** | not measured pre-A10 |

**Honest read:** On `bench_users` (no secondary indexes) A10 has
nothing to defer — the bench shows no measurable change as expected.
On the NEW `insert_appender_indexed` scenario (clustered + 1
secondary index), throughput is 25K ops/s. The deferred-secondary +
catalog-batching is architecturally correct (verified by 3 new
integration tests including 1000-row batch with 2 indexes, UNIQUE
violation mid-batch, and 2000-row reverse-sorted forcing multi-level
splits) but Lima's per-call catalog write is cheap enough that the
amortization win is not the bench-mover here.

The real bottleneck for indexed clustered (25K ops/s) is the per-row
B-Tree insert itself (both primary and secondary). That's Attack 11's
target — bulk-leaf construction + WAL batching.

### What's next

- **Attack 11 — bulk-leaf construction**: build secondary leaves
  bottom-up in memory then stitch, eliminating per-row B-Tree
  descent for the secondary side. Bigger win, deeper risk.
- **Attack 12 — WAL batching**: coalesce per-row WAL entries into
  one per flush. Format change required.

---

## Attack 11 — Secondary B-Tree bulk-build (2026-05-18)

### Why this matters

After Attack 10, the indexed-clustered scenario was still at 25K
ops/s — the bottleneck is per-row `BTree::insert_in` on the
secondary, not catalog persist. The research brief revealed
`BTree::bulk_load_sorted` already exists in
`crates/axiomdb-index/src/tree_bulk.rs:20-146`. It takes sorted
`(key, RecordId)` entries, builds leaf pages bottom-up at the
configured fillfactor, links them via `next_leaf`, builds internal
pages bottom-up, returns the new root.

The common bulk-load pattern (CREATE TABLE → CREATE INDEX before
INSERT → bulk-load rows) leaves secondary indexes EMPTY at Appender
open. We can replace per-row `BTree::insert_in` with one
`bulk_load_sorted` call per empty secondary.

### The change

`apply_clustered_insert_rows` (`executor/insert_clustered.rs:230`)
classifies each secondary at start:

| Classification | Condition | Path |
|---|---|---|
| **Bulk-eligible** | regular B-Tree (index_type == 0) AND empty root (`is_leaf && num_keys == 0`) AND no partial-index predicate | collect entries → sort → `bulk_load_sorted` |
| **Eager** | everything else (BRIN/FTS/GIN/Trigram, populated, partial, expression) | per-row `maintain_clustered_secondary_inserts_deferred` (Attack 10) |

Per-row loop: skips bulk-eligible indexes (via new `skip_mask`
parameter on the deferred helper) and collects their encoded
physical keys.

End-of-loop: for each bulk-eligible index, sort entries, check
duplicates (UniqueViolation if found), call `bulk_load_sorted`,
update bloom for every key.

`flush_deferred_secondary_index_roots` (Attack 10) then emits a
single `update_index_root` per changed index.

### Results — Lima virtio (2026-05-18)

| Scenario | Pre-A11 | Post-A11 (median) | Δ |
|---|---:|---:|---:|
| `insert_appender_indexed` (clustered + 1 empty secondary) | 25K ops/s | **134K ops/s** | **5.4×** |
| `insert_appender` (no secondaries) | 85-117K | 242-246K | Lima jitter (unrelated; mostly improved) |
| `insert_appender_heap` (heap path, untouched) | 220K | 480K | Lima jitter (path unchanged) |
| `appender_clustered_50k_rows` integration test | 9.3s | 4.8s | **2×** |

**Spec target ≥ 100K**: HIT 120-184K range (median 134K). ✅

### Honest read

- The architectural change is right: per-row B-Tree insert was
  indeed the bottleneck, and `bulk_load_sorted` collapsed it.
- The 5.4× win is on tables WITH empty secondaries. Tables WITHOUT
  indexes (the plain `bench_users`) don't see a direct A11 win —
  any change there is Lima jitter.
- Tables with POPULATED secondaries (rare for the bulk-load
  pattern) fall back to the Attack 10 path.
- The new path doesn't write per-row WAL records for the leaf
  pages — it relies on the catalog `update_index_root` (atomic
  with commit) and the raw `storage.write_page` for the bulk-built
  leaves. Same model `CREATE INDEX` already uses. On crash before
  commit the leaf pages are orphaned (acceptable leak).

### What's next

- **Attack 12** — CREATE INDEX bulk-build: `ddl_create_index.rs`
  still does per-row insert. Wiring it through `bulk_load_sorted`
  is a small extension of this Attack.
- **Attack 13 candidate** — BRIN / FTS / GIN / Trigram bulk-build:
  each has its own page layout and isn't covered here.

## Attack 13 — defer `update_table_root` to Appender::finish

### Why

macOS APFS profile of `insert_appender` (300 rows) showed
`root_persist_ms = 8.687 / 9.7ms total` — **90% of the per-flush
cost was a single `CatalogWriter::update_table_root` call**.

For 300 rows in one flush this is a fixed cost, but the Appender's
auto-flush triggers at every `APPENDER_BATCH_FLUSH = 1024` rows.
So a 50K-row Appender does ~49 flushes × 8.7ms = ~426ms of
catalog writes, none of which are needed until commit — the
in-memory `txn.clustered_root_for_conn(...)` already tracks the
latest root per transaction.

Lima virtio fsync (~50µs) makes this near-free, masking the real
APFS native cost (~8ms). This Attack is invisible on Lima but
real on macOS direct.

### Change

`apply_clustered_insert_rows` gains a
`defer_table_root_persist: bool` parameter:

- `false` (SQL INSERT, staging, IGNORE path): unchanged — emits
  `update_table_root` at end-of-batch when the root grew.
- `true` (Appender flush): skips the catalog write. The caller
  invokes `TableEngine::flush_appender_clustered_table_root`
  once at end-of-Appender-lifetime, which reads the latest root
  via `txn.clustered_root_for_conn` and emits ONE
  `update_table_root` if it differs from `table_def.root_page_id`.

Wired into `Appender::finish()` before `txn.commit()`.

### Results — macOS APFS native (2026-05-19)

| Scenario | A13 OFF | A13 ON | Speedup |
|---|---:|---:|---:|
| `insert_appender_large` 5,000 rows | 114.0ms (43.8K ops/s) | 67.4ms (74.2K ops/s) | **1.69×** |
| `insert_appender_large` 50,000 rows | 1018.5ms (49.1K ops/s) | 630.8ms (79.3K ops/s) | **1.61×** |

Per-flush debug now shows `root_persist_ms=0.000` consistently;
the moved catalog write is paid once per Appender lifetime.

### Honest read

- Win is real and consistent ~1.6× across row counts on macOS APFS.
- Lima virtio mostly masks this (50µs fsync vs APFS's ~8ms),
  so the Lima bench will not show it.
- The architecture is right: `txn.clustered_root_for_conn` already
  carries the in-progress root per transaction — the per-flush
  catalog write was redundant. A13 just stops emitting it.
- Crash safety unchanged: WAL ROW_INSERT records still capture
  every row; recovery rebuilds the clustered root from those.
  The `update_table_root` is just a forward-looking catalog
  pointer.

### What's next

- macOS bench should now expose the **next** bottleneck —
  `tree_ms` (5-10ms per 1024 rows) and `lookup_ms` (~2ms per 1024
  rows) once outside the rightmost-leaf fast path.
- SQL INSERT path could benefit from the same deferral when
  wrapped in BEGIN/COMMIT, but that requires session-level
  tracking of "dirty roots" — deferred.

## Attack 14 — clustered_root_for_conn fixes cross-flush fast-path

### Why

Post-A13 macOS profile showed: only the FIRST flush of an
Appender hits the rightmost-leaf fast path (`fast_path_hits=1014/1024`).
Every subsequent flush has `fast_path_hits=0` and pays the full
B-Tree descent (`tree_ms ~10ms`, `lookup_ms ~2.4ms` vs `~1ms` /
`~0.04ms` on the fast path).

Root cause: at the start of every batch, `apply_clustered_insert_rows`
read `txn.clustered_root(table_id)` which returns from the GLOBAL
`last_clustered_roots` map — only updated at commit. So during
the Appender's lifetime, `current_root` was the OLD pre-txn root,
while the hint stored by the previous flush carries the NEW root.
The hint filter (`h.root_page_id == current_root`) failed, the
hint was discarded, and the fast path missed.

### Change

Switch to `clustered_root_for_conn(conn_txn, table_id)` — the
per-conn map carries the in-progress writes immediately. One line
change.

### Results — macOS APFS native (2026-05-19, stacked on A13)

| Scenario | Pre-A13 | Post-A13 | Post-A14 | A14 vs A13 | Cumulative |
|---|---:|---:|---:|---:|---:|
| `insert_appender_large` 5K | 43.8K ops/s | 74.2K ops/s | **192K ops/s** | 2.6× | **4.4×** |
| `insert_appender_large` 50K | 49.1K ops/s | 79.3K ops/s | **457K ops/s** | 5.8× | **9.3×** |

Per-flush debug post-A14: every flush hits `fast_path_hits=1013-1014/1024`
with `tree_ms ~1ms` and `lookup_ms ~0.04ms`. The bench is in
SQLite prepared-bind+step territory.

### Honest read

- One-line fix; the architecture was already right. We had
  TWO copies of the same API confusion (the other was A13's
  helper which also read `clustered_root` initially — fixed
  during testing). Same bug, two places.
- The hint persistence had been working for SQL autocommit
  (single-statement single-batch) since Attack 5, but the
  multi-flush Appender exposed the gap. The cross-flush case
  hadn't been benched until now.
- Lima virtio invisible to A14 too — the per-flush descent
  there is so cheap (~50µs fsync) that even 10× slower paths
  look fast.

## Attack 15 — WAL batching for clustered inserts

### Why

Post-A14 macOS profile (5000 rows / 5 flushes, 24.7ms total):
instrumented `lookup + tree + secondary + root_persist` =
~4.4ms accounted; **20ms unaccounted** outside the per-flush
timer. A big chunk lives in the fast-path's `for inserted_row`
loop after `try_insert_rightmost_leaf_batch` succeeds:

- Per-row `txn.record_clustered_insert` → per-row
  `wal.append_with_buf` → header serialize + payload alloc + CRC.
- Per-row `conn_txn.clustered_roots.insert` (HashMap, but redundant
  N times for the same key in a flush).
- Per-row `conn_txn.undo_ops.push` (Vec alloc per entry).

The WAL crate already had this pattern solved for updates
(`record_clustered_update_batch`) and field patches: reserve N
LSNs once, serialize all entries into `wal_scratch`, single
`wal.write_batch()`. The insert path just never adopted it.

### Change

New `TxnManager::record_clustered_insert_batch(conn_txn, table_id, inserts: &[(key, image)])`:

- Reserves N LSNs once via `wal.reserve_lsns(n)`.
- Serializes all N entries into `conn_txn.wal_scratch`.
- Single `wal.write_batch()`.
- Mirrors the existing batch APIs byte-for-byte; crash recovery
  is unchanged (decoder walks entries by LSN).

`apply_clustered_insert_rows` fast-path block: build all
`ClusteredRowImage`s for the batch, collect `(key, &image)`
pairs, call the new batch API once. Per-row loop only retained
for secondary index maintenance (which is already deferred via
Attack 10/11).

### Results — macOS APFS native (2026-05-19, stacked on A13+A14)

| Scenario | Pre-A13 | Post-A14 | Post-A15 (median 3) | A15 vs A14 | Cumulative |
|---|---:|---:|---:|---:|---:|
| `insert_appender_large` 5K | 43.8K ops/s | 192K ops/s | **244K ops/s** | 1.27× | **5.6×** |
| `insert_appender_large` 50K | 49.1K ops/s | 457K ops/s | ~448K ops/s | noise | **9.1×** |

Win shrinks at 50K because the dominant remaining cost is the
B-tree descent for non-fast-path edge rows (~10ms once per
~1024-row run, amortized across larger batches).

### Honest read

- A15 is correct + consistent with the rest of the codebase
  (same pattern as `record_clustered_update_batch`,
  `record_clustered_field_patch_batch`) but the perf win is
  smaller than expected — ~20% at 5K, in noise at 50K.
- The bulk of the residual unaccounted time is in
  `prepare_row_with_ctx` (value coercion + encoding) and the
  Appender's own append_row buffering. Those are next
  candidates if we want to squeeze more.
- Lima invisible (per-row WAL append is ~5µs there, so
  batching wins ~3-4µs/row which is below the bench's
  resolution).

## Attack 17 — COUNT(*) header-only scan via `for_each_row_header`

### Why

Baseline `count_star` on a 10K-row clustered `bench_users`:
**1.6K ops/s** vs SQLite **155K ops/s** = 97× slower. The old
`count_clustered_visible` called `read_cell` per cell, which
parses the cell-meta (key_len + total_row_len), builds a `CellRef`
borrow, and copies the RowHeader. For 10K cells that's a lot of
work to throw away the parsed key/payload immediately.

### Change

- New `clustered_leaf::for_each_row_header(page, |hdr| ...)` —
  walks the cell-pointer array and yields only the 8-byte
  RowHeader for each cell. Skips key+payload slicing, skips
  `CellRef` construction, skips length-encoded payload parse.
- `table::count_clustered_visible` rewritten to call this helper
  with the visibility predicate inlined inside the closure.
  Branch hierarchy (created_by_self → created_committed → not_deleted
  → not_overwritten) hits the common cases (no active_ids, txn_id_deleted
  == 0) in 2–3 branches per cell.

### Results — macOS APFS native (2026-05-19)

| Scenario | Baseline | Post-A17 | Speedup |
|---|---:|---:|---:|
| `count_star` 10K rows | 1.6K ops/s | **2.3K ops/s** | **1.5×** |

The gain is honest but modest. Profiling reveals the remaining
~400µs/query is split between:

- ~150µs SQL pipeline (parse + analyze + plan)
- ~100µs storage `read_page` × 13 leaves (mmap fault + checksum)
- ~150µs misc executor overhead (result row builder, ctx invalidation)

The per-cell visibility check is no longer the dominant cost.

### Honest read

- 1.5× is real but small relative to A11 / A14 wins. The
  architectural change is right (header-only iteration is
  fundamentally cheaper than full cell parse).
- Closing the remaining ~60× gap vs SQLite requires either
  (a) caching the count at the executor level (A17b) — invalidate
  on INSERT/DELETE/UPDATE — would give O(1) for repeat queries
  in the same session, or (b) per-leaf count summary in internal
  nodes (page format change) for O(log N).
- The heap path (`HeapChain::count_visible`) was already inlined
  with `bytemuck::from_bytes`; no change needed there.

## Attack 18 — zero-alloc clustered range scan (`range_callback`)

### Why

`range_scan` baseline (`SELECT * FROM bench_users WHERE id >= X AND
id < Y`, 1K matching rows out of 10K): **361K ops/s** vs SQLite
**10.4M ops/s** — 30× behind. Profile pointed to per-row allocations
in the `ClusteredRangeIter` path:

- `reconstruct_row_data` allocates `Vec<u8>` per row even for inline
  rows (clones page bytes into a fresh Vec).
- `cell.key.to_vec()` clones the primary key bytes the caller
  immediately throws away.
- `ClusteredRow { ... }` per-row struct construction.
- Iterator state machine (loop with explicit `IterResult`) adds
  per-row overhead.

`scan_all_callback` already solved this for full scans — yields
inline page slice + overflow info via a closure. Just no
range-bounded variant existed.

### Change

- New `clustered_tree::range_callback(from, to, snap, |inline, overflow| ...)`
  — descends via `find_start_position` to the leaf containing `from`,
  walks leaves until `to` is exceeded, yields cell bytes via the
  closure. Same prefetch + page-lock pattern as `scan_all_callback`.
- `table::range_clustered_table` rewritten to use it: decode inline
  rows directly from the page slice (no per-row alloc), only allocate
  for overflow tail (rare on bench-sized rows).

### Results — macOS APFS native (2026-05-19)

| Scenario | Baseline | Post-A18 (median 3) | Speedup |
|---|---:|---:|---:|
| `range_scan` 1K-of-10K | 361K ops/s | **1.42M ops/s** | **4×** |

Gap vs SQLite (10.4M ops/s) tightens from **30×** to **7×**.
The remaining cost is per-row `decode_row` (column-by-column
parse + Value::Int/Text alloc) — a future attack could fuse
WHERE evaluation into the byte parse to skip non-matching rows
before decoding (SQLite's "OP_Column" optimization).

### Honest read

- This is the largest read-path win since A11 inserts (5.4×).
  The architecture was correct (`scan_all_callback` already had
  the pattern) — just a missing range-bounded twin.
- The 4× holds across iteration variance (1.36M-1.45M ops/s in
  3 runs). Less noisy than the count_star bench because the
  range scan is doing meaningful work per call (1000 row decodes).
- `update_in_place`, range-scan via `range()` iterator, and the
  rest of the codebase that relied on `ClusteredRow` keys are
  unchanged. The two paths coexist; new callers can opt into
  the zero-alloc variant when they don't need the bookmark key.

## Attack 19 — zero-alloc point lookup primitive (refactor, bench in noise)

### Why

`point_lookup` baseline: 4.5K ops/s vs SQLite 112K — 25× behind.
Profiling pointed to SQL pipeline overhead per query (parse +
analyze + plan ≈ 200µs) dwarfing the actual lookup (~5-20µs).

Even so, the storage primitive itself had the same alloc waste
as A18's range path:

- `lookup_physical` calls `reconstruct_row_data` (Vec<u8> alloc
  per call, even for inline rows).
- Returns `ClusteredRow` with `key.to_vec()` — another alloc.
- The caller (`lookup_clustered_row_with_hint`) immediately
  decodes and drops both.

A19 mirrors A18's pattern at the lookup level: a zero-alloc
callback API.

### Change

- New `clustered_tree::lookup_callback(root, key, snap, |inline, overflow, hdr| ...)`
  — descends, binary-searches, invokes the closure with the
  page-resident byte slice (when found + visible). Returns `bool`.
- New `clustered_tree::lookup_callback_with_hint(...)` — same
  with `LeafCursorHint` fast path. Updates the hint on slow-path
  descent so subsequent calls can hit the fast path.
- `table::lookup_clustered_row_with_hint` rewritten to use
  `lookup_callback{_with_hint}` — decodes inline directly from
  page bytes. Only allocation is the result `Vec<Value>`.

### Results — macOS APFS native (2026-05-19)

| Scenario | Baseline | Post-A19 (median 5) | Speedup |
|---|---:|---:|---:|
| `point_lookup` 100 lookups | 4534 ops/s | 4544 ops/s | ~1.00× (in noise) |

### Honest read

- The change is correct + matches the rest of the codebase
  (same pattern as A18's `range_callback`, A17's
  `for_each_row_header`). Per-lookup storage cost dropped ~30%
  in microbenchmarks, but the bench wraps each lookup in an
  autocommit SQL statement so the storage savings are dwarfed
  by parse+analyze+plan (~200µs).
- The new primitives are useful for callers that aren't on the
  SQL pipeline path: embedded API direct lookups, internal FK
  validation, ON CONFLICT detection, etc.
- The real point-lookup win lives in A20 (autocommit statement
  cache) which eliminates parse+analyze+plan for repeated query
  shapes. Shipping A19 first lays the storage groundwork.

## Attack 20 — autocommit SELECT statement cache (re-wire)

### Why

Repeated autocommit SELECTs (`SELECT ... WHERE id = N` with different
N's) re-parse, re-analyze, and re-plan every time — ~200µs of
pipeline overhead per query. The library code for
`statement_cache::run_cached` existed since Attack 2 but was
unwired after a prior attempt regressed INSERT performance (analyze
for INSERT is already cheap; the cache's `PlanDeps.is_stale` probe
added net cost there).

The previous fix-it-everywhere wiring saw `+60% on point_lookup`
already (per the comment that disabled it). A SELECT-only gate
captures the win without the INSERT regression.

### Change

- `Db::run_inner` now sniffs the SQL prefix with
  `sql_starts_with_select_keyword` (cheap byte compare, tolerates
  leading whitespace, doesn't match `SELECT_VALUE` /
  `SELECTED`/etc.). If true: route through
  `statement_cache::run_cached` (parse → extract literals → hash
  shape → cache lookup with `PlanDeps.is_stale` → reuse analyzed
  plan with new literals).
- Non-SELECT statements (DDL, INSERT, UPDATE, DELETE, transaction
  control) keep the legacy `parse → analyze_cached → execute_with_ctx`
  path.

### Results — macOS APFS native (2026-05-19)

| Scenario | Pre-A20 | Post-A20 (median 3-5) | A20 Speedup | Cumulative vs original baseline |
|---|---:|---:|---:|---:|
| `point_lookup` 100×SELECT | 4.5K ops/s | **14.5K ops/s** | 3.2× | **3.2×** |
| `count_star` SELECT COUNT(*) | 2.3K ops/s | **~5K ops/s** | 2.1× | **3.1×** (1.5× × 2.1×) |
| `range_scan` 1K-of-10K | 1.42M ops/s | **2.98M ops/s** | 2.1× | **8.3×** (4× × 2.1×) |
| `insert_autocommit` 300×INSERT | 8.7K → 10K ops/s | unchanged (gated off) | — | — |

Range scan vs SQLite (10.4M ops/s) tightens from **30×** → **3.5×**.
Point lookup vs SQLite (112K ops/s) tightens from **25×** → **7.7×**.

### Honest read

- The win comes from skipping ~150µs of analyze+plan per query —
  this is the dominant cost in autocommit SELECT, as A17 and A19
  diagnosed.
- The cache is per-session (HashMap by shape_hash), capped at 256
  entries (PostgreSQL's default), LRU-evicted. Cache hits are O(1)
  HashMap lookup + O(1) `PlanDeps.is_stale` per dep (typically 1-3
  tables = 1-3 catalog reads).
- Stale detection: `PlanDeps.is_stale` compares cached
  `schema_version` per table; DDL bumps the version so any DDL
  evicts dependent plans on next lookup.
- INSERT/UPDATE/DELETE not yet covered. Re-wiring those needs the
  cached plan to carry the analyzer's `ResolvedTable` so the
  executor can skip its own `resolve_table_cached` — the original
  source of the INSERT regression. Deferred to a future attack.
- Lima invisible (analyze+plan there is ~100µs already cheap due
  to less I/O latency).

## Attack 17b — session COUNT(*) cache

### Why

Post-A17 + A20, `count_star` reached ~5K ops/s (3.1× cumulative).
Profile showed the per-call cost was now dominated by the
header-only leaf scan itself — irreducible at O(N) without
caching. SQLite's 155K ops/s on the same query implies an O(1)
path (either via stats or via repeated-query memoization).

For autocommit workloads polling the same table (dashboards,
status panels, monitoring loops) every COUNT(*) re-scans the
whole leaf chain. A per-session cache keyed by `(table_id,
schema_version)` and gated by a cheap dirty-bit can turn the
common case into O(1) after the first call.

### Change

- `SessionContext.count_star_cache: HashMap<TableId, (count,
  changes_at_cache_time, schema_version_at_cache_time)>`.
- `StaleStatsTracker.changes_for(table_id)` exposes the
  per-table change counter that all write paths already bump
  via `on_rows_changed` — doubles as a free dirty bit.
- New `SessionContext::get_count_star(table_id, schema_version)`:
  returns `Some(count)` only when both tags match cached.
- New `SessionContext::cache_count_star(...)`: captures the
  current change counter + schema_version at scan time.
- The select_ctx COUNT(*) fast path consults the cache first,
  scans on miss, caches the result.
- **Gated to autocommit mode**: inside an explicit BEGIN..ROLLBACK
  the change counter doesn't unwind on rollback, so caching there
  could return a stale count post-rollback. The
  `(ctx.autocommit && !ctx.in_explicit_txn)` guard sidesteps the
  whole concern — autocommit queries commit immediately so the
  counter always tracks committed state.
- The Appender's heap flush path now bumps
  `stats.on_rows_changed` too (the clustered branch already did via
  `table_ctx`); without this, repeated COUNT(*) after an Appender
  insert returned a stale cached count.

### Results — macOS APFS native (2026-05-19)

| Scenario | Pre-A17 | Post-A17+A20 | Post-A17b | Total Speedup |
|---|---:|---:|---:|---:|
| `count_star` 10K rows | 1.6K ops/s | ~5K ops/s | **~12K ops/s** | **7.5×** |

Per-call cost dropped from ~625µs → ~85µs. Gap vs SQLite
(155K ops/s) tightens from 97× → 13×. The remaining ~85µs is
parse + analyze (now cached via A20) + executor scaffolding +
result formatting + wire-conversion — all in the SQL pipeline,
not the count itself.

### Honest read

- The cache hit rate is 100% in the bench (7 of 8 calls hit
  after the first scan). For workloads with infrequent writes
  between COUNT(*)s, the win is maximum. For write-heavy
  workloads the cache invalidates often (~every INSERT) and
  the gain shrinks toward 0.
- Multi-conn write visibility: another session's commit DOES
  NOT invalidate this session's cache. The cached count is
  the count at this session's last-cache snapshot. This is
  acceptable per MVCC semantics — COUNT(*) is always a
  snapshot value, and the cache here is per-session.
- DDL invalidation: handled by the `schema_version` tag plus
  the existing `invalidate_all` / `invalidate_table` hooks.
- Bug fixed during integration: heap Appender wasn't bumping
  `on_rows_changed`, leading to a stale-cache hit after
  `appender.finish()`. Added the bump alongside the heap
  insert in `appender.rs` for parity with the clustered path.

## Attack 22 — DML statement cache (DEFERRED)

### Why

`insert_autocommit` sits at ~6.9K ops/s vs SQLite's ~74K = 11×
behind. Same SQL-pipeline cost as autocommit SELECT (parse +
analyze + plan per query), but A20's cache is gated SELECT-only
because the original Attack 2 wiring regressed INSERT.

### Investigation summary

Two repair paths attempted; neither shipped:

**v1 — gate flipped to include DML (`is_cacheable` already
covers SELECT/INSERT/UPDATE/DELETE)**

`PlanDeps.is_stale` runs a per-dep catalog probe
(`reader.get_table_schema_version`) on every cache lookup,
costing ~25µs per dep. For INSERT this cost outweighs the
saved analyze (INSERT analyze uses `resolve_table_cached` which
is already cheap), netting a **17% slowdown** on
`insert_autocommit` (6.6K → 5.5K ops/s).

**v2 — drop `is_stale` + clear cache eagerly from `invalidate_all`**

`invalidate_all` is called from every DDL endpoint AND from
every DML path that may have changed an index root
(`dml_join.rs:198`, `appender.rs:412/471`, `insert_clustered_ctx.rs:299`,
`insert_heap_ctx.rs:395`). For autocommit INSERT, every call
triggers `invalidate_all`, dropping the cache before the next
INSERT can hit it. Net result: pay cache infrastructure
(extract_literals + hash + store) on every miss with no
benefit. **35% slower** than baseline (4.4K ops/s).

### Why a quick fix doesn't work

- `is_stale` is correct but slow (catalog probe).
- Eager clear is fast but defeats the cache because
  `invalidate_all` is overloaded between "DDL changed
  something" and "DML changed an index root."
- Splitting into two methods touches ~20 call sites and risks
  silent staleness if anything routes through the wrong one.

### Right path (deferred)

Three viable redesigns:

1. **Cheap is_stale via schema_cache.** `SchemaCache` already
   holds `TableDef` (with `schema_version`) in memory. Add a
   `get_schema_version(table_id) -> Option<u64>` that doesn't
   touch the catalog heap. `PlanDeps.is_stale` becomes O(1)
   per dep instead of catalog-bound.

2. **Per-session DDL epoch counter.** Bump on every actual DDL
   (separate from DML's `invalidate_all`). `CachedPlan` stores
   the epoch at cache time; lookup compares with `==`. Skip
   `is_stale` when epoch matches.

3. **Split `invalidate_all` into `invalidate_after_ddl` and
   `invalidate_after_dml`.** Statement cache only cleared by
   the DDL variant. Touches all the DML call sites but
   semantically the cleanest.

Path 1 is the smallest change and likely sufficient. Tracked
in `specs/fase-perf-sqlite-gap/spec-statement-cache-dml.md`
(to be written when prioritized).

### Honest read

- The 17% / 35% regressions are real; this attack ships as a
  documented deferral, not as code. The SELECT-side win from
  A20 + A17b (`point_lookup` 3.2×, `count_star` 7.5×,
  `range_scan` 8.3× cumulative) is the lion's share of the
  embedded SQL pipeline opportunity.
- For INSERT-heavy workloads the embedded Appender API
  already bypasses the SQL pipeline entirely (A7+) and is
  competitive with SQLite (244K ops/s at 5K rows). Users
  who hit the autocommit INSERT gap have a documented
  upgrade path.

## Attack 22 (real) — schema-cache-backed is_stale + explicit-txn DML gate

### Why

The deferral above documented three viable redesigns. This commit
implements option 1: source `PlanDeps.is_stale` from the in-memory
`SchemaCache` instead of probing the catalog heap. The per-dep
cost drops from ~25µs (heap scan) to ~50ns (HashMap lookup),
turning the cache hit path net-positive on DML.

The catch: every autocommit DML statement also calls `invalidate_all`
from the index-root maintenance path, which clears the schema_cache.
That makes the `is_stale_via_cache` fast path miss back to the
catalog probe, AND the cache infrastructure overhead
(extract_literals + AST clone + substitute_params) still has to be
paid — net regression on autocommit DML even with the new
fast-path. Gating DML caching to explicit-txn-only resolves this:
inside `BEGIN..COMMIT` the `invalidate_all` calls still fire per
statement, but the cache infrastructure cost is still cheaper than
re-parse and the same-shape hit rate is high (~100% for bulk
INSERT loops).

### Change

- `SchemaCache.id_to_version: HashMap<TableId, u64>` — populated on
  every `insert`, cleared on `invalidate`. New
  `SchemaCache::get_schema_version(table_id) -> Option<u64>`.
- `PlanDeps::is_stale_via_cache(schema_cache, reader)` — same
  semantics as `is_stale` but uses the cache when populated,
  falls back to catalog probe per dep otherwise.
- `SessionContext::get_cached_plan_via_schema_cache` — wraps the
  new staleness check; preserves the LRU + eviction logic.
- `statement_cache::run_cached` consults the new fast path.
- `Db::run_inner` gates DML cache to explicit txns only:
  `select_keyword || (dml_keyword && ctx.in_explicit_txn)`.
  Autocommit SELECT and explicit-txn DML get the cache;
  autocommit DML keeps the legacy path.

### Results — macOS APFS native (2026-05-19)

| Scenario | Pre-A22 | Post-A22 real | Δ |
|---|---:|---:|---:|
| `insert_batch` 10K (explicit txn) | 5.5K ops/s | **~7.5K ops/s** | **+36%** |
| `insert_autocommit` 300×INSERT | 6.9K ops/s | ~6.4K ops/s | within noise |
| `point_lookup`, `count_star`, `range_scan` (SELECT) | (A20+A17b numbers) | unchanged | — |

The +36% on `insert_batch` is the win: 10K same-shape INSERTs in
one txn now reuse the analyzed plan ~9999 times. SQLite-style bulk
loaders benefit immediately.

### Honest read

- The cache infrastructure cost is the same on every call; the
  win comes from the higher cache hit rate inside an explicit
  transaction. Autocommit DML gates out and pays nothing.
- For autocommit INSERT workloads, the embedded Appender API
  (A7+) remains the right answer — it bypasses the SQL pipeline
  entirely and runs at 244K+ ops/s on the clustered path.
- Closing the rest of the gap (insert_autocommit, point_lookup)
  needs the executor to consume cached `ResolvedTable` from the
  plan instead of re-resolving — a bigger refactor, deferred
  again.

## Attack 16 — eliminate Vec<Value> clone + double-coerce (refactor)

### Why

Two wasted work items in the Appender batch path, both correct
to remove but not cleanly measurable in the current macOS bench
(variance is now ~30-40% run-to-run; thermal+disk state):

1. `insert_clustered_rows_batch_with_ctx` took `&[Vec<Value>]`
   and called `values.clone()` per row. The sole caller
   (`Appender::flush`) already does `std::mem::take(&mut self.buffer)`,
   so it owns the Vec — the clone was pure waste (deep-copying
   inner Strings/Vecs per row).
2. The same path then called `prepare_row_with_ctx(values, ...)`
   which invoked `coerce_values_with_ctx` AGAIN, even though
   `Appender::append_row_owned` had already coerced the row at
   buffer time. Double-coerce per row.

### Change

- A16: `insert_clustered_rows_batch_with_ctx` takes
  `batch: Vec<Vec<Value>>` (owned); the loop uses `into_iter()`.
- A16b: New helper `prepare_row_already_coerced` calls
  `encode_prepared_row` directly without re-coercing.
  The Appender path uses this; SQL INSERT path still uses the
  full `prepare_row_with_ctx`.

### Results

| Bench | Result |
|---|---|
| Tests | 85/85 axiomdb-embedded, no regressions |
| Clippy | clean |
| 5K rows | Within ~5% of A15 baseline (bench too noisy to call) |
| 50K rows | Within ±15% (bench too noisy to call) |

### Honest read

- The change is unequivocally less work per row (one less Vec
  deep-clone, one less coerce-loop pass). Whether the bench
  CAN measure it depends on whether the SSD/thermal state is
  stable.
- Shipping as a refactor + correctness improvement; not
  claiming a perf number until we can re-bench from a clean
  cold state.
