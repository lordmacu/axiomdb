# Plan: 5.17 — In-place B+Tree write path expansion

## Files to create/modify

- `crates/axiomdb-index/src/tree.rs`
  - keep the existing no-split `insert_leaf(...)` same-page contract explicit
  - add same-page `delete_leaf(...)` persistence when the post-delete leaf is
    still valid without rebalance
  - make `delete_subtree(...)` skip parent rewrites when the child page ID is
    unchanged
  - change the “child split + parent has room” insert path to write the parent
    back to the same page instead of alloc/free
  - avoid extra `internal_underfull()` reads on paths where the parent key count
    cannot change
- `crates/axiomdb-index/tests/integration_btree.rs`
  - add a counting storage wrapper for alloc/free/write assertions
  - add regression tests for same-page insert, same-page delete, ancestor
    rewrite elimination, and child-split parent absorption
- `crates/axiomdb-index/benches/btree.rs`
  - add focused microbenchmarks for delete-heavy and mixed delete+insert paths
    that expose write-path churn more directly than point-lookup benches
- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add an executor-level regression for indexed `UPDATE` / `DELETE` workloads
    so the storage optimization is still exercised through real DML
- `docs-site/src/internals/btree.md`
  - on implementation close, replace the stale “pure CoW for all writes”
    narrative with the real hybrid Phase 5 model and document why

## Reviewed first

These files were reviewed before writing this plan:

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
- `specs/fase-05/spec-5.17-in-place-btree-write-path.md`

Research reviewed before writing this plan:

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

- `insert_leaf(...)` already mutates the leaf in place on the no-split path, so
  the implementation must extend that contract instead of reintroducing it.
- The strongest measurable gap in current workloads is not “every insert
  allocates a new leaf”; it is the remaining page churn in delete/update paths
  plus parent rewrites that continue after local mutations.
- The current runtime still serializes writers, which makes same-page writes
  acceptable in Phase 5 without inventing Phase 7 epoch machinery.

### What we borrow

- `research/sqlite/src/btree.c`
  - borrow: keep the fast path local to the page whenever occupancy remains
    valid; structural balancing is the fallback, not the default
- `research/mariadb-server/storage/innobase/include/btr0cur.h`
  - borrow: explicit optimistic-vs-structural split in the algorithm
- `research/postgres/src/backend/access/nbtree/nbtinsert.c`
  - borrow: if the parent can absorb a child split, stop rewrite propagation
    there instead of bubbling churn upward

### What we reject

- broad “always mutate first, rebalance later” delete logic for underfull pages
  in this subphase; that would change failure behavior of structural delete
  paths more than necessary
- testing churn through `page_count()` or timing alone
- touching SQL executor/index-maintenance code to fake the improvement while
  leaving the B+Tree hot path unchanged

### How AxiomDB adapts it

- AxiomDB will keep structural cases on the current code path and only widen
  the same-page fast path where the subtree can be completed locally
- the implementation will make page-ID stability itself the contract:
  unchanged page ID means unchanged ancestor path above that page

## Algorithm / Data structure

### 1. Keep insert fast-path semantics explicit

No API change is needed for the already-existing path:

```rust
if node.num_keys() < threshold {
    write_leaf_same_pid(storage, old_pid, mutated_leaf)?;
    return Ok(InsertResult::Ok(old_pid));
}
```

This remains the base case that higher levels can use to stop rewrite
propagation.

### 2. Add same-page helper(s) for leaf/internal persistence

Add small local helpers in `tree.rs` to eliminate duplicate “Page::new +
checksum + write_page” boilerplate:

```rust
fn write_leaf_same_pid(
    storage: &mut dyn StorageEngine,
    pid: u64,
    node: LeafNodePage,
) -> Result<u64, DbError>;

fn write_internal_same_pid(
    storage: &mut dyn StorageEngine,
    pid: u64,
    node: InternalNodePage,
) -> Result<u64, DbError>;
```

Exact rule:

- both helpers always return `pid`
- neither helper allocates or frees pages
- `in_place_update_child(...)` should reuse `write_internal_same_pid(...)`
  instead of duplicating page-write logic

This closes the exact persistence pattern before implementation starts.

### 3. Make child-split parent absorption same-page

In `insert_subtree(...)`, when a child returns `Split { left_pid, right_pid, sep }`
and `n < ORDER_INTERNAL`:

1. mutate `node2` with the new left/right layout
2. persist `node2` with `write_internal_same_pid(storage, pid, node2)`
3. return `InsertResult::Ok(pid)`

Exact consequence:

- if this internal page is itself a child, its parent sees `new_child_pid == child_pid`
  and stops rewrite propagation
