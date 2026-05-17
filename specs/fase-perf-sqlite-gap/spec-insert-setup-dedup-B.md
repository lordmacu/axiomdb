# Spec: insert-setup-dedup-B — clone removal in INSERT hot path

Phase: perf-sqlite-gap — close embedded INSERT gap with SQLite
Task: B — eliminate per-statement and per-row clones in the INSERT path
Status: approved

## Context

Attack 3.A landed a ~2× across-the-board win by enabling the
`ResolvedTable` cache inside transactions
(`spec-insert-setup-dedup-A.md`, commit `50930d99`). The
`execute_with_ctx` per-call cost dropped from 55-110 µs to **~44 µs**,
still well above the spec's ≤ 25 µs target. The remaining gap is small
per-statement and per-row allocations that look insignificant
individually but compound at 10K iterations.

A grep audit of the INSERT hot path
(`crates/axiomdb-sql/src/executor/{insert_clustered_ctx.rs,shared.rs}`)
shows 10+ clones in functions called per statement, plus 2 clones per
row. Attack 3.B removes them by:

1. Replacing per-call `Vec.clone()` / `to_vec()` with `&[T]` references
   where ownership is not needed.
2. Caching derived per-table data (`col_positions`) in `SessionContext`
   keyed by `table_id`.
3. Wrapping `IndexDef` and `ResolvedTable` slots in `Arc<T>` for
   cheap shared ownership across the batch lifetime.
4. Taking ownership of `primary_key_bytes` once per row in the batch
   push, instead of cloning twice.

SQLite's btree-cursor reuse pattern
(`research/sqlite/src/btree.c:9482-9491` — `BTCF_ValidNKey` fast path)
is the inspiration: keep derived state alive across statements,
re-validate cheaply, never rebuild from scratch on the hot path.

## Goal

Eliminate every clone or allocation in the per-statement and per-row
INSERT path that does NOT contribute to user-visible output, without
changing any public API or correctness invariant. Target ≥ 30% extra
throughput on top of Attack 3.A.

## Non-goals

- Replacing the per-row `prepare_row (codec+PK)` work (the 0.6 µs
  inside the per-row loop). That is competitive with SQLite and is
  attacked separately if needed (Attack 4).
- Statement fingerprinting / SQL-text plan cache (Attack 2).
- Session-level `catalog_dirty` for skipping the catalog probe (the
  deferred Attack 3.A.2).
- Cross-statement cursor reuse à la SQLite `BTCF_ValidNKey`. Separate
  follow-up.
- Restructuring `enqueue_clustered_insert_ctx` into smaller functions.
  Mechanical clone removal only.

## Behavior

### Public API

No new public API. Internal refactors only.

`SessionContext` gains one cache slot (additive):

```rust
// crates/axiomdb-sql/src/session.rs
impl SessionContext {
    /// Returns the cached column positions for `table_id` and the
    /// statement-supplied `columns` list, computed by
    /// `build_insert_column_positions`. Returns `None` if no entry
    /// or if `columns_signature` differs from the cached signature.
    pub fn get_insert_col_positions(
        &self,
        table_id: u32,
        columns_signature: u64,
    ) -> Option<&Vec<usize>>;

    /// Caches `col_positions` under `(table_id, columns_signature)`.
    /// Evicted lazily when the table's schema_version changes.
    pub fn cache_insert_col_positions(
        &mut self,
        table_id: u32,
        columns_signature: u64,
        col_positions: Vec<usize>,
    );
}
```

`columns_signature` is a 64-bit hash of the statement's column list
(or 0 when the list is `None` = all columns in declaration order),
so different `INSERT INTO t(a,b) VALUES…` vs `INSERT INTO t(a,c) …`
do not collide.

### Semantics

For each clone targeted, the new behavior is functionally identical to
the old one — same `ResolvedTable`, same `col_positions`, same
`primary_idx` passed to `prepare_row_with_ctx`. The only observable
difference is fewer allocations and no extra refcounts during the hot
path.

**Cache invalidation** for `col_positions`: tied to the table's
`schema_version`. When `resolve_table_cached` evicts the
`ResolvedTable` entry for a table (because `schema_version` changed),
it ALSO evicts the matching `col_positions` entries for that table.
This is one extra HashMap call on the slow path; no cost on the cache-hit
path.

### Error cases

No new error cases. Existing errors (TableNotFound, ColumnNotFound,
NotImplemented for REPLACE/ODKU/OnConflict on clustered) all flow
through unchanged.

### Cross-path impact

| Path | Expected speedup |
|------|------------------|
| Embedded Rust (`Db::execute`) | 1.3-1.5× on INSERT batched path |
| C FFI / Python | same as embedded (FFI overhead ≈ 0) |
| MySQL wire — INSERT batch in one txn | 1.3-1.5× expected |
| MySQL wire — autocommit INSERT | 1.2-1.3× (TCP still dominates) |
| SELECT / COUNT / GROUP BY | small (search_path clone helps but those paths have less setup) |

