# Plan: 5.20 — Stable-RID UPDATE fast path

## Files to create/modify

- `crates/axiomdb-storage/src/heap.rs`
  - add same-slot rewrite primitive for one tuple
  - enforce "new encoded len fits in existing slot payload" precondition
  - update `row_version`, slot length, and checksum on rewrite

- `crates/axiomdb-storage/src/heap_chain.rs`
  - add grouped batch same-slot update primitive by `(page_id, slot_id)`
  - read each affected page once, rewrite many slots, write once
  - return per-row old/new byte images needed by WAL and higher layers

- `crates/axiomdb-sql/src/table.rs`
  - add `update_rows_stable_rid_batch(...)`
  - add `update_rows_stable_rid_batch_with_ctx(...)`
  - keep current delete+insert update helpers as fallback path

- `crates/axiomdb-sql/src/executor/update.rs`
  - compute evaluated `changed_cols`
  - partition rows into stable-RID candidates vs fallback rows
  - compute affected indexes per stable-RID row
  - route stable-RID rows through the new batch heap rewrite path
  - route fallback rows through the existing path

- `crates/axiomdb-sql/src/index_maintenance.rs`
  - add helpers to determine whether an index is affected by one UPDATE row
  - add row-to-index batching for only the affected indexes on stable-RID rows
  - keep full index maintenance for fallback rows

- `crates/axiomdb-sql/src/executor/shared.rs`
  - expose or add reusable column-reference collection helper for assignment and
    partial-predicate dependency masks

- `crates/axiomdb-wal/src/txn.rs`
  - add a dedicated stable-RID update WAL record and undo op
  - make savepoint rollback and full rollback restore old bytes

- `crates/axiomdb-wal/src/recovery.rs`
  - add recovery handling for the new stable-RID update WAL record
  - undo uncommitted in-place updates by restoring old bytes in place

- `crates/axiomdb-sql/tests/integration_executor.rs`
  - add UPDATE regressions for stable-RID fast path, fallback path, partial
    index impact, and FK correctness

- `crates/axiomdb-storage/tests/` or existing storage integration tests
  - add direct heap same-slot rewrite tests and rollback/recovery tests

- `benches/comparison/local_bench.py`
  - keep `update` as the primary benchmark target for this subphase

## Reviewed first

These files were reviewed before writing this plan:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor/update.rs`
- `crates/axiomdb-sql/src/executor/delete.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/executor/shared.rs`
- `crates/axiomdb-sql/src/partial_index.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `crates/axiomdb-storage/src/heap.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-wal/src/recovery.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `crates/axiomdb-index/tests/integration_btree.rs`
- `benches/comparison/local_bench.py`
- `specs/fase-05/spec-5.19-btree-batch-delete.md`
- `specs/fase-05/spec-5.20-stable-rid-update-fast-path.md`

Research reviewed before writing this plan:

- `research/sqlite/src/update.c`
- `research/sqlite/src/delete.c`
- `research/mariadb-server/sql/sql_update.cc`
- `research/mariadb-server/storage/innobase/include/btr0cur.h`
- `research/mariadb-server/storage/innobase/row/row0upd.cc`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/postgres/src/backend/access/heap/heapam.c`
- `research/duckdb/src/execution/operator/persistent/physical_update.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_update_op.cpp`
- `research/oceanbase/src/sql/das/ob_das_update_op.cpp`
- `research/datafusion/datafusion/core/src/physical_planner.rs`

## Research synthesis

### AxiomDB-first constraints

- The benchmark that hurts (`UPDATE bench_users SET score = score + 1 WHERE active = TRUE`)
  does not point first to planner work. The expensive part is the write path.
- The existing `5.20` tracker text is incomplete because the current heap update
  path changes `RecordId`; selective index skipping is only correct after row
  identity becomes stable.
- `5.19` already gave AxiomDB a batch delete primitive, so the highest-value
  follow-up is to stop forcing old-key removal for indexes that are truly
  unaffected.
- The Phase 5 write model still allows a pragmatic in-place optimization under
  the current single-writer / no-concurrent-reader assumption.

### What we borrow / reject / adapt

- `research/sqlite/src/update.c`
  - borrow: changed-column analysis and index-impact analysis
  - adapt: explicit Rust masks instead of VDBE register bookkeeping

- `research/mariadb-server/sql/sql_update.cc`
  - borrow: write-set driven avoidance of unnecessary work
  - adapt: per-row `changed_cols` and `index_affected` decisions in the executor

