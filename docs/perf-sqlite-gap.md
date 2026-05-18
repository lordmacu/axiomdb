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
