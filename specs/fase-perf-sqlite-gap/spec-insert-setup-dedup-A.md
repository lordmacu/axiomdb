# Spec: insert-setup-dedup-A — versioned ResolvedTable cache

Phase: perf-sqlite-gap — close the embedded-mode INSERT gap with SQLite
Task: A — ResolvedTable cache validated by `schema_version` (works inside explicit txn)
Status: approved

## Context

Diagnostic on 2026-05-16 measured AxiomDB embedded INSERT at 8.8K rows/s vs
SQLite at 546K rows/s (62× slower) on the same Rust-native bench
(`axiomdb_bench --compare --rows 10000`). The per-row work inside the
clustered INSERT loop is only ~1 µs (competitive with SQLite); the gap is
**~50-110 µs of per-statement overhead** that runs once per INSERT
statement.

The dominant single cost is `resolve_table_cached`
([shared.rs:26](crates/axiomdb-sql/src/executor/shared.rs:26)) which
**bypasses its own cache whenever a transaction is active**:

```rust
// Inside an explicit transaction, catalog metadata can change mid-transaction
// (bulk DELETE, TRUNCATE, DDL, savepoint rollback). Skip the cache so every
// statement always reads the current catalog state via the active snapshot.
let in_txn = conn_txn.is_some();

if let Some(schema) = tref.schema.as_deref() {
    if !in_txn {                                  // ← cache only outside txn
        if let Some(cached) = ctx.get_table(...) { return Ok(cached.clone()); }
    }
    let resolved = resolver.resolve_table(...)?;  // ← full catalog scan
    if !in_txn { ctx.cache_table(..., resolved.clone()); }
    return Ok(resolved);
}
```

The reason for the bypass is correct — DDL / TRUNCATE / `ALTER` inside a
transaction can change schema mid-flight, so the cached `ResolvedTable`
could become stale. But AxiomDB already has the infrastructure to detect
this precisely: every `TableDef` carries a `schema_version: u64`
([schema_table.rs:220](crates/axiomdb-catalog/src/schema_table.rs:220))
that `CatalogWriter::bump_table_schema_version`
([writer.rs:1288](crates/axiomdb-catalog/src/writer.rs:1288)) increments
on every DDL operation. The OID-based plan cache in
[plan_deps.rs](crates/axiomdb-sql/src/plan_deps.rs) already uses this for
plan invalidation. This spec extends the same pattern to `ctx.cache`.

SQLite solves the same problem with the "schema cookie" mechanism:
[`research/sqlite/src/prepare.c:518-526`](research/sqlite/src/prepare.c)
reads `BTREE_SCHEMA_VERSION` from the btree meta page and compares it to
the cached `pSchema->schema_cookie`; on mismatch it calls
`sqlite3ResetOneSchema` and the caller re-prepares. Our equivalent is
checking `TableDef.schema_version` per cached entry.

## Goal

Make `resolve_table_cached` serve cached `ResolvedTable` entries from
inside an explicit transaction when the table's `schema_version` has not
changed since the entry was cached, so single-table INSERT/UPDATE/DELETE
loops stop paying the full catalog-scan cost on every statement.

## Non-goals

- Caching parsed `Stmt` ASTs or analyzed plans by SQL text. Deferred to
  Attack 2 / Attack D (statement fingerprinting / prepared statement
  cache).
- Eliminating `resolved.def.clone()` / `schema_cols.to_vec()` /
  `primary_idx.clone()` inside `enqueue_clustered_insert_ctx`. Deferred
  to Attack 3.B (clone removal).
- Caching `col_positions` per `(table_id, columns_signature)`. Deferred
  to Attack 3.B.3.
- Cross-statement cursor reuse (like SQLite's `BTCF_ValidNKey` fast path
  in [`btree.c:9482`](research/sqlite/src/btree.c)). Separate work.
- New API surface in `axiomdb-embedded`. The change is internal.

## Behavior

### Public API

No new public API. Internal change to one function:

```rust
// crates/axiomdb-sql/src/executor/shared.rs — current signature unchanged
fn resolve_table_cached(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
    conn_txn: Option<&axiomdb_wal::ConnectionTxn>,
    tref: &crate::ast::TableRef,
) -> Result<ResolvedTable, DbError>;
```

`SessionContext` gains one cache-companion method (purely additive — does
not change the existing `cache_table` / `get_table` / `invalidate_table`
surface):

```rust
// crates/axiomdb-sql/src/session.rs
impl SessionContext {
    /// Returns the cached `ResolvedTable` if its `def.schema_version` matches
    /// `expected_version`. Returns `None` on miss OR on version mismatch
    /// (does NOT auto-evict — that is the caller's choice, since they may
    /// re-resolve and want to overwrite atomically via `cache_table`).
    pub fn get_table_if_version(
        &self,
        database: &str,
        schema: &str,
        table: &str,
        expected_version: u64,
    ) -> Option<&ResolvedTable>;
}
```

### Semantics

`resolve_table_cached` new behavior, regardless of whether `conn_txn` is
`Some` or `None`:

1. Compute `database` and `schema` exactly as today (`tref.database`,
   `tref.schema`, `ctx.search_path` fallback).