- `research/mariadb-server/storage/innobase/include/btr0cur.h`
  - borrow: local in-place update path vs structural fallback path
  - adapt: same-slot rewrite vs existing delete+insert

- `research/postgres/src/backend/access/heap/heapam.c`
  - borrow: unchanged indexed columns should avoid index rewrites
  - reject: full HOT chain in this subphase
  - adapt: stable-RID same-slot rewrite only when bytes fit

- `research/duckdb/src/execution/operator/persistent/physical_update.cpp`
  - borrow: in-place mode and delete+insert mode are different physical plans
  - adapt: choose the mode row by row inside AxiomDB UPDATE

- `research/oceanbase/src/sql/das/ob_das_update_op.cpp`
  - borrow: old/new row staging up front
  - adapt: stage old bytes and new encoded bytes before mutating heap/indexes

## Algorithm / Data structure

### 1. Heap primitive: same-slot rewrite

Add a new low-level heap helper:

```rust
pub fn rewrite_tuple_same_slot(
    page: &mut Page,
    slot_id: u16,
    new_data: &[u8],
) -> Result<Vec<u8>, DbError>
```

Contract:

- succeeds only if `align8(RowHeader + new_data)` fits in the tuple's current
  allocated slot payload
- returns the old row bytes (without `RowHeader`) for WAL/undo
- rewrites the tuple in place
- increments `row_version`
- keeps `txn_id_created` / `txn_id_deleted` stable for this Phase 5 path
- updates the slot's logical length if `new_data` is shorter than the old row

Critical closed decision:

- this helper does **not** attempt to preserve old snapshot-visible versions
- it is a Phase 5 optimization that relies on the current execution model
- Phase 7 will revisit this with proper HOT/forwarded versions if needed

### 2. HeapChain batch API

Add a page-grouped batch API:

```rust
pub fn update_batch_same_slot(
    storage: &mut dyn StorageEngine,
    updates: &[(RecordId, Vec<u8>)],
) -> Result<Vec<StableUpdateImage>, DbError>
```

Where:

```rust
struct StableUpdateImage {
    rid: RecordId,
    old_bytes: Vec<u8>,
    new_bytes: Vec<u8>,
}
```

Behavior:

1. sort/group updates by `page_id`
2. read each affected page once
3. rewrite every eligible slot on that page
4. write the page once
5. return the old/new images in input order for WAL logging

This API is only for already-validated same-slot updates.

### 3. Executor staging

In `execute_update_ctx(...)` and `execute_update(...)`, replace the current
"all matching rows -> one physical path" logic with staged partitioning:

```text
1. scan matching rows as today
2. for each row:
   a. evaluate assignments -> new_values
   b. encode new_values once
   c. compare against old bytes length
   d. compute changed_cols from old_values vs new_values
   e. compute affected indexes for this row
   f. if encoded row fits same slot:
        stable_rid_batch.push(...)
      else:
        fallback_batch.push(...)
3. apply stable_rid_batch
4. apply fallback_batch using current delete+insert path
5. merge counts and root updates
```

Important:

- matching-row qualification remains unchanged in this subphase
- no planner work is mixed into `5.20`

### 4. Computing `changed_cols`

Closed decision:

- `changed_cols` is computed from evaluated old/new values, not only the set of
  assignment targets

Reason:

- `SET x = x` or `SET email = LOWER(email)` that normalizes to the same value
  must not force index maintenance for `email` under the stable-RID path
- the executor already has both value vectors, so this costs little

Implementation shape:

```text
changed_cols[i] = old_values[i] != new_values[i]
```

This mask is statement-local and row-specific.

### 5. Computing `index_affected`

Add a helper in `index_maintenance.rs`:

```rust
fn index_affected_by_update(
    idx: &IndexDef,
    changed_cols: &[bool],
    compiled_pred: Option<&Expr>,
    pred_ref_mask: Option<&[bool]>,
    old_row: &[Value],
    new_row: &[Value],
) -> Result<bool, DbError>
```

Rules:

1. if any indexed key column changed -> affected
2. else if partial predicate exists and any predicate-referenced column changed:
   - evaluate predicate on old row and new row
   - if membership changed -> affected
   - if membership stayed true and predicate inputs changed in a way that could
     alter key presence semantics -> affected
3. else -> unaffected

Closed decision:

- on stable-RID rows, unaffected indexes are skipped entirely
- on fallback rows, all indexes are treated as affected regardless of
  `changed_cols`

### 6. Stable-RID index maintenance

For stable-RID rows:

