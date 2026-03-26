# Spec: 5.17 — In-place B+Tree write path expansion

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-index/src/page_layout.rs`
- `crates/axiomdb-index/src/lib.rs`
- `crates/axiomdb-index/tests/integration_btree.rs`
- `crates/axiomdb-index/benches/btree.rs`
- `crates/axiomdb-storage/src/engine.rs`
- `crates/axiomdb-storage/src/memory.rs`
- `crates/axiomdb-storage/src/page.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-embedded/src/lib.rs`
- `benches/comparison/local_bench.py`
- `benches/comparison/axiomdb_bench/src/main.rs`
- `docs/fase-2.md`
- `docs-site/src/internals/btree.md`

These research files were reviewed before writing this spec:

- `research/sqlite/src/btree.c`
- `research/mariadb-server/storage/innobase/include/btr0cur.h`
- `research/mariadb-server/storage/innobase/page/page0cur.cc`
- `research/mariadb-server/storage/innobase/row/row0ins.cc`
- `research/postgres/src/backend/access/nbtree/nbtinsert.c`
- `research/postgres/src/backend/access/nbtree/nbtpage.c`
- `research/duckdb/src/storage/data_table.cpp`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- `research/oceanbase/src/storage/ls/ob_ls_tablet_service.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
- `research/datafusion/datafusion/sql/src/statement.rs`

## Research synthesis

### AxiomDB-first constraints

- The tracker text for `5.17` is partially stale: `insert_leaf(...)` already has
  an in-place fast path when the leaf stays below the split threshold.
- The real remaining write amplification is:
  - `delete_leaf(...)` always alloc/free even when no rebalance is needed
  - `delete_subtree(...)` still rewrites the parent when the child keeps the
    same page ID
  - `insert_subtree(...)` still alloc/free on the parent when a child split is
    absorbed by a non-full internal page
- Phase 5 runtime is still effectively single-writer:
  - MySQL server paths execute through `Arc<Mutex<Database>>`
  - embedded paths also serialize mutable database access
- AxiomDB does not yet have Phase 7 lock-free readers, epoch reclamation, or
  secondary-index MVCC, so `5.17` must be justified by the current single-writer
  model, not by future CoW guarantees.

### What we borrow

- `research/sqlite/src/btree.c`
  - borrow: mutate the target page locally first, and only escalate to
    structural balancing when occupancy thresholds require it
- `research/mariadb-server/storage/innobase/include/btr0cur.h`
  - borrow: separate optimistic page-local writes from pessimistic structural
    paths
- `research/mariadb-server/storage/innobase/page/page0cur.cc`
  - borrow: leaf insert/delete on the hot path should be direct page-local
    edits, not page replacement by default
- `research/postgres/src/backend/access/nbtree/nbtinsert.c`
  - borrow: when a child split can be absorbed by a parent page with room, the
    parent write should stay local to that page instead of forcing upper-level
    churn

### What we reject

- treating `5.17` as “enable in-place insert for the first time”, because that
  would be factually wrong for the current codebase
- copying PostgreSQL/InnoDB latch, buffer, or WAL machinery into Phase 5
- mutating underfull delete paths in place before deciding whether a rebalance
  succeeds; for structural delete paths AxiomDB keeps the current allocate/new
  subtree flow
- using `page_count()` as proof that no alloc/free happened; freelist reuse can
  hide churn and is not a reliable oracle
- using DuckDB/OceanBase physical storage techniques as direct templates here;
  their storage models are not page-local B+Tree write paths like AxiomDB's

### How AxiomDB adapts it

- `5.17` extends the existing hybrid tree model instead of replacing it:
  - no-split leaf insert remains in-place
  - no-underflow leaf delete becomes in-place
  - parent absorb-with-room after child split becomes in-place
  - ancestor rewrites are skipped when the child page ID is unchanged
- split / merge / rotate / root-collapse keep their existing structural paths
- this subphase is explicitly scoped to Phase 5's serialized writer model

## What to build (not how)

Implement the real remaining B+Tree in-place write-path optimization for Phase 5.

### Surface 1: keep the existing in-place insert contract

`insert_leaf(...)` already writes the leaf back to the same page when:

- the target key does not already exist
- `num_keys < fill_threshold`
- no leaf split is required

That behavior is now part of the subphase contract and must stay true.

### Surface 2: add in-place leaf delete when no rebalance is needed

`delete_leaf(...)` must stop allocating a replacement page when:

- the deleted key exists
- the target leaf is the root, or
- after deletion the leaf still has at least `MIN_KEYS_LEAF`

In those cases:

- the modified leaf stays on the same page ID
- the operation performs no `alloc_page()` / `free_page()` pair for the leaf
- the parent path must be allowed to observe “same child page ID” and avoid
  unnecessary rewrites

When a non-root leaf becomes underfull after deletion:

- `delete_leaf(...)` may keep the current allocate-new subtree flow
- parent rebalance / merge / rotate behavior remains structural and unchanged