2. If there is a cache entry for `(database, schema, name)`:
   a. Read the current `schema_version` for that table from the catalog
      using the current active snapshot. This is a single
      `CatalogReader::find_table_by_name(...)` call.
   b. If `current.schema_version == cached.def.schema_version`:
      → return the cached `ResolvedTable` (cloned, as today).
   c. Otherwise: fall through to full resolution and overwrite the cache
      entry. Also invalidate the heap-tail hint
      (`ctx.invalidate_heap_tail(cached.def.id)`).

3. If there is no cache entry: full resolution via
   `make_resolver_with_database` + `resolver.resolve_table`, then
   `ctx.cache_table(...)` regardless of whether a txn is active.

**Why this is safe inside an explicit transaction:**

- Every DDL operation that can mutate schema calls
  `CatalogWriter::bump_table_schema_version` on the affected table (this
  is already in the codebase — `ddl_alter_column.rs:1001/1096/1224/1919`,
  `ddl_create_index.rs`, `ddl_drop_index.rs`, etc.).
- DDL inside a txn writes to the catalog with the txn's
  `ConnectionTxn` snapshot, so `find_table_by_name` from that same
  `ConnectionTxn` will read the bumped version.
- `TRUNCATE` and bulk DELETE do NOT change schema, so they intentionally
  do NOT bump `schema_version` — cached entries remain valid (the
  catalog still describes the same columns / indexes / FKs).
- `ROLLBACK` to a savepoint undoes catalog mutations through MVCC;
  subsequent reads from the same `ConnectionTxn` see the rolled-back
  state, which means the `schema_version` they read reverts too.

**Performance contract:**

- Cache HIT cost: 1 catalog `find_table_by_name` (must read 1 row from
  the `axiom_tables` heap to learn current `schema_version`) + 1
  `ResolvedTable::clone` (Vec\<ColumnDef\> + Vec\<IndexDef\> + ...).
- Cache MISS cost: identical to today — full
  `resolver.resolve_table(...)`.

The `find_table_by_name` read is itself a non-trivial cost
(~10-20 µs estimated). It is acceptable for this step (A.1). Step A.2
will amortize even this away with a per-`ConnectionTxn` "last seen
schema_version" cache, gated on a flag that DDL flips.

### Error cases

The function's error surface is unchanged. New error modes:

| Input | Expected error | Message |
|-------|----------------|---------|
| Cached entry exists but underlying table was DROPped mid-txn (`find_table_by_name` returns None) | `DbError::TableNotFound` (same as today) | `"table '{name}' not found"` |
| Catalog read fails during version check | propagate underlying `DbError` | — |

### Cross-path impact

This change touches the **engine SQL layer**, so it benefits every
path that goes through `Db::run(sql)`:

| Path | Expected speedup on INSERT-heavy workload |
|------|-------------------------------------------|
| Embedded Rust (`Db::execute`) | **Large (5-10×)** — engine dominates, this attack is exactly there. |
| C FFI / Python `bindings/python/axiomdb.py` | **Large (5-10×)** — FFI overhead is ~0 (verified). |
| MySQL wire (`axiomdb-server`) — INSERT batch in one txn | **Large (5-10×)** — engine dominates, TCP is once-per-batch. |
| MySQL wire — INSERT autocommit (1 txn / row) | **Medium (2-3×)** — TCP roundtrip becomes the new bottleneck. |
| Single-row SELECT WHERE id=? | **Medium (1.5-2×)** — same `resolve_table_cached` is on the hot path; TCP and result serialization share what's left. |

Validation step B.4 will re-run `benches/comparison/bench_runner.py`
against an `axiomdb-server` build with this change and document the
actual wire numbers.

## Edge cases

Each becomes a test case in the plan:

- [ ] **Cache hit inside explicit txn** — `BEGIN; INSERT × 100; COMMIT` —
  the 2nd-100th INSERTs hit the cache.
- [ ] **Cache miss after `ALTER TABLE` inside the same txn** —
  `BEGIN; INSERT; ALTER TABLE t ADD COLUMN x INT; INSERT;` — second
  INSERT must re-resolve and see the new column.
- [ ] **Cache miss after `CREATE INDEX` inside the same txn** —
  `BEGIN; INSERT; CREATE INDEX i ON t(c); INSERT;` — second INSERT must
  re-resolve and see the new index so index maintenance kicks in.
- [ ] **Cache miss after `DROP INDEX` inside the same txn** — analogous;
  index maintenance must NOT try to update the dropped index.
- [ ] **Cache hit unaffected by `TRUNCATE`** — `BEGIN; INSERT;
  TRUNCATE t; INSERT;` — second INSERT can reuse cached `ResolvedTable`
  (TRUNCATE does not change schema).
- [ ] **Cache hit unaffected by bulk `DELETE`** — same as TRUNCATE.
- [ ] **`DROP TABLE` mid-txn invalidates lookup** — `BEGIN; INSERT INTO t;
  DROP TABLE t; INSERT INTO t;` — second INSERT errors with
  `TableNotFound`.
