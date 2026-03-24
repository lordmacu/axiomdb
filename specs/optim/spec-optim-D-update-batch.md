# Spec: UPDATE Batch Optimization (optim-D)

## What to build (not how)

A `TableEngine::update_rows_batch()` function that replaces the per-row
`update_row()` loop in both UPDATE executor paths with a two-phase batch
operation: batch-delete all old rows, then batch-insert all new rows. This
reuses the already-proven `delete_rows_batch()` and `insert_rows_batch()`
functions, each of which amortizes page I/O across all rows in a batch so that
every heap page is read and written exactly once per phase regardless of how
many rows it contains.

The executor dispatch rule: after collecting all matching `(RecordId,
Vec<Value>)` pairs from the scan, if N == 1 the existing single-row
`update_row()` is called (avoid Vec allocation overhead for the common
single-row UPDATE); if N > 1 `update_rows_batch()` is called.

The WAL format does not change. The batch path emits the same sequence of WAL
entries that would have been produced row-by-row — N delete entries followed by
N insert entries — because `delete_rows_batch()` and `insert_rows_batch()` each
already write WAL entries internally. No new WAL entry type is introduced.

## Inputs / Outputs

### `TableEngine::update_rows_batch`

```rust
pub fn update_rows_batch(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &TableDef,
    columns: &[ColumnDef],
    updates: Vec<(RecordId, Vec<Value>)>,
) -> Result<u64, DbError>
```

- **Input `storage`**: mutable reference to the storage backend. Passed to
  `delete_rows_batch` and `insert_rows_batch`.
- **Input `txn`**: active transaction manager. An active transaction must be
  open before this call. Passed to both batch sub-functions.
- **Input `table_def`**: table definition — supplies `data_root_page_id` and
  `id` (table catalog ID) required by the heap and WAL layers.
- **Input `columns`**: column definitions for encoding and type coercion. Length
  must match `new_values.len()` for every row in `updates`.
- **Input `updates`**: `Vec<(RecordId, Vec<Value>)>` — each element is
  `(old_rid, new_values)`. `old_rid` identifies the heap slot to delete;
  `new_values` holds the full set of column values for the replacement row.
  Empty vec is a valid input (returns `Ok(0)` immediately).
- **Output**: `Ok(u64)` — the number of rows updated (equals `updates.len()` on
  success).
- **Errors**:
  - `DbError::NoActiveTransaction` — no transaction open.
  - `DbError::AlreadyDeleted { page_id, slot_id }` — any old slot is already
    dead (fails fast; prior pages in the delete phase may already be mutated).
  - `DbError::TypeMismatch` — any `new_values` has the wrong column count.
  - `DbError::InvalidCoercion` — any new value cannot be coerced to its column
    type. Detected in the encode-all-first step of `insert_rows_batch` before
    any heap writes occur for the insert phase.
  - I/O errors from storage or WAL writes.

### `execute_update_ctx` and `execute_update` changes

Both executor functions gain the same dispatch logic after the scan-and-filter
loop. No new public API is added to the executor; the change is internal.

The collect-then-dispatch pattern (scan → collect matching pairs → dispatch
based on N) replaces the inline per-row `update_row()` call inside the loop.

## Use cases

### 1. Batch UPDATE on a large table (happy path)

`UPDATE scores SET score = score + 1 WHERE active = TRUE` — 5 000 of 5 000
rows match. The scan produces 5 000 `(RecordId, new_values)` pairs. N > 1, so
`update_rows_batch()` is called.

Phase 1 (`delete_rows_batch`): the 5 000 old RIDs are grouped by page. Suppose
the table spans 50 pages with ~100 rows each. Each of the 50 pages is loaded
once, all matching slots on that page are stamped dead in a single pass, and
the page is written back once. Total: 50 reads + 50 writes.

Phase 2 (`insert_rows_batch`): 5 000 encoded rows are appended to the chain.
Each destination page is loaded once and written once. For a 16-row-per-page
capacity the chain grows by ~313 pages. Total: ~313 reads + ~313 writes.

Compare with the per-row path: 3 page ops × 5 000 rows = 15 000 ops.
Batch total: ~726 ops. Approximately 20× fewer page operations.

### 2. Single-row UPDATE (existing path preserved)

`UPDATE users SET name = 'Alice' WHERE id = 42` — exactly 1 row matches. N == 1,
so `update_row()` is called directly, bypassing the Vec collection step. No
allocation overhead, no behavioral change.

### 3. UPDATE with no WHERE clause (full-table batch)

`UPDATE products SET tax_rate = 0.21` — all rows match. Identical to use
case 1 from the executor's perspective. The batch path handles it transparently.

### 4. UPDATE that matches no rows

`UPDATE orders SET status = 'shipped' WHERE id = 99999` and no row has that ID.
The scan loop produces 0 pairs. `update_rows_batch()` returns `Ok(0)` before
touching the heap. The executor returns `QueryResult::Affected { count: 0 }`.

### 5. Crash recovery — uncommitted batch UPDATE

