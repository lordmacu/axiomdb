# Plan: 5.19 — B+Tree batch delete (sorted single-pass)

## Files to create/modify

- `crates/axiomdb-index/src/tree.rs`
  - add the new exact-key batch delete API
  - add private recursive/page-local helpers for batch deletion
  - collapse the root once per batch, not once per key
- `crates/axiomdb-sql/src/index_maintenance.rs`
  - add per-index key collection helpers for batched delete
  - add statement-level DELETE and UPDATE batch maintenance helpers
  - keep current single-row helpers as wrappers/fallbacks
- `crates/axiomdb-sql/src/executor.rs`
  - replace per-row delete loops in DELETE with per-index batch delete
  - replace old-key per-row delete loops in UPDATE with per-index batch delete
  - persist final index roots once per affected index
- `crates/axiomdb-index/tests/integration_btree.rs`
  - add correctness tests for `delete_many_in(...)`
  - add root-collapse / cross-leaf / underflow regressions
- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add DELETE/UPDATE regressions that prove batch delete is used and remains
    correct
- `benches/comparison/local_bench.py`
  - keep `delete_where` and `update` as the main before/after regression target

## Reviewed first

These files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-index/src/page_layout.rs`
- `crates/axiomdb-index/src/lib.rs`
- `crates/axiomdb-index/tests/integration_btree.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `benches/comparison/local_bench.py`
- `specs/fase-05/spec-5.17-in-place-btree-write-path.md`
- `specs/fase-05/spec-5.18-heap-insert-tail-pointer-cache.md`
- `specs/fase-06/spec-6.1-6.3-secondary-indexes.md`
- `specs/fase-06/spec-6.3b-indexed-delete-where.md`
- `specs/fase-05/spec-5.19-btree-batch-delete.md`

Research reviewed before writing this plan:

- `research/sqlite/src/delete.c`
- `research/postgres/src/backend/access/nbtree/nbtree.c`
- `research/mariadb-server/storage/innobase/include/btr0bulk.h`
- `research/mariadb-server/storage/innobase/include/row0merge.h`
- `research/mariadb-server/storage/innobase/include/ibuf0ibuf.h`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`

## Research synthesis

### AxiomDB-first constraints

- The current hot loop is already isolated: `delete_from_indexes(...)` computes
  one exact key and calls `BTree::delete_in(...)` once per row and per index.
- `6.3b` already removed the full-heap scan from indexable `DELETE ... WHERE`,
  so `5.19` must target index mutation itself, not candidate discovery.
- The executor already has the whole row set in memory before index deletes for
  both DELETE and UPDATE, so key grouping can happen without new planner work.
- Root page IDs are already mutable in-memory during statement execution. That
  makes it safe to batch many deletes per index and persist only the final root.

### What we borrow / reject / adapt

- `research/postgres/src/backend/access/nbtree/nbtree.c`
  - borrow: group many removals and apply them page-locally in one operation
  - adapt: exact-key batch delete on a mutable B+Tree, not VACUUM callbacks
- `research/mariadb-server/storage/innobase/include/btr0bulk.h`
  - borrow: ordered bulk work deserves its own tree API
  - adapt: `delete_many_in(...)` instead of looping point deletes
- `research/mariadb-server/storage/innobase/include/row0merge.h`
  - borrow: sort/merge style index work
  - adapt: sort exact encoded delete keys per index before tree mutation
- `research/mariadb-server/storage/innobase/include/ibuf0ibuf.h`
  - reject: change buffer for this subphase
  - adapt: document it as a future follow-up, not part of `5.19`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
  - borrow: explicit staging between row selection and physical delete
  - adapt: executor collects rows first, then index maintenance groups keys
- `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
  - borrow: separate delete intent preparation from storage mutation
  - adapt: `index_maintenance.rs` becomes the staging layer
- `research/sqlite/src/delete.c`
  - borrow: keep row discovery and mutation separate
  - adapt: no VM/opcodes, just explicit Rust helpers

## Algorithm / Data structure

### 1. New B+Tree API: exact-key batch delete

Add a new public helper in `tree.rs`:

```rust
pub fn delete_many_in(
    storage: &mut dyn StorageEngine,
    root_pid: &AtomicU64,
    sorted_keys: &[Vec<u8>],
) -> Result<usize, DbError>
```

Contract:

- `sorted_keys` are already exact encoded keys for this one index
- they must be sorted ascending by byte order
- they may be empty
- the function returns how many keys were actually removed
- callers read the final root from `root_pid` after the call

Implementation rule:

- do not implement this as `for key in sorted_keys { delete_in(...) }`