- only actual internal splits continue to allocate new pages

### 4. Make non-structural leaf delete same-page

In `delete_leaf(...)`:

```rust
node.remove_at(idx);
let underfull = !is_root && node.num_keys() < MIN_KEYS_LEAF;

if !underfull {
    write_leaf_same_pid(storage, old_pid, node)?;
    return Ok(DeleteResult::Deleted {
        new_pid: old_pid,
        underfull: false,
    });
}

// existing allocate-new path for structural delete remains
```

Exact decision:

- root leaf deletes are always same-page
- non-root deletes only stay same-page when they do not require rebalance
- the current allocate-new path is preserved for underfull structural deletes

### 5. Stop redundant parent work on delete

In `delete_subtree(...)`, after the recursive child delete returns:

```rust
if !underfull && new_child_pid == child_pid {
    return Ok(DeleteResult::Deleted {
        new_pid: pid,
        underfull: false,
    });
}
```

If `!underfull` but `new_child_pid != child_pid`, keep using
`in_place_update_child(...)`.

Exact decision:

- after `in_place_update_child(...)`, return `underfull: false` directly
- do not call `internal_underfull()` on a path that only changed a child pointer
  and did not change key count

### 6. Leave structural delete paths unchanged

For non-root deletes that leave the leaf underfull:

- keep the current allocate-new leaf path
- keep `rebalance(...)`, `rotate_left(...)`, `rotate_right(...)`,
  `merge_children(...)`, and `collapse_root(...)` unchanged

This keeps the risky part of delete semantics stable for `5.17`.

### 7. Add instrumentation-based tests

In `integration_btree.rs`, define a local test-only wrapper:

```rust
struct CountingStorage {
    inner: MemoryStorage,
    allocs: usize,
    frees: usize,
    writes: usize,
}
```

Rules:

- increment counters on every forwarded `alloc_page`, `free_page`, `write_page`
- expose snapshots so tests can assert deltas around a single insert/delete
- do not use `page_count()` as the fast-path oracle

## Implementation phases

1. Refactor `tree.rs` to centralize same-page writes with local helpers.
2. Convert the parent-absorb insert path to same-page writes.
3. Convert non-structural leaf delete to same-page writes.
4. Add delete-side ancestor rewrite elimination and remove unnecessary
   `internal_underfull()` reads.
5. Add counting-storage integration tests for alloc/free deltas.
6. Add executor-level regression and focused microbenchmarks.
7. Update technical docs to describe the real hybrid model and the exact scope
   of the optimization.

## Tests to write

- unit:
  - same-page helper writes preserve checksum and page ID
- integration:
  - no-split insert: `alloc_page/free_page` delta is `0/0`
  - non-underflow delete: `alloc_page/free_page` delta is `0/0`
  - delete with unchanged child page ID does not rewrite the parent
  - child split absorbed by parent keeps the parent page ID
  - mixed insert/delete workload still preserves lookup/range correctness
  - SQL executor regression: indexed `UPDATE` and `DELETE` still return correct
    results on PK-backed tables
- bench:
  - focused B+Tree mixed delete+insert benchmark in `crates/axiomdb-index/benches/btree.rs`
  - before/after comparison with `benches/comparison/local_bench.py` for
    indexed `update` and `delete_where`
  - before/after comparison with `benches/comparison/axiomdb_bench/src/main.rs`
    for full-cycle insert/delete scenarios

## Anti-patterns to avoid

- Do not “optimize” this in SQL/executor code while leaving `tree.rs` hot paths
  unchanged.
- Do not claim success based only on wall-clock timing; prove reduced page churn
  with counting storage.
- Do not use same-page deletes for underfull non-root leaves in this subphase.
- Do not re-document the tree as pure CoW after the implementation; that is no
  longer true in Phase 5.
- Do not add concurrency justifications that rely on Phase 7 behavior that does
  not exist yet.

## Risks

- Risk: same-page parent absorption after child split accidentally breaks key
  ordering or separator placement.
  - Mitigation: add explicit tests that verify lookup correctness after the
    first split absorbed by a non-full parent.
- Risk: delete-side fast path skips a parent rewrite when a separator key should
  have changed.
  - Mitigation: only skip when `new_child_pid == child_pid` and the delete did
    not underflow; that means the parent key layout is unchanged.
- Risk: timing benches fluctuate and hide the actual improvement.
  - Mitigation: treat alloc/free deltas as the correctness oracle and benches
    as supporting evidence.
- Risk: docs remain stale and keep promising pure Copy-on-Write.
  - Mitigation: update `docs-site/src/internals/btree.md` in the same
    implementation commit.
