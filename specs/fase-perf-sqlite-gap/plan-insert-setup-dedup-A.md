# Plan: insert-setup-dedup-A — versioned ResolvedTable cache

Phase: perf-sqlite-gap — close embedded INSERT gap with SQLite
Task: A — `ResolvedTable` cache validated by `schema_version`
Spec: specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-A.md
Status: in-progress

## Summary

Five steps. **Step 1** adds the new `SessionContext::get_table_if_version`
companion method (no behavior change yet — just the API). **Step 2** rewrites
`resolve_table_cached` to consult that method even inside an explicit
transaction, with a cheap `get_table_by_id` catalog read for version
validation; this is A.1 in the spec. **Step 3** adds the 11 edge-case
integration tests called out in the spec. **Step 4** measures with
`axiomdb_bench --diagnose-insert` and, **if and only if** the A.1 budget
(`≤ 25 µs/row`) is hit but the A.2 stretch (`≤ 12 µs/row`) is not, adds
the per-`ConnectionTxn` "catalog-clean fast path" (A.2). **Step 5** closes
with workspace gates, wire-test smoke, docs, and the final commit.

TDD throughout: each step opens with a failing test, then the
minimal implementation that makes it pass.

## Dependencies

Must be done first:
- [x] spec-insert-setup-dedup-A approved
- [x] diagnostic infrastructure landed (commit `f8037b4c` — `--diagnose-insert`
      and `--diagnose-insert-deep`)
- [x] Phase 40.2 OID-based plan cache infra (already present:
      `plan_deps.rs`, `bump_table_schema_version`)

Blocks (until done):
- Attack 3.B (clone removal in `enqueue_clustered_insert_ctx`) — easier
  once setup is no longer rebuilt every call.
- Attack 2 (statement fingerprinting / plan cache by SQL text) — that
  cache layers on top of this one.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_resolve_table_cache.rs` — 11
  edge-case tests called out in the spec.

Modified files:
- `crates/axiomdb-sql/src/session.rs` — add
  `SessionContext::get_table_if_version` (~15 lines).
- `crates/axiomdb-sql/src/executor/shared.rs` — rewrite
  `resolve_table_cached` cache-lookup logic (~30 line diff).
- `crates/axiomdb-wal/src/txn.rs` — Step 4 ONLY: add
  `catalog_dirty: bool` field to `ConnectionTxn` (skipped if Step 4 is
  not needed).
- `crates/axiomdb-sql/src/executor/ddl_*.rs` — Step 4 ONLY: set
  `conn_txn.catalog_dirty = true` in the existing DDL paths that
  already call `bump_table_schema_version`.
- `memory/project_state.md`, `docs/perf-sqlite-gap.md` — Step 5.

---

## Step 1 — Add `SessionContext::get_table_if_version`

**Goal:** New companion accessor that returns `Some(&ResolvedTable)` only
when the cached entry's `schema_version` matches the caller's
`expected_version`. Pure addition; no existing call site changes yet.

**Files:**
- `crates/axiomdb-sql/src/session.rs` (~15 lines added)
- `crates/axiomdb-sql/tests/integration_resolve_table_cache.rs` (new file,
  starts with 1 unit-level test of the new method)

**Approach:** TDD — write a test that exercises the new method on a
hand-built `SessionContext`, then add the method.

### Test to add

```rust
// crates/axiomdb-sql/tests/integration_resolve_table_cache.rs (new)

use axiomdb_catalog::resolver::ResolvedTable;
use axiomdb_catalog::schema::{
    ColumnDef, ColumnType, RelationKind, TableDef, TablePersistence,
    TableStorageLayout, DEFAULT_DATABASE_NAME,
};
use axiomdb_sql::SessionContext;

fn fake_resolved_table(id: u32, name: &str, schema_version: u64) -> ResolvedTable {
    ResolvedTable {
        def: TableDef {
            id, schema_name: "public".into(), table_name: name.into(),
            kind: RelationKind::Table,
            persistence: TablePersistence::Permanent,
            storage_layout: TableStorageLayout::Heap,
            schema_version,
            triggers: vec![],
        },
        columns: vec![],
        indexes: vec![],
        constraints: vec![],
        foreign_keys: vec![],
    }
}