### 2. Batch delete recursion in the B+Tree

Add a private helper:

```rust
struct BatchDeleteResult {
    new_pid: u64,
    underfull: bool,
    deleted: usize,
}

fn delete_many_subtree(
    storage: &mut dyn StorageEngine,
    pid: u64,
    sorted_keys: &[Vec<u8>],
    is_root: bool,
) -> Result<BatchDeleteResult, DbError>
```

#### Leaf algorithm

Leaf deletion is a merge between:

- the leaf's existing sorted keys
- the sorted keys-to-delete slice for that leaf

Pseudo:

```text
read leaf
walk both arrays left-to-right
copy survivors into a compacted leaf image
count deleted keys

if deleted == 0:
    return unchanged

if root or resulting count >= MIN_KEYS_LEAF:
    write leaf once to same pid
    return underfull=false
else:
    write compacted leaf to replacement pid
    free old pid
    return underfull=true
```

This keeps the current Phase 5 structural contract for underflow cases, while
eliminating repeated point deletes inside one leaf.

#### Internal-node algorithm

Internal batch delete is not a loop of point deletes. It works in three phases:

```text
1. Partition the sorted key slice by child range using the current separators.
2. Recurse once per affected child with its contiguous key sub-slice.
3. Normalize the parent once after all affected children return.
```

Normalization rules:

- update child pointers locally for children whose pid changed
- collect underfull children
- rebalance/merge underfull children left-to-right using the existing
  rotate/merge logic as building blocks where possible
- write the parent once after the normalization pass
- return `underfull=true` only if the final normalized parent is still below
  the minimum occupancy and is not the root

Important:

- the batch helper may reuse the current rotate/merge code, but it must not
  route back through per-key `delete_subtree(...)`
- root collapse still happens once at the end of `delete_many_in(...)`

### 3. SQL-layer grouping: exact keys per index

Add explicit collectors in `index_maintenance.rs`.

#### DELETE collector

```rust
fn collect_delete_keys_by_index(
    indexes: &[IndexDef],
    rows: &[(RecordId, Vec<Value>)],
    compiled_preds: &[Option<Expr>],
) -> Result<Vec<Vec<Vec<u8>>>, DbError>
```

Rules:

- output is parallel to `indexes`
- each bucket contains exact encoded keys for that index
- skip rows that do not belong in the index under the existing rules:
  - partial predicate false
  - any indexed key value is `NULL`
- for non-unique / FK indexes, append `encode_rid(rid)` exactly as today
- sort each non-empty bucket ascending before returning

#### UPDATE collector

Add a dedicated batch shape:

```rust
struct IndexUpdateBatch {
    delete_keys: Vec<Vec<u8>>,
    insert_rows: Vec<(Vec<Value>, RecordId)>,
}

fn collect_update_batches_by_index(
    indexes: &[IndexDef],
    rows: &[(RecordId, Vec<Value>, RecordId, Vec<Value>)],
    compiled_preds: &[Option<Expr>],
) -> Result<Vec<IndexUpdateBatch>, DbError>
```

Rules for partial indexes:

- old row matched, new row matched   -> delete + insert
- old row matched, new row not match -> delete only
- old row not match, new row matched -> insert only
- neither matched                    -> no-op for that index

`delete_keys` must be sorted ascending before execution.

### 4. New index-maintenance entrypoints

Add statement-level helpers:

```rust
pub fn delete_many_from_indexes(...)
pub fn apply_update_batches_to_indexes(...)
```

#### DELETE helper behavior

For each affected index:

```text
root = AtomicU64(current_root)
delete_many_in(storage, &root, sorted_delete_keys)
if any key deleted:
    bloom.mark_dirty(index_id)
final_root = root.load()
if final_root != original_root:
    return (index_id, final_root)
```

No per-row root persistence.

#### UPDATE helper behavior

For each affected index:

```text
root = AtomicU64(current_root)
delete_many_in(storage, &root, sorted_delete_keys)
if delete_keys not empty:
    bloom.mark_dirty(index_id)

for (new_row, new_rid) in insert_rows:
    insert_into_indexes_for_one_index_using_root(root, ...)
    bloom.add(index_id, key) as today

final_root = root.load()
if final_root != original_root:
    return (index_id, final_root)
```

Critical closed decision:

- DELETE phase runs before INSERT phase for a given index in UPDATE
- the catalog root is persisted once per index after both phases finish

### 5. Executor integration

#### `execute_delete_ctx(...)` and `execute_delete(...)`

Replace:

```text
for each row:
    delete_from_indexes(...)
    maybe persist root
```

With:

```text
compiled_preds = ...
root_updates = delete_many_from_indexes(...)
persist root_updates once
refresh in-memory index roots once
invalidate ctx cache once if any root changed
```

#### `execute_update_ctx(...)` and `execute_update(...)`

Keep row qualification and heap updates as they are conceptually, but change
index maintenance order:

```text
1. Build to_update as today.
2. Run FK checks as today.
3. Apply heap updates and collect:
   (old_rid, old_values, new_rid, new_values)
4. Build per-index update batches.
5. Apply batch old-key deletes per index.
6. Apply existing insert path for new keys per index.
7. Persist final roots once per index.
8. Invalidate ctx cache once if any root changed.
```

Do not persist roots inside the row loop anymore.

## Implementation phases

1. Add `delete_many_in(...)` and private batch recursion helpers in
   `crates/axiomdb-index/src/tree.rs`.
2. Add B+Tree integration tests for same-leaf, cross-leaf, underflow, merge,
   and root-collapse cases.
3. Add per-index key collectors and statement-level batch helpers in
   `crates/axiomdb-sql/src/index_maintenance.rs`.
4. Integrate batched delete into `execute_delete_ctx(...)` and
   `execute_delete(...)`.
5. Integrate batched old-key delete into `execute_update_ctx(...)` and
   `execute_update(...)`.
6. Add SQL integration regressions for DELETE, UPDATE, partial indexes, and PK
   reuse semantics.
7. Re-run the existing comparison benchmark scenarios for `delete_where` and
   `update`.

## Tests to write

- unit / index:
  - `delete_many_in` empty input is a no-op
  - `delete_many_in` removes multiple keys from one leaf with one final leaf
    image
  - `delete_many_in` removes keys spanning many leaves and preserves sorted
    lookups/range scan
  - `delete_many_in` handles underflow + merge/root collapse correctly
  - deleting missing keys is harmless and does not corrupt the tree
- unit / SQL maintenance:
  - `collect_delete_keys_by_index` encodes unique vs non-unique vs FK keys
    correctly
  - `collect_update_batches_by_index` handles partial-index old/new membership
    transitions correctly
  - root updates are emitted once per index batch
- integration:
  - `DELETE FROM bench_users WHERE id > 2500` leaves the PK index correct
  - `UPDATE bench_users SET score = score + 1 WHERE active = TRUE` preserves PK
    lookup correctness and uses batched old-key deletion
  - partial index delete/update remains correct
  - FK auto-index delete remains correct
  - same-key UNIQUE/PRIMARY update replacement stays valid
- bench:
  - `python3 benches/comparison/local_bench.py --scenario delete_where --rows 5000`
  - `python3 benches/comparison/local_bench.py --scenario update --rows 5000`

## Anti-patterns to avoid

- Do not implement `delete_many_in(...)` as a loop around `delete_in(...)`.
- Do not persist index roots inside the row loop anymore.
- Do not sort by `RecordId`; sort by the full encoded key bytes that the B+Tree
  actually stores.
- Do not weaken partial-index or `NULL` skip rules when collecting batch keys.
- Do not combine `5.19` with change-buffer / deferred-write machinery.
- Do not re-open DELETE candidate discovery here; `6.3b` already owns that
  concern.

## Risks

- Risk: multiple children underflow in the same internal node after a large
  batch delete.
  Mitigation: explicit normalization pass after all child batch results, plus
  dedicated tree tests for cross-leaf range deletes and root collapse.

- Risk: UPDATE on UNIQUE/PRIMARY indexes could spuriously fail if inserts are
  attempted before old keys are removed.
  Mitigation: delete phase is always executed before insert phase per index.

- Risk: partial index membership transitions could remove or insert the wrong
  keys.
  Mitigation: batch collectors evaluate the stored predicate on both old and new
  row values explicitly.

- Risk: non-unique and FK indexes could delete the wrong entries if the
  `RecordId` suffix is omitted.
  Mitigation: collectors must use the same `key || encode_rid(rid)` encoding as
  the current single-row path.

- Risk: root changes after batch delete and subsequent inserts could drift from
  the catalog if persisted too early.
  Mitigation: persist the final root once per index after the whole index batch
  finishes.

## Assumptions

- `6.3b` remains the path that makes `DELETE ... WHERE` on PK/range predicates
  feed `5.19` enough candidate rows to matter.
- Phase 5/6 still operate under the current serialized-writer assumptions.
- If benchmarks show that UPDATE remains dominated by insert-side index writes
  after `5.19`, the next subphase should target batch insert or deferred index
  writes explicitly instead of overloading this one retroactively.
