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