### Surface 3: eliminate ancestor rewrites when child page ID is unchanged

`delete_subtree(...)` must mirror the optimization that `insert_subtree(...)`
already has:

- if the child delete finishes without underflow and returns the same page ID,
  the parent must not be rewritten at all
- if the parent only needs a child-pointer update and no structural change, the
  parent stays on the same page ID
- the underfull flag for that parent path remains `false` because key count did
  not change

### Surface 4: absorb child split into parent in place when the parent has room

When `insert_subtree(...)` receives `InsertResult::Split { ... }` from a child
and the current internal node is not full:

- the parent must absorb the separator and right-child pointer on the same page
  ID
- the parent must no longer allocate a replacement page just to insert one
  separator into a non-full internal node
- ancestors above that parent must not be rewritten if the parent page ID did
  not change

### Surface 5: keep structural paths unchanged

These paths are not redesigned in `5.17`:

- leaf split
- internal split
- rotate left/right
- merge children
- root collapse

They must continue to behave exactly as today, except that they may be reached
less often because more writes finish on the same page.

## Inputs / Outputs

- Input:
  - `BTree::insert(...)`
  - `BTree::delete(...)`
  - `BTree::insert_in(...)`
  - `BTree::delete_in(...)`
  - internal helpers:
    - `insert_subtree(...)`
    - `insert_leaf(...)`
    - `delete_subtree(...)`
    - `delete_leaf(...)`
- Output:
  - same public API and return types as today
  - same lookup / range / uniqueness / FK observable semantics as today
  - fewer page allocations, frees, and parent rewrites on non-structural write paths
- Errors:
  - same `DbError` behavior as today:
    - `DuplicateKey`
    - key validation errors
    - storage read/write/alloc/free errors
    - corruption errors

## Use cases

1. Inserting into a leaf with available room keeps the leaf page ID unchanged
   and does not force ancestor rewrites.
2. Deleting one key from a non-root leaf that remains above `MIN_KEYS_LEAF`
   keeps the same leaf page ID and skips parent rewrite when the child page ID
   is unchanged.
3. Deleting from a root leaf down to zero keys keeps the root leaf page ID and
   does not allocate a replacement page.
4. Inserting enough rows to split a child, when the parent still has room,
   updates the parent in place and stops rewrite propagation at that parent.
5. Deleting from a sparse leaf that becomes underfull still follows the
   structural rebalance path and remains correct.
6. SQL `UPDATE` on a table with a PRIMARY KEY index benefits because the PK
   index delete+insert pair now avoids page churn whenever both operations stay
   on non-structural paths.

## Acceptance criteria

- [ ] `insert_leaf(...)` no-split writes keep the same leaf page ID and do not
      call `alloc_page()` or `free_page()`.
- [ ] `delete_leaf(...)` on a root leaf, or on a non-root leaf that remains at
      `>= MIN_KEYS_LEAF`, keeps the same leaf page ID and does not call
      `alloc_page()` or `free_page()` for that leaf.
- [ ] `delete_subtree(...)` does not rewrite the parent when a child delete
      completes with `new_child_pid == child_pid` and `underfull == false`.
- [ ] `insert_subtree(...)` does not allocate/free the parent when a child split
      is absorbed by a parent that still has room for one more separator.
- [ ] Structural delete cases that require rotate/merge still rebalance
      correctly and preserve lookup/range correctness.
- [ ] Existing B+Tree invariants remain true after mixed insert/delete
      workloads: keys remain ordered, lookups return the correct `RecordId`,
      and range scans stay sorted.
- [ ] The proof of “no churn on fast path” uses an instrumented storage wrapper
      that counts `alloc_page()` / `free_page()` calls; it is not inferred from
      `page_count()`.
- [ ] End-to-end benchmarks for indexed `UPDATE`/`DELETE` and focused B+Tree
      microbenchmarks show measurable improvement versus the pre-change
      baseline recorded before implementation.

## Out of scope

- multi-writer correctness
- Phase 7 epoch reclamation and lock-free reader safety
- restoring a pure Copy-on-Write model for every write path
- redesigning rotate/merge algorithms
- secondary-index MVCC or visibility-aware uniqueness checks
- heap/WAL/network costs that may still dominate the remaining INSERT gap after
  B+Tree page churn is reduced

## Dependencies

- current Phase 5 single-writer execution model
- existing `in_place_update_child(...)` helper in `crates/axiomdb-index/src/tree.rs`
- existing split / merge / rotate implementations
- existing B+Tree integration tests and comparison benchmarks

## ⚠️ DEFERRED

- [ ] Phase 7 lock-free readers + epoch reclamation remain the place to
      reconcile the current hybrid tree model with the original pure-CoW design
- [ ] If INSERT remains materially slower than target after page-churn removal,
      the remaining gap must be revisited in heap/WAL/wire paths rather than
      overloading `5.17` beyond B+Tree scope
