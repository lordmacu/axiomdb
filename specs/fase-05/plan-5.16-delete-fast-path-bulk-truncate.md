# Plan: 5.16 — DELETE fast-path bulk truncate

## Files to create/modify

- `crates/axiomdb-catalog/src/writer.rs`
  - add `update_table_data_root(table_id, new_root, schema, table_name)` or
    equivalent helper that replaces the visible `TableDef` row while preserving
    `table_id`, schema, and name
- `crates/axiomdb-sql/src/executor.rs`
  - replace the current indexed-table fallback in both `execute_delete_ctx(...)`
    and `execute_delete(...)`
  - route `TRUNCATE TABLE` through the same bulk helper
  - add private helpers to:
    - allocate empty heap/index roots
    - collect old heap-chain page IDs
    - collect old B-Tree page IDs
    - rotate roots and schedule old pages for deferred free
  - invalidate session cache exactly once after root rotation
- `crates/axiomdb-wal/src/txn.rs`
  - extend transaction/savepoint state with deferred-free tracking
  - make savepoint rollback discard deferred frees recorded after the savepoint
  - make full rollback discard all deferred frees for the active txn
  - expose `defer_free_pages(...)` and `release_committed_frees(...)`
- `crates/axiomdb-network/src/mysql/group_commit.rs`
  - after successful fsync + `advance_committed(&ids)`, release deferred frees
    for the same committed txn batch
- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add executor regression tests for indexed full delete/truncate, rollback,
    savepoint rollback, and parent-FK truncate rejection
- `crates/axiomdb-network/tests/integration_protocol.rs`
  - add wire tests for bulk delete/truncate through MySQL protocol
- `tools/wire-test.py`
  - add live client assertions for the new fast path and its regressions

## Reviewed first

These files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `crates/axiomdb-sql/src/bloom.rs`
- `crates/axiomdb-storage/src/engine.rs`
- `crates/axiomdb-storage/src/heap.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-catalog/src/schema.rs`
- `crates/axiomdb-catalog/src/writer.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-wal/src/recovery.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-network/src/mysql/group_commit.rs`
- `benches/comparison/local_bench.py`
- `benches/comparison/axiomdb_bench/src/main.rs`
- `specs/fase-05/spec-5.16-delete-fast-path-bulk-truncate.md`

Research reviewed before writing this plan:

- `research/mariadb-server/sql/handler.cc`
- `research/mariadb-server/sql/sql_truncate.h`
- `research/mariadb-server/sql/sql_base.cc`
- `research/mariadb-server/storage/maria/ma_delete_all.c`
- `research/mariadb-server/storage/innobase/handler/ha_innodb.cc`
- `research/mariadb-server/mysql-test/main/commit_1innodb.result`
- `research/postgres/src/backend/commands/tablecmds.c`
- `research/sqlite/src/delete.c`
- `research/sqlite/src/vdbe.c`
- `research/sqlite/src/btree.c`
- `research/sqlite/test/auth3.test`
- `research/sqlite/test/hook2.test`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- `research/duckdb/src/storage/data_table.cpp`
- `research/duckdb/src/execution/index/bound_index.cpp`
- `research/oceanbase/src/rootserver/ob_ddl_operator.cpp`
- `research/oceanbase/src/storage/ob_storage_schema.h`
- `research/oceanbase/src/sql/engine/basic/ob_truncate_filter_struct.h`
- `research/oceanbase/src/sql/engine/basic/ob_truncate_filter_struct.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
- `research/oceanbase/src/storage/ls/ob_ls_tablet_service.cpp`
- `research/datafusion/datafusion/sql/src/statement.rs`
- `research/datafusion/datafusion/core/src/physical_planner.rs`
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`

## Research synthesis

### AxiomDB-first constraints

- The current `DELETE` fast path is heap-only and cannot stay that way because
  PK/UNIQUE/FK code paths do direct `BTree::lookup_in(...)`.
- `TRUNCATE TABLE` is already incorrect for indexed tables, so `5.16` must fix
  correctness as well as performance.
- Savepoints and group commit are already present, so page-free timing is part
  of the design, not an afterthought.

### What we borrow

- `research/postgres/src/backend/commands/tablecmds.c`
  - borrow: swap to fresh empty storage, keep old storage until durable commit,
    then reclaim it
- `research/sqlite/src/delete.c`
  - borrow: table-clear optimization must clear every index root too
- `research/sqlite/test/auth3.test`
  - borrow: the optimization is invalid when it would bypass observable
    semantics; for AxiomDB that means parent-FK enforcement
- `research/mariadb-server/sql/handler.cc`
  - borrow: full-table delete/truncate deserves a dedicated engine path, not a
    special case hidden inside row-by-row delete
- `research/oceanbase/src/sql/engine/basic/ob_truncate_filter_struct.cpp`
  - borrow: treat truncate as a root/generation switch instead of N row deletes