## Edge cases

Each becomes a test case in the plan:

- [ ] `INSERT INTO t VALUES (...)` (no column list) reuses cached
  `col_positions` after first call.
- [ ] `INSERT INTO t(a,b) VALUES (...)` and `INSERT INTO t(a,c) VALUES (...)`
  do NOT share a cache entry (different signature).
- [ ] `ALTER TABLE t ADD COLUMN ...` evicts cached `col_positions` for
  table `t` (the schema_version bump cascades).
- [ ] `DROP TABLE t` evicts cached `col_positions` (via the
  `invalidate_table` call).
- [ ] `INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')` (multi-row
  VALUES) — single call, single `col_positions` cache lookup; per-row
  loop allocates nothing extra.
- [ ] Two consecutive INSERTs into the SAME table reuse all cached
  state (col_positions + ResolvedTable).
- [ ] Two consecutive INSERTs into DIFFERENT tables get distinct
  cache entries — no aliasing bugs.
- [ ] `enqueue_clustered_insert_ctx` followed by another statement
  flushes the batch correctly (the new shared-ownership pattern must
  not pin the batch open).

## On-disk format

No on-disk format change.

## Performance budget

Baseline (post-Attack-3.A): `execute_with_ctx` per row = **~44 µs**,
INSERT batched throughput = **21K rows/s**.

| Metric | Today | Target after B |
|--------|------|---------------:|
| `execute_with_ctx` per row | ~44 µs | **≤ 30 µs** |
| `axiomdb_bench --diagnose-insert-deep` "validate" + "prepare_row" totals | unchanged | unchanged |
| INSERT throughput (10K rows / 1 txn) | 21K rows/s | **≥ 28K rows/s** |
| Ratio vs SQLite (insert_batch) | 47× | **≤ 35×** |
| Allocations per INSERT call (counted via dhat or similar — optional gate) | ~10 | ≤ 5 |
| Workspace test runtime | baseline | within +5% |

These are step-up targets, not "match SQLite" targets — Attack 2 is the
structural change that closes the rest of the gap.

## Dependencies

- Depends on:
  - Attack 3.A (commit `50930d99`) — the cache that B is removing the
    clones FROM.
- Blocks:
  - Attack 2 (statement fingerprinting) — Attack 2 caches the full
    setup; doing 3.B first means Attack 2's cached payload is leaner.

## Open questions

All resolved during the brainstorm; nothing pending. (If an Arc-vs-clone
trade-off turns out to fight the borrow checker badly during
implementation, revisit before approving — but the structure of the
code suggests Arc<IndexDef> is straightforward since IndexDef is
already small + Clone.)

## Done criteria

- [ ] `ctx.search_path.clone()` in `shared.rs:42` removed (iterate by
  reference).
- [ ] `primary_idx.clone()` in `insert_clustered_ctx.rs:379` removed
  (refactor the borrow so `&primary_idx` works, or wrap in `Arc`).
- [ ] `build_insert_column_positions` is called at most once per
  `(table_id, columns_signature)` per session.
- [ ] `primary_key_bytes` is moved/borrowed in batch push instead of
  being cloned twice per row.
- [ ] Same removals applied to `execute_clustered_insert_ctx`
  (autocommit path) where it makes sense.
- [ ] `cargo nextest run --workspace` (Lima) — clean.
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean.
- [ ] `cargo fmt --check` — clean.
- [ ] `axiomdb_bench --compare --rows 10000` shows
  `insert_batch ≥ 28K rows/s` (+30% vs A.1's 21K).
- [ ] `axiomdb_bench --diagnose-insert --rows 10000` shows
  `execute_with_ctx per row ≤ 30 µs`.
- [ ] At least 4 new integration tests covering the
  `col_positions` cache invariants (4 of the 8 edge cases — the
  hot-path ones).
- [ ] All previously `#[ignore]`-marked tests in
  `integration_resolve_table_cache.rs` remain ignored (no new
  TODOs introduced).

## References

External:
- SQLite cursor reuse fast path:
  `research/sqlite/src/btree.c:9482-9491` (`BTCF_ValidNKey` + same-size
  overwrite).
- SQLite "in-memory page" reuse pattern in `prepare.c` /
  `vdbeaux.c::sqlite3VdbeReset`.

Internal:
- Inventory of clones (grep on 2026-05-16):
  - `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs` lines 44,
    49, 53-65, 67, 343, 348, 361-362, 379, 500, 516-517
  - `crates/axiomdb-sql/src/executor/shared.rs` lines 37, 42, 51, 108
- Attack 3.A spec:
  `specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-A.md`
- Diagnostic harness: `--diagnose-insert` + `--diagnose-insert-deep` in
  `benches/comparison/axiomdb_bench/src/main.rs`.
- User-facing doc:
  `docs/perf-sqlite-gap.md` (will be updated when 3.B closes).