A batch UPDATE of N rows begins: `delete_rows_batch` writes N delete WAL entries
and marks N slots dead; `insert_rows_batch` writes WAL PageWrite entries for
the N new slots. The process crashes before `TxnManager::commit()` is called.

On restart, the WAL recovery pass replays the uncommitted transaction's entries
in reverse (undo pass). The N insert entries are undone (slots re-stamped dead),
and the N delete entries are undone (slots re-stamped live). After recovery the
table is byte-for-byte identical to its pre-UPDATE state. This is guaranteed by
the existing WAL undo machinery — `update_rows_batch` emits no new entry types
so no new recovery logic is needed.

### 6. Partial failure — coercion error on row K

`UPDATE t SET x = 'not_an_int'` where `x` is `INTEGER`. `insert_rows_batch`
encodes all rows before any heap write (fail-fast). The error is returned
before the insert phase touches the heap. However, the delete phase (Phase 1)
has already completed. The caller (the executor) does not attempt partial
rollback; transaction-level rollback is the responsibility of the caller issuing
`ROLLBACK` or the WAL undo pass on reconnect/crash. This is the same behavior
as any other multi-operation sequence within a transaction.

## Acceptance criteria

- [ ] `TableEngine::update_rows_batch` exists in `crates/axiomdb-sql/src/table.rs`
      with the exact signature above.
- [ ] `update_rows_batch` with an empty `updates` vec returns `Ok(0)` without
      touching the heap or WAL.
- [ ] `execute_update_ctx` (session path, `executor.rs` ~line 822) collects all
      matching `(RecordId, Vec<Value>)` pairs before calling any update function.
      Uses `update_row` for N == 1 and `update_rows_batch` for N > 1.
- [ ] `execute_update` (non-ctx path, `executor.rs` ~line 2830) is updated with
      the same dispatch logic for the heap path (secondary index maintenance
      loop remains per-row as before — see Out of scope).
- [ ] After `UPDATE t SET x = x + 1` on a table with 1 000 rows, `SELECT * FROM t`
      returns every row with the updated value; no row is missing, duplicated, or
      left with the old value.
- [ ] After a batch UPDATE followed by a crash (simulated by `drop(engine)` before
      commit), re-opening the engine returns all rows to their pre-UPDATE values.
- [ ] `cargo test --workspace` passes with all existing tests unchanged.
- [ ] `cargo clippy --workspace -- -D warnings` passes with no new warnings.
- [ ] No `unwrap()` added in `src/` outside `#[cfg(test)]` blocks.
- [ ] All `unsafe` blocks (if any) carry `// SAFETY:` comments.

## Out of scope

- **HOT (Heap-Only Tuple) same-page optimization**: updated rows are always
  appended to the end of the heap chain by `insert_rows_batch`, even when the
  old and new encoded sizes would fit in the same page. Same-page overwrites are
  a future optimization requiring a separate HOT phase.
- **Secondary index maintenance batching**: `execute_update` currently updates
  secondary indexes per-row after each `update_row` call. The batch path in this
  spec does not batch index maintenance; index entries are still updated per-row
  in `execute_update` (by iterating over the collected pairs after calling
  `update_rows_batch`). Batching index maintenance is a separate future
  optimization.
- **Primary key change detection**: if an UPDATE changes the value of a PK
  column, the behavior is unchanged from today (handled at the WAL/catalog
  level); no new PK-change detection is introduced in this spec.
- **Partial-failure atomicity within the batch**: if the insert phase fails after
  the delete phase completes, the transaction is left in a partially-mutated
  state and must be rolled back by the caller. Automatic subtransaction rollback
  is not part of this spec.
- **Column mask / lazy decode for the UPDATE scan**: because `update_rows_batch`
  requires the full new row (`Vec<Value>` with all columns) to encode and
  insert, the scan always passes `None` for the column mask. Lazy decode for
  UPDATE is explicitly excluded — full row decode is correct and required.
- **`execute_update_ctx` secondary index batching**: the ctx path currently has
  no secondary index maintenance (it calls only `update_row`). This remains
  unchanged.

## Dependencies

- `TableEngine::delete_rows_batch` — must exist in `table.rs` with the
  signature `(storage, txn, table_def, &[RecordId]) -> Result<u64, DbError>`.
  **Already implemented** as of optim-B/C.
- `TableEngine::insert_rows_batch` — must exist in `table.rs` with the
  signature `(storage, txn, table_def, columns, &[Vec<Value>]) ->
  Result<Vec<RecordId>, DbError>`. **Already implemented** as of Phase 4.
- `HeapChain::delete_batch` — called by `delete_rows_batch`; already exists
  with signature `(storage, root_page_id, &[(u64, u16)], txn_id) ->
  Result<Vec<(u64, u16, Vec<u8>)>, DbError>`.
- `HeapChain::insert_batch` — called by `insert_rows_batch`; already exists.
- `TxnManager` — active transaction must be open; existing `active_txn_id()`
  and `active_snapshot()` APIs are sufficient.
- No new crate dependencies are required.