- [ ] **Cache miss after `SAVEPOINT s; DDL; ROLLBACK TO s`** — cache
  entry from before the savepoint is still valid (DDL was rolled back so
  `schema_version` reverts via MVCC).
- [ ] **Concurrent DDL by another connection bumps version**
  (single-process, 2 connections): conn A caches; conn B does
  `ALTER TABLE` and commits; conn A's next statement detects the new
  version and re-resolves.
- [ ] **First INSERT into a brand-new table** populates the cache.
- [ ] **Search-path resolution** (unqualified name, table only in 2nd
  schema of `search_path`) caches under the resolved
  `(database, schema)` key correctly.

## On-disk format

No on-disk format change. `schema_version` already exists in the
serialized `TableDef`.

## Performance budget

Measured on `axiomdb_bench --diagnose-insert --rows 10000` (Rust
native, M-series macOS, BEGIN/COMMIT wrapping 10K INSERTs).

| Metric | Today | Target after A.1 | Target after A.2 |
|--------|-------|------------------|------------------|
| `execute_with_ctx` per row | 55-110 µs | ≤ 25 µs | ≤ 12 µs |
| End-to-end INSERT throughput | 8.8K rows/s | ≥ 40K rows/s | ≥ 80K rows/s |
| Ratio vs SQLite (Rust native) | 62× slower | ≤ 14× slower | ≤ 7× slower |
| Workspace test runtime | baseline | within +5% | within +5% |

`axiomdb_bench --diagnose-insert` and `--diagnose-insert-deep` already
exist (committed in `f8037b4c`) and are the canonical measurement
harness.

## Dependencies

- Depends on:
  - Phase 40.2 — OID-based plan cache infra (already present:
    `plan_deps.rs`, `bump_table_schema_version`).
  - `TableDef::schema_version` field (already present:
    `schema_table.rs:220`).
- Blocks:
  - Attack 3.B (clone removal in `enqueue_clustered_insert_ctx`) —
    that work is easier once setup is no longer rebuilt every call.
  - Attack 2 (statement fingerprinting) — that cache will key on
    SQL text, but its `ResolvedTable` payload reuses this lookup.

## Open questions

All resolved during brainstorm; nothing pending. (If implementation
uncovers issues, revise the spec before continuing.)

## Done criteria

- [ ] `resolve_table_cached` returns cached entries inside explicit txn
  when `schema_version` matches, validated by a new dedicated test
  module `crates/axiomdb-sql/tests/integration_resolve_table_cache.rs`
  with at least one test per "Edge cases" bullet above.
- [ ] `axiomdb_bench --compare --rows 10000` shows INSERT ratio ≤ 14×
  (down from current 62×).
- [ ] `axiomdb_bench --diagnose-insert --rows 10000` reports
  `execute_with_ctx` per row ≤ 25 µs (down from 55-110 µs).
- [ ] `cargo nextest run --workspace` (Lima) — clean.
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean.
- [ ] `cargo fmt --check` — clean.
- [ ] `tools/wire-test.py` — clean (no wire regressions).
- [ ] No new public API surface in `axiomdb-embedded`. Internal change
  only.
- [ ] Existing `SchemaCache::invalidate` / `invalidate_table` /
  `invalidate_all` semantics preserved (these still work).
- [ ] Rustdoc updated on `SessionContext::cache_table` / `get_table` /
  the new `get_table_if_version` explaining the version-based protocol.

## References

External:
- SQLite schema cookie read + compare:
  [`research/sqlite/src/prepare.c:518-526`](research/sqlite/src/prepare.c)
- SQLite schema clear after change:
  [`research/sqlite/src/build.c:640,657`](research/sqlite/src/build.c)
  (`sqlite3SchemaClear`)
- SQLite reset one schema after cookie mismatch:
  `sqlite3ResetOneSchema` in `research/sqlite/src/build.c`
- PostgreSQL `plancache.c` dual-dependency design — already referenced
  in our [plan_deps.rs:11](crates/axiomdb-sql/src/plan_deps.rs).

Internal:
- Current `resolve_table_cached`:
  [crates/axiomdb-sql/src/executor/shared.rs:1-72](crates/axiomdb-sql/src/executor/shared.rs)
- `SchemaCache` / `SessionContext::cache_table`:
  [crates/axiomdb-sql/src/session.rs:1055-1124](crates/axiomdb-sql/src/session.rs)
- `TableDef.schema_version`:
  [crates/axiomdb-catalog/src/schema_table.rs:220](crates/axiomdb-catalog/src/schema_table.rs)
- `CatalogWriter::bump_table_schema_version`:
  [crates/axiomdb-catalog/src/writer.rs:1288](crates/axiomdb-catalog/src/writer.rs)
- Existing OID-based plan cache infrastructure:
  [crates/axiomdb-sql/src/plan_deps.rs](crates/axiomdb-sql/src/plan_deps.rs)
- Diagnostic harness (already in place):
  [benches/comparison/axiomdb_bench/src/main.rs](benches/comparison/axiomdb_bench/src/main.rs)
  (`--diagnose-insert` and `--diagnose-insert-deep`).
- Brainstorm transcript: this conversation, 2026-05-16.