#[test]
fn get_table_if_version_returns_some_on_match() {
    let mut ctx = SessionContext::default();
    ctx.cache_table(DEFAULT_DATABASE_NAME, "public", "t",
                    fake_resolved_table(1, "t", 7));
    let r = ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 7);
    assert!(r.is_some());
    assert_eq!(r.unwrap().def.id, 1);
}

#[test]
fn get_table_if_version_returns_none_on_version_mismatch() {
    let mut ctx = SessionContext::default();
    ctx.cache_table(DEFAULT_DATABASE_NAME, "public", "t",
                    fake_resolved_table(1, "t", 7));
    assert!(ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 8).is_none());
    assert!(ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 6).is_none());
}

#[test]
fn get_table_if_version_returns_none_on_miss() {
    let ctx = SessionContext::default();
    assert!(ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 0).is_none());
}
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/session.rs — add inside `impl SessionContext`

/// Returns the cached `ResolvedTable` for `(database, schema, table)`
/// only if its `def.schema_version` equals `expected_version`.
///
/// Returns `None` on cache miss OR on version mismatch. Does NOT
/// auto-evict on mismatch — the caller is expected to re-resolve
/// and overwrite via [`cache_table`].
///
/// This is the cache-hit fast path for `resolve_table_cached` inside
/// explicit transactions, mirroring SQLite's schema-cookie check
/// (`research/sqlite/src/prepare.c:518-526`).
pub fn get_table_if_version(
    &self,
    database: &str,
    schema: &str,
    table: &str,
    expected_version: u64,
) -> Option<&ResolvedTable> {
    let cached = self.cache.get(&Self::key(database, schema, table))?;
    if cached.def.schema_version == expected_version {
        Some(cached)
    } else {
        None
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_resolve_table_cache
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
```

### Commit

```
feat(perf-sqlite-gap): step 1 — add SessionContext::get_table_if_version

New cache accessor that validates against TableDef.schema_version.
Pure addition — no caller wired yet (step 2 does that).
3 unit tests cover hit, version mismatch, and miss.
```

---

## Step 2 — Wire `resolve_table_cached` to use the versioned cache inside txn

**Goal:** The bench's INSERT path inside `BEGIN/COMMIT` starts hitting
the cache. `--diagnose-insert` should show ≥ 2× improvement immediately.

**Files:**
- `crates/axiomdb-sql/src/executor/shared.rs` — rewrite the cache-check
  blocks (lines ~26-67) for both the qualified and search-path paths.
- `crates/axiomdb-sql/tests/integration_resolve_table_cache.rs` —
  one end-to-end test that proves cache hits happen inside txn.

**Approach:** TDD — failing integration test first (counts catalog reads
indirectly by measuring throughput), then the change.

### Test to add

```rust
// Same test file as Step 1 — appended.

use axiomdb_sql::{bloom::BloomRegistry, SchemaCache};
use axiomdb_storage::MemoryStorage;
use axiomdb_wal::TxnManager;

// Reuses the bench-wide harness pattern: build storage + txn manually,
// drive raw SQL through parse / analyze / execute.
mod harness; // small helper file with `run(sql)` + `setup()`

#[test]
fn cached_inside_txn_after_first_insert() {
    // 100 INSERTs into the same table inside one BEGIN..COMMIT.
    // The first one populates the cache; the next 99 must reuse it.
    let mut h = harness::setup();
    h.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)");
    h.run("BEGIN");
    let cache_size_before = h.session.cached_count();
    for i in 1..=100 {
        h.run(&format!("INSERT INTO t VALUES ({i}, 'a')"));
    }
    h.run("COMMIT");
    let cache_size_after = h.session.cached_count();
    assert_eq!(cache_size_before, 0,
        "no entry before any INSERT");
    assert_eq!(cache_size_after, 1,
        "exactly one ResolvedTable cached for table 't' after 100 INSERTs");
}
```

(The cache_size assertion proves the entry was added and not
re-created/re-keyed on every INSERT — if the old behavior were still
in effect, every INSERT would either bypass the cache entirely or
re-insert the same key 100 times. Both fail the test.)

### Implementation outline

```rust
// crates/axiomdb-sql/src/executor/shared.rs — replace the body of
// resolve_table_cached's cache logic.

fn resolve_table_cached(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
    conn_txn: Option<&axiomdb_wal::ConnectionTxn>,
    tref: &crate::ast::TableRef,
) -> Result<ResolvedTable, DbError> {
    let database = effective_database_for_ref(tref, ctx);

    // Database existence pre-check (unchanged).
    if tref.database.is_some() {
        // … same as today …
    }

    // Common cache-validate-then-resolve helper closure.
    let try_cached_with_version = |
        ctx: &mut SessionContext,
        schema: &str,
        name: &str,
    | -> Result<Option<ResolvedTable>, DbError> {
        let Some(cached_id) = ctx.get_table(&database, schema, name)
            .map(|r| r.def.id)
        else { return Ok(None); };

        // Cheap version probe — 1 catalog row read (axiom_tables by id).
        let snap = conn_txn
            .map(|c| txn.active_snapshot(c))
            .unwrap_or_else(|| txn.snapshot());
        let mut reader = CatalogReader::new(storage, snap)?;
        let current = reader.get_table_by_id(
            // need the id we cached
            cached_id,
        )?;
        let Some(current_def) = current else {
            // table dropped underneath us — evict, force re-resolve
            // which will surface a TableNotFound to the caller
            ctx.invalidate_table(&database, schema, name);
            return Ok(None);
        };

        let cached_version = ctx
            .get_table(&database, schema, name)
            .map(|r| r.def.schema_version)
            .expect("get_table returned Some above");
        if cached_version == current_def.schema_version {
            return Ok(ctx
                .get_table_if_version(&database, schema, name, cached_version)
                .cloned());
        }
        // Stale — evict heap-tail hint and tell caller to re-resolve.
        ctx.invalidate_table(&database, schema, name);
        Ok(None)
    };

    // Qualified name path.
    if let Some(schema) = tref.schema.as_deref() {
        if let Some(hit) = try_cached_with_version(ctx, schema, &tref.name)? {
            return Ok(hit);
        }
        let mut resolver = make_resolver_with_database(storage, txn, conn_txn, &database)?;
        let resolved = resolver.resolve_table(Some(schema), &tref.name)?;
        ctx.cache_table(&database, schema, &tref.name, resolved.clone());
        return Ok(resolved);
    }

    // Unqualified name path — walk search_path.
    let search_path: Vec<String> = ctx.search_path.clone();   // Step B.1 removes this clone
    for schema in &search_path {
        if let Some(hit) = try_cached_with_version(ctx, schema, &tref.name)? {
            return Ok(hit);
        }
        let mut resolver = make_resolver_with_database(storage, txn, conn_txn, &database)?;
        if let Ok(resolved) = resolver.resolve_table(Some(schema), &tref.name) {
            ctx.cache_table(&database, schema, &tref.name, resolved.clone());
            return Ok(resolved);
        }
        // FDW fallback (unchanged).
        let snap = conn_txn
            .map(|c| txn.active_snapshot(c))
            .unwrap_or_else(|| txn.snapshot());
        let mut reader = CatalogReader::new(storage, snap)?;
        if let Some(ftable) = reader.get_foreign_table(schema, &tref.name)? {
            return Ok(fdw_resolved_table(ftable));
        }
    }
    Err(DbError::TableNotFound { name: tref.name.clone() })
}
```

Key invariants preserved:
- DDL still surfaces correctly: `bump_table_schema_version` makes the next
  `get_table_by_id` read return a higher version → cache miss → full
  re-resolve via the existing path.
- FDW fallback unchanged.
- `invalidate_table` still works the same.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_resolve_table_cache
./tools/vm.sh test -p axiomdb-sql   # nothing else broken
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5

# Quick numerical check (not a hard gate yet — step 4 has the budget):
cargo run -p axiomdb-bench-comparison --release -- \
    --scenario insert_batch --rows 10000 --diagnose-insert 2>&1 | tail -10
```

Expect `execute_with_ctx per row` to drop from 55-110 µs to roughly
20-30 µs. If it does not move, STOP and revise.

### Commit

```
feat(perf-sqlite-gap): step 2 — resolve_table_cached uses cache inside txn

Cache lookup now consults the versioned accessor and validates against
the current TableDef.schema_version via a cheap get_table_by_id read.
DDL stays correct because bump_table_schema_version (already wired in
all DDL paths from Phase 40.2) advances the version on every change.

Mirrors SQLite's schema-cookie pattern from research/sqlite/src/prepare.c.

Integration test cached_inside_txn_after_first_insert demonstrates the
cache populates exactly once across 100 INSERTs in a single txn.
```

---

## Step 3 — Add the 11 edge-case tests from the spec

**Goal:** Every "Edge cases" bullet in the spec has a corresponding test
in `integration_resolve_table_cache.rs`.

**Files:**
- `crates/axiomdb-sql/tests/integration_resolve_table_cache.rs` —
  append.

**Approach:** Each test is small (15-30 lines). Names mirror the spec
bullets exactly so a reviewer can map 1:1.

### Tests to add

```rust
#[test] fn cache_hit_inside_explicit_txn() { /* 100 INSERTs/1 txn,
    assert cached_count == 1 (already in Step 2, kept here for symmetry
    OR moved here and Step-2 keeps only the simpler 2-row version) */ }

#[test] fn alter_table_mid_txn_forces_re_resolve() {
    // BEGIN; INSERT INTO t(id) VALUES (1);
    // ALTER TABLE t ADD COLUMN x INT;
    // INSERT INTO t(id, x) VALUES (2, 99);
    // COMMIT;
    // SELECT x FROM t WHERE id=2 → 99
}

#[test] fn create_index_mid_txn_forces_re_resolve() {
    // similar shape; index maintenance must run on the 2nd INSERT
}

#[test] fn drop_index_mid_txn_forces_re_resolve() { /* … */ }

#[test] fn truncate_mid_txn_keeps_cache() {
    // BEGIN; INSERT; TRUNCATE t; INSERT; COMMIT;
    // cached_count remains 1 throughout
}

#[test] fn bulk_delete_mid_txn_keeps_cache() { /* analogous */ }

#[test] fn drop_table_mid_txn_invalidates_lookup() {
    // BEGIN; INSERT INTO t; DROP TABLE t; INSERT INTO t → TableNotFound
}

#[test] fn savepoint_rollback_reverts_schema_version() {
    // BEGIN; INSERT;
    // SAVEPOINT s; ALTER TABLE t ADD COLUMN x INT; ROLLBACK TO s;
    // INSERT — must use the pre-savepoint schema (no `x` column)
}

#[test] fn concurrent_ddl_other_conn_bumps_version() {
    // Two Db handles on the same storage; conn A caches; conn B does
    // ALTER and commits; conn A's next INSERT sees the new column.
}

#[test] fn first_insert_into_new_table_populates_cache() { /* … */ }

#[test] fn unqualified_name_caches_under_resolved_schema() {
    // search_path = ['app', 'public']; INSERT INTO t where t lives in app;
    // cache key is (default_db, "app", "t")
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_resolve_table_cache
```

All 11+ tests pass (3 already added in steps 1 + 2 plus the 11 here =
14 tests in this file).

### Commit

```
test(perf-sqlite-gap): step 3 — 11 edge-case tests for cache invalidation

Each test mirrors one bullet of the "Edge cases" section in
spec-insert-setup-dedup-A.md. Covers DDL mid-txn, savepoint rollback,
concurrent DDL across connections, FDW fallback, search-path
resolution, and the DROP TABLE path.
```

---

## Step 4 — Measure; if A.1 budget hit but A.2 stretch not, add per-txn fast path

**Goal:** Confirm A.1 hits the spec's hard budget. If A.1 is hit but
the stretch (≤ 12 µs/row, ≥ 80K rows/s) is not, add A.2.

**Decision tree:**
1. Run `cargo run -p axiomdb-bench-comparison --release -- --scenario
   insert_batch --rows 10000 --diagnose-insert`.
2. Read `execute_with_ctx per row`:
   - **≤ 12 µs/row** → A.2 already met by A.1 alone. Skip Step 4 body;
     mark this step `done` with the measurement and continue to Step 5.
   - **≤ 25 µs/row but > 12 µs/row** → A.1 done, A.2 needed. Implement
     the per-txn fast path below.
   - **> 25 µs/row** → A.1 budget MISSED. STOP and revise the plan. Likely
     causes: cache validation is still doing too much, or the catalog
     `get_table_by_id` is more expensive than expected. Add a profiler
     run before changing more code.

### A.2 implementation (only if needed)

**Idea (mirrors SQLite "DB_SchemaLoaded" property, `main.c:1404`):**
add a per-`ConnectionTxn` flag that flips when the connection itself
issues DDL. As long as that flag is false **and** the txn snapshot
hasn't been refreshed, the cache version check can be skipped — DDL
from another conn would have refreshed the snapshot (READ COMMITTED) or
be invisible until COMMIT anyway (REPEATABLE READ).

**Files:**
- `crates/axiomdb-wal/src/txn.rs` — add `pub catalog_dirty: bool` to
  `ConnectionTxn` (default `false`).
- All DDL executors that call `bump_table_schema_version`: also set
  `conn_txn.catalog_dirty = true`. Grep audit:
  - `ddl_alter_column.rs:1001/1096/1224/1919`
  - `ddl_create_index.rs:~635`
  - `ddl_drop_index.rs:~128`
  - Plus any other site grep finds — add to all of them in one pass.
- `crates/axiomdb-sql/src/executor/shared.rs` — short-circuit the
  version probe when `conn_txn.is_some_and(|c| !c.catalog_dirty)`.

### Test to add

```rust
#[test]
fn catalog_dirty_flag_set_by_local_ddl() {
    // BEGIN; INSERT; ALTER; INSERT;
    // After ALTER: conn_txn.catalog_dirty == true (validated via a
    // test-only accessor or by observing the re-resolve happens)
}

#[test]
fn catalog_dirty_flag_remains_false_on_pure_dml() {
    // BEGIN; INSERT × 100; COMMIT — flag never flipped
}
```

### Verification

```bash
cargo run -p axiomdb-bench-comparison --release -- \
    --scenario insert_batch --rows 10000 --diagnose-insert | tail -10
# Expect execute_with_ctx ≤ 12 µs/row, throughput ≥ 80K rows/s.

./tools/vm.sh test -p axiomdb-sql --test integration_resolve_table_cache
./tools/vm.sh test -p axiomdb-wal   # ConnectionTxn change
```

### Commit (only if Step 4 body executes)

```
perf(perf-sqlite-gap): step 4 — per-ConnectionTxn catalog-clean fast path

Skip the per-statement schema_version probe when the active conn_txn
has not issued any DDL. Mirrors SQLite's DB_SchemaLoaded property
(research/sqlite/src/main.c:1404). Drops execute_with_ctx from ~25 µs
to ~10-12 µs per row.
```

---

## Step 5 — Close: workspace gates + wire smoke + docs + final commit

**Goal:** Every Done criterion in the spec is checked.

### Verification against spec

- [ ] `cargo nextest run --workspace` (Lima) — clean
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean
- [ ] `cargo fmt --check` — clean
- [ ] `tools/wire-test.py` — clean (no wire regressions; pre-flight per
      memory: `pkill axiomdb-server && cargo build --release -p
      axiomdb-server && rm target/release/axiomdb-server-bk-*`)
- [ ] `axiomdb_bench --compare --rows 10000` INSERT ratio ≤ 14×
- [ ] `axiomdb_bench --diagnose-insert --rows 10000` `execute_with_ctx`
      per row ≤ 25 µs (≤ 12 µs if Step 4 ran)
- [ ] All 14+ tests in `integration_resolve_table_cache.rs` pass
- [ ] Rustdoc updated on `get_table_if_version`,
      `resolve_table_cached`, and the optional `catalog_dirty` field

### Docs to update

- `memory/project_state.md` — bump "Próximo paso" / record new INSERT
  numbers
- New `docs/perf-sqlite-gap.md` — short doc summarizing Attack 3.A
  results (before/after numbers, wire path measurement, link to the
  spec/plan)
- `memory/project_sqlite_baseline.md` — update with new numbers; the
  old baseline becomes "pre-attack-3.A"

### Final commit

```
feat(perf-sqlite-gap): close Attack 3.A — versioned ResolvedTable cache

Implements specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-A.md
Plan: specs/fase-perf-sqlite-gap/plan-insert-setup-dedup-A.md

Results (10K rows / 1 txn, axiomdb_bench --diagnose-insert):
  execute_with_ctx per row: 55-110 µs → ? µs   (target ≤ 25 µs)
  INSERT throughput:        8.8K r/s  → ? r/s  (target ≥ 40K)
  Ratio vs SQLite:          62×       → ?×     (target ≤ 14×)

Tests: 14 new integration tests covering all spec edge cases.
Wire smoke: unchanged (cross-path benefit documented in
docs/perf-sqlite-gap.md).
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `get_table_by_id` is more expensive than expected (linear scan over `axiom_tables`) | Medium | Measure in Step 2 verification; if it dominates, A.2 (Step 4) bypasses it. |
| A DDL site exists that mutates schema without calling `bump_table_schema_version` | Medium | Step 3's `concurrent_ddl_other_conn_bumps_version` plus a grep audit during Step 4 (DDL files known: ddl_alter_column, ddl_create_index, ddl_drop_index). If any are missing, add the bump there as part of Step 4 (this is a pre-existing bug we surface). |
| Save-point rollback doesn't revert `schema_version` correctly | Medium | Spec test `savepoint_rollback_reverts_schema_version`. If MVCC doesn't restore the version, we have a deeper bug to fix as a pre-step. |
| `ResolvedTable.clone()` on every hit is still expensive | Low | Phase 3.B (later) replaces the clone with `Arc<ResolvedTable>`; for now the clone is acceptable since the spec target is 25 µs total (clone is ~1-2 µs). |
| Wire-test regression on a path that wasn't covered | Medium | Step 5 explicitly runs `tools/wire-test.py`; per memory, pre-flight with fresh server build. |
| Multi-conn test framework not available (no easy "second Db handle on same storage") | Low | Skip the concurrent-DDL test as `#[ignore]` if it requires non-trivial harness work; track as a follow-up. Mention in Step 3 commit if so. |

## Rollback plan

1. If Steps 1-2 land but cause a regression: `git revert <step-2 hash>
   <step-1 hash>` — both commits are tiny and self-contained.
2. If Step 4 (A.2) causes a regression but A.1 was fine: just revert
   step 4. A.1 alone keeps the win.
3. If the whole plan turns out wrong:
   `git branch abandoned/plan-insert-setup-dedup-A-2026-05-16` from
   the last clean commit; reset main; revert the spec to `draft`
   with a note explaining what failed.

## Estimated effort

Total: ~1.5-2 days for A.1 + A.2 + tests + docs.
- Step 1 (get_table_if_version + 3 tests): 30 min
- Step 2 (resolve_table_cached rewrite + 1 test): 1.5 h
- Step 3 (11 edge tests): 3-4 h
- Step 4 (A.2 — only if needed): 4 h
- Step 5 (close: workspace, wire, docs): 1-2 h