- `research/datafusion/datafusion/core/src/physical_planner.rs`
  - borrow: keep `DELETE FROM t` with empty filters and `TRUNCATE` as separate
    execution intents even if they share storage machinery underneath

### What we reject

- immediate in-place destruction of old roots
- stale-index-tombstone strategy without a delete index / visibility-aware
  uniqueness path
- keeping the old `record_truncate()` heap-stamp contract for indexed tables

### How AxiomDB adapts it

- full-table delete/truncate will rotate table/index roots through the catalog,
  because AxiomDB already has MVCC catalog rows that rollback/savepoint can undo
- old pages are not freed by WAL undo/recovery; they are reclaimed only after
  commit durability, which fits both immediate and group-commit modes

## Algorithm / Data structure

### 1. Add deferred page-free tracking to `TxnManager`

Extend transaction state:

```rust
#[derive(Debug, Clone, Copy)]
pub struct Savepoint {
    undo_len: usize,
    deferred_free_len: usize,
}

struct ActiveTxn {
    txn_id: TxnId,
    snapshot_id_at_begin: u64,
    undo_ops: Vec<UndoOp>,
    deferred_free_pages: Vec<u64>,
}

struct DeferredFreeBatch {
    txn_id: TxnId,
    pages: Vec<u64>,
}
```

Add exact APIs:

```rust
pub fn defer_free_pages<I>(&mut self, pages: I) -> Result<(), DbError>
where
    I: IntoIterator<Item = u64>;

pub fn release_committed_frees(
    &mut self,
    storage: &mut dyn StorageEngine,
    txn_ids: &[TxnId],
) -> Result<(), DbError>;
```

Rules:

- `savepoint()` captures both lengths
- `rollback_to_savepoint()`:
  - undoes `undo_ops[sp.undo_len..]`
  - truncates `active.deferred_free_pages` to `sp.deferred_free_len`
- `rollback()` discards the active transaction's deferred pages entirely
- `commit()` moves `active.deferred_free_pages` into a manager-level queue keyed
  by `txn_id`
- pages are freed only by `release_committed_frees(...)`

This closes the rollback/savepoint ambiguity before implementation starts.

### 2. Add catalog root-rotation support for tables

`CatalogWriter` already has `update_index_root(...)`. Add the table analogue:

```rust
pub fn update_table_data_root(
    &mut self,
    table_id: u32,
    new_root_page_id: u64,
    schema: &str,
    table_name: &str,
) -> Result<(), DbError>;
```

Implementation mirrors `rename_table(...)`:

1. scan visible `axiom_tables`
2. find `table_id`
3. delete old row
4. insert new row with `data_root_page_id = new_root_page_id`

No extra metadata table is introduced.

### 3. Add private page-collection helpers in `executor.rs`

Keep this local to the executor for `5.16`:

```rust
fn alloc_empty_heap_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError>;
fn alloc_empty_index_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError>;
fn collect_heap_chain_pages(storage: &dyn StorageEngine, root_page_id: u64) -> Result<Vec<u64>, DbError>;
fn collect_btree_pages(storage: &dyn StorageEngine, root_page_id: u64) -> Result<Vec<u64>, DbError>;
```

Exact behavior:

- `alloc_empty_heap_root`:
  - `alloc_page(PageType::Data)`
  - initialize a fresh empty `Page::new(PageType::Data, pid)`
  - write it once
- `alloc_empty_index_root`:
  - same leaf-root initialization already used by `CREATE INDEX`
- `collect_heap_chain_pages`:
  - follow `chain_next_page(...)` from root until `0`
- `collect_btree_pages`:
  - BFS/stack walk identical to current `free_btree_pages(...)`, but return IDs
    instead of freeing immediately

Sort + dedup the final free list before deferring it.

### 4. Replace the current full-table DELETE shortcut

Factor a shared helper:

```rust
struct BulkEmptyPlan {
    visible_row_count: u64,
    old_pages_to_free: Vec<u64>,
    new_data_root: u64,
    new_index_roots: Vec<(u32, u64)>,
}

fn plan_bulk_empty_table(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &TableDef,
    indexes: &[IndexDef],
    snap: TransactionSnapshot,
) -> Result<BulkEmptyPlan, DbError>;

fn apply_bulk_empty_table(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    table_def: &TableDef,
    schema: &str,
    table_name: &str,
    indexes: &[IndexDef],
    plan: BulkEmptyPlan,
) -> Result<(), DbError>;
```

`plan_bulk_empty_table(...)` does:

1. `visible_row_count = scan_rids_visible(old_data_root, snap).len()`
2. allocate `new_data_root`
3. collect all old heap pages from `old_data_root`
4. for each index:
   - allocate `new_root`
   - collect all old pages from `idx.root_page_id`
5. return the full page list + new roots

`apply_bulk_empty_table(...)` does:

1. `CatalogWriter::update_table_data_root(...)`
2. loop `CatalogWriter::update_index_root(index_id, new_root)`
3. `txn.defer_free_pages(plan.old_pages_to_free)`
4. reset Bloom filters:
   - `bloom.create(index_id, 0)` for every rotated index

No per-row `delete_from_indexes(...)` and no `record_truncate()` in this path.

### 5. Wire it into `DELETE` and `TRUNCATE`

#### `DELETE FROM t` with no `WHERE`

In both `execute_delete_ctx(...)` and `execute_delete(...)`:

- keep `has_fk_references` check
- new fast-path condition:

```rust
stmt.where_clause.is_none() && !has_fk_references
```

- do **not** require `secondary_indexes.is_empty()`
- use `resolved.indexes` (all index types with columns) for rotation
- return `QueryResult::Affected { count: visible_row_count, ... }`

If `has_fk_references` is true:

- keep the current slow path unchanged

#### `TRUNCATE TABLE`

In `execute_truncate(...)`:

1. resolve table + indexes
2. reject if parent FKs reference the table
3. plan/apply bulk empty
4. reset AUTO_INCREMENT
5. return `count = 0`

This closes the indexed-table correctness gap in the existing truncate path.

### 6. Exact deferred-free call sites

Immediate commit mode:

- in `execute(...)` and `execute_with_ctx(...)`, before `txn.commit()?`, capture:

```rust
let committing_txn_id = txn.active_txn_id();
```

- after successful immediate `txn.commit()?`, call:

```rust
if let Some(txn_id) = committing_txn_id {
    txn.release_committed_frees(storage, &[txn_id])?;
}
```

Do this at every SQL-visible commit point:

- autocommit statement success
- explicit `COMMIT`
- implicit DDL pre-commit + post-DDL commit
- autocommit=false read-only `SELECT` commit

Group commit mode:

- in `crates/axiomdb-network/src/mysql/group_commit.rs`, after:
  - `wal_flush_and_fsync()`
  - `advance_committed(&ids)`

add:

```rust
guard.txn.release_committed_frees(&mut guard.storage, &ids)?;
```

Do **not** free anything on fsync failure.

This closes the group-commit loose end explicitly.

## Implementation phases

1. Add `TxnManager` deferred-free state, savepoint integration, and unit tests.
2. Add `CatalogWriter::update_table_data_root(...)`.
3. Add executor-local root/page helpers and the shared bulk-empty helper.
4. Replace indexed full-table delete fast-path in ctx and non-ctx executors.
5. Route `TRUNCATE TABLE` through the same helper and add parent-FK rejection.
6. Hook immediate-commit and group-commit call sites to `release_committed_frees(...)`.
7. Add executor + wire regressions and update docs/tracker when implementation closes.

## Tests to write

- unit:
  - `TxnManager` savepoint rollback truncates deferred-free tail
  - `TxnManager` rollback drops deferred frees
  - `release_committed_frees(...)` frees only committed txn batches
- integration:
  - `DELETE FROM t` fast path on PK-only table
  - `DELETE FROM t` fast path on PK + UNIQUE + secondary index table
  - `TRUNCATE TABLE t` on indexed table then reinsert same PK/UNIQUE
  - `BEGIN; DELETE FROM t; ROLLBACK;`
  - `BEGIN; SAVEPOINT s; DELETE FROM t; ROLLBACK TO SAVEPOINT s;`
  - `TRUNCATE TABLE parent` with referencing child FK fails
  - `DELETE FROM parent` with referencing child FK still uses row-by-row semantics
- wire:
  - MySQL client can `DELETE FROM` PK table and then reinsert same PK
  - `TRUNCATE TABLE` + reinsertion over wire
  - explicit `BEGIN/ROLLBACK` proves roots restore correctly
- bench:
  - rerun `crud_flow/delete`
  - compare before/after against SQLite baseline from `axiomdb_bench`

## Anti-patterns to avoid

- do not keep the current `secondary_indexes.is_empty()` gate
- do not free old pages before commit durability is confirmed
- do not patch correctness by sprinkling ad hoc heap visibility checks into a
  subset of index paths while leaving stale roots alive
- do not leave `TRUNCATE TABLE` on the old heap-stamp path for indexed tables
- do not update only index roots and leave the heap root on the old chain;
  that would keep delete performance good but miss the “bulk truncate” goal

## Risks

- Risk: freeing pages in the wrong phase breaks rollback or group commit.
  - Mitigation: queue frees per txn and release only from explicit commit sites.
- Risk: cache/state uses stale roots after catalog rotation.
  - Mitigation: `ctx.invalidate_all()` once per bulk-empty operation.
- Risk: `update_index_root(...)` in a loop scans the catalog repeatedly.
  - Mitigation: acceptable for Phase 5 index counts; keep API simple now.
- Risk: crash after WAL fsync but before all deferred frees finish leaks pages.
  - Mitigation: accept as a documented Phase 7 cleanup debt; correctness is not affected.