- group only the affected old keys per index
- run `delete_many_from_indexes(...)` only for those indexes
- insert only the new keys for those same affected indexes
- persist roots once per affected index after delete+insert finishes

For unaffected indexes:

- do nothing
- do not dirty bloom
- do not persist roots

### 7. Fallback path

Fallback rows keep current semantics:

- `TableEngine::update_row[_with_ctx](...)`
- old-key batch delete from `5.19`
- new-key insert path as today
- all indexes treated as affected because `RecordId` changed

This avoids mixing unsafe skipping with the old heap design.

### 8. WAL / rollback / recovery

Add a dedicated WAL entry type:

```text
EntryType::UpdateInPlace
```

with payload:

```text
key       = [page_id:8][slot_id:2]
old_value = previous row bytes
new_value = replacement row bytes
```

Add a dedicated undo op:

```rust
UndoOp::UndoUpdateInPlace { page_id, slot_id, old_bytes: Vec<u8> }
```

Rollback behavior:

- restore `old_bytes` into the same slot
- restore the previous `row_version` from the old image

Recovery behavior:

- uncommitted `UpdateInPlace` entries restore `old_value` to the same slot
- committed entries require no special redo beyond the page image already being
  on disk under the current write ordering

Closed decision:

- `UpdateInPlace` does not reuse the current `record_update(...)` semantics
  because delete+insert undo is not correct for same-slot rewrites

## Implementation phases

1. Add heap same-slot rewrite primitive and direct unit tests in storage.
2. Add `HeapChain::update_batch_same_slot(...)` grouped by page.
3. Add WAL `UpdateInPlace` entry + rollback + recovery support.
4. Add executor-side staging:
   - old values
   - new values
   - old bytes
   - encoded new bytes
   - `changed_cols`
5. Add `index_affected_by_update(...)` and predicate dependency masks.
6. Wire stable-RID rows to selective index maintenance.
7. Keep fallback rows on the current path.
8. Add executor integration tests and rerun the local benchmark.

## Tests to write

- unit:
  - same-slot rewrite succeeds when new row fits
  - same-slot rewrite rejects oversized replacement row
  - row length shrink updates slot length correctly
  - predicate dependency masks mark partial index as affected correctly
  - unaffected indexes are skipped only on stable-RID rows

- integration:
  - `UPDATE score = score + 1 WHERE active = TRUE` with PK only keeps row count
    and PK lookup correctness
  - UPDATE changing a UNIQUE secondary key rewrites only that index
  - UPDATE changing a partial-index predicate membership updates that index
  - fallback row growth path still keeps all indexes correct
  - rollback restores old row bytes for stable-RID updates
  - savepoint rollback restores old row bytes for stable-RID updates

- recovery:
  - crash after uncommitted `UpdateInPlace` is undone on reopen
  - committed `UpdateInPlace` survives reopen

- bench:
  - `python3 benches/comparison/local_bench.py --scenario update --rows 50000 --table`
  - target: materially reduce the 12× gap vs MariaDB on the benchmark schema

## Anti-patterns to avoid

- Do not implement the current tracker text literally by skipping indexes solely
  from SET column names while the `RecordId` still changes.
- Do not mix planner work into this subphase; it obscures the real write-path
  bottleneck.
- Do not reuse delete+insert undo semantics for same-slot rewrites.
- Do not update unaffected indexes "just in case" on stable-RID rows; that
  would erase the point of the subphase.
- Do not silently change affected-row semantics for no-op updates here.

## Risks

- Risk: same-slot rewrite breaks rollback or crash recovery.
  Mitigation: dedicated `UpdateInPlace` WAL + undo + recovery tests.

- Risk: partial-index membership changes are missed.
  Mitigation: compute predicate dependency masks and compare predicate result on
  old vs new rows.

- Risk: selective skipping is accidentally used on fallback rows.
  Mitigation: make the fallback path treat all indexes as affected by contract.

- Risk: page-local batching is implemented but still rereads the same page for
  each row.
  Mitigation: group updates by `page_id` inside `HeapChain`.

- Risk: this Phase 5 optimization becomes confused with Phase 7 MVCC guarantees.
  Mitigation: keep the spec explicit that this is not a HOT chain and defer full
  snapshot-safe stable row identities.

## Assumptions

- The current benchmark pain is dominated by write-path cost, not candidate
  discovery.
- Phase 5 may use the current single-writer / no-concurrent-reader assumption
  for this optimization.
- If the benchmark remains far behind after this change, the next subphase is
  batched new-key insert for affected indexes, not another planner change.
