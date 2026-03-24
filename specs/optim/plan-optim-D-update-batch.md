# Plan: UPDATE Batch Optimization (optim-D)

## Files to create/modify

| File | Action | What changes |
|---|---|---|
| `crates/axiomdb-sql/src/table.rs` | modify | Add `update_rows_batch()` function |
| `crates/axiomdb-sql/src/executor.rs` | modify | Refactor `execute_update_ctx` and `execute_update` to collect-then-dispatch |

No new files. No new crate dependencies. No changes to `heap_chain.rs`, WAL,
storage, or catalog layers.

---

## Algorithm / Data structure

### `update_rows_batch` in `table.rs`

```
fn update_rows_batch(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &TableDef,
    columns: &[ColumnDef],
    updates: Vec<(RecordId, Vec<Value>)>,
) -> Result<u64, DbError>

  if updates.is_empty():
      return Ok(0)

  // Split the input into two parallel vecs
  let mut rids:       Vec<RecordId>    = Vec::with_capacity(updates.len())
  let mut new_values: Vec<Vec<Value>>  = Vec::with_capacity(updates.len())
  for (rid, values) in updates:
      rids.push(rid)
      new_values.push(values)

  // Phase 1: batch-delete all old slots
  // Each heap page is read once, all matching slots stamped dead, written once.
  // WAL delete entries emitted by delete_rows_batch after the page writes.
  let deleted = delete_rows_batch(storage, txn, table_def, &rids)?
  // deleted == rids.len() on success (AlreadyDeleted errors fail fast)

  // Phase 2: batch-insert all new rows
  // All rows encoded before any heap write (fail-fast on coercion errors).
  // Each heap page loaded once, rows packed in, written once.
  // WAL PageWrite entries emitted by insert_rows_batch after page writes.
  let _new_rids = insert_rows_batch(storage, txn, table_def, columns, &new_values)?

  Ok(deleted)
```

The returned `u64` is `deleted` (which equals `updates.len()` on success)
because `insert_rows_batch` always inserts the same number of rows as were
deleted, and partial failure is indicated by an early `?` propagation.

### `execute_update_ctx` refactor in `executor.rs`

Replace the inline per-row update inside the scan loop with a collect-then-dispatch pattern:

```
// Existing: scan produces rows (unchanged)
let snap = txn.active_snapshot()?
let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?

// NEW: collect all matching (rid, new_values) pairs first
let mut pending: Vec<(RecordId, Vec<Value>)> = Vec::new()
for (rid, current_values) in rows:
    if let Some(ref wc) = stmt.where_clause:
        if !is_truthy(&eval(wc, &current_values)?):
            continue
    let mut new_values = current_values.clone()
    for (col_pos, val_expr) in &assignments:
        new_values[*col_pos] = eval(val_expr, &current_values)?
    pending.push((rid, new_values))

// NEW: dispatch based on count
let count = match pending.len() {
    0 => 0u64,
    1 => {
        // Preserve single-row path: no Vec overhead for common case
        let (rid, new_values) = pending.remove(0)
        TableEngine::update_row(storage, txn, &resolved.def, &schema_cols, rid, new_values)?
        1u64
    }
    _ => TableEngine::update_rows_batch(
             storage, txn, &resolved.def, &schema_cols, pending)?,
}

return Ok(QueryResult::Affected { count, last_insert_id: None })
```

Note: `execute_update_ctx` has no secondary index maintenance today. No index
loop is added here.

### `execute_update` refactor in `executor.rs`

The non-ctx path at ~line 2830 has secondary index maintenance per-row. The
refactor separates heap batching from index maintenance:

```
// Existing scan and assignment resolution (unchanged)
let snap = txn.active_snapshot()?
let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?

// NEW: collect all matching pairs AND per-row current_values for index use
struct MatchedRow {
    rid:            RecordId,
    current_values: Vec<Value>,
    new_values:     Vec<Value>,
}
let mut matched: Vec<MatchedRow> = Vec::new()
for (rid, current_values) in rows:
    if let Some(ref wc) = stmt.where_clause:
        if !is_truthy(&eval(wc, &current_values)?):
            continue
    let mut new_values = current_values.clone()
    for (col_pos, val_expr) in &assignments:
        new_values[*col_pos] = eval(val_expr, &current_values)?
    matched.push(MatchedRow { rid, current_values, new_values })

// NEW: dispatch heap operation (batch or single)
let heap_updates: Vec<(RecordId, Vec<Value>)> = matched
    .iter()
    .map(|m| (m.rid, m.new_values.clone()))
    .collect()

let count = match heap_updates.len() {
    0 => 0u64,
    1 => {
        let (rid, new_values) = heap_updates.into_iter().next().unwrap()
        // SAFETY: len == 1, next() always succeeds
        TableEngine::update_row(storage, txn, &resolved.def, &schema_cols, rid, new_values)?
        1u64
    }
    n => {
        TableEngine::update_rows_batch(
            storage, txn, &resolved.def, &schema_cols, heap_updates)?;
        n as u64
    }
}

// Existing: per-row secondary index maintenance (unchanged, now iterates matched)
// The new_rid from update_rows_batch is not tracked per-row; however, secondary
// index entries point to RIDs. Because updated rows are appended to the chain
// end, their new RIDs are returned by insert_rows_batch internally but not
// surfaced here. To preserve correctness, index maintenance in execute_update
// must either:
//   (a) call update_rows_batch returning new RIDs (requires signature change), OR
//   (b) keep per-row update_row for tables with secondary indexes.
//
// For this phase: if secondary_indexes is non-empty, fall back to the existing
// per-row loop. Batch path used only when secondary_indexes.is_empty().
// See Anti-patterns and Risks sections.
```

The conditional fallback logic for secondary indexes is detailed in the
Implementation phases section below.

---

## Implementation phases (ordered)

### Phase 1 — Add `update_rows_batch` to `table.rs`

Location: after the `delete_rows_batch` function (~line 368).

Steps:
1. Add the function with the signature from the spec.
2. Early-return `Ok(0)` for empty input.
3. Split `updates` into `rids` and `new_values` vecs using `unzip()` or
   manual iteration (manual is clearer).
4. Call `Self::delete_rows_batch(storage, txn, table_def, &rids)?`.
5. Call `Self::insert_rows_batch(storage, txn, table_def, columns, &new_values)?`.
6. Return `Ok(deleted_count)`.

Verify: `cargo test -p axiomdb-sql` passes. No change to executor yet.

### Phase 2 — Refactor `execute_update_ctx` in `executor.rs`

Location: `execute_update_ctx` function (~line 822).

Steps:
1. Replace the `let mut count = 0u64; for (rid, ...) { update_row(...) }` block
   with the collect-then-dispatch pattern from the algorithm section.
2. Keep `let mut count = 0u64` removed; derive count from `pending.len()` before
   dispatch.
3. The `N == 1` branch calls `update_row()` directly and returns 1.
4. The `N > 1` branch calls `update_rows_batch()`.
5. The `N == 0` branch returns `Ok(QueryResult::Affected { count: 0, ... })`.

Verify: `cargo test -p axiomdb-sql` passes.

### Phase 3 — Refactor `execute_update` in `executor.rs`

Location: `execute_update` function (~line 2830).

Steps:
1. Replace the per-row loop body (lines ~2870–2921) with the collect-then-dispatch
   pattern.
2. For tables **without** secondary indexes (`secondary_indexes.is_empty()`):
   - Use `update_rows_batch()` for N > 1.
   - Use `update_row()` for N == 1.
3. For tables **with** secondary indexes:
   - Keep the existing per-row loop (`update_row` + index maintenance) to avoid
     losing the `new_rid` needed for `insert_into_indexes`.
   - The collection step is still useful to pre-evaluate all expressions before
     any mutations (avoids reading stale rows after partial deletions in the
     future); iterate `matched` instead of `rows` for the per-row index loop.
4. The count is derived from `matched.len()` for the no-index path, or from the
   loop counter for the index path.

Verify: `cargo test --workspace` passes. Run the UPDATE benchmark.

### Phase 4 — Tests

Add tests in `crates/axiomdb-sql/tests/` (integration) and in `table.rs`
`#[cfg(test)]` module (unit). See Tests section below.

### Phase 5 — Benchmark

Run `cargo bench --bench update_bench` (or equivalent) before and after the
change. Report the comparison table per the mandatory benchmark protocol.

---

## Tests to write

### Unit tests — `crates/axiomdb-sql/src/table.rs` `#[cfg(test)]`

**`test_update_rows_batch_empty`**
- Setup: open an in-memory engine, create table `t(id INTEGER, v INTEGER)`, begin txn.
- Call `update_rows_batch(storage, txn, &def, &cols, vec![])`.
- Assert: `Ok(0)` returned; `scan_table` still returns 0 rows (table empty).

**`test_update_rows_batch_single_row`**
- Setup: insert 1 row `(1, 10)`, begin txn.
- Call `update_rows_batch` with 1 pair: `(rid_of_row_1, vec![Value::Int(1), Value::Int(99)])`.
- Assert: returns `Ok(1)`; scan returns 1 row `(1, 99)`.

**`test_update_rows_batch_n_rows`**
- Setup: insert 20 rows `(i, i*10)` for i in 1..=20, begin txn.
- Scan to get all 20 RIDs; build updates `(rid_i, vec![Value::Int(i), Value::Int(i*10 + 1)])`.
- Call `update_rows_batch` with all 20 pairs.
- Assert: returns `Ok(20)`; scan returns exactly 20 rows; for each row, `v == i*10 + 1`.
- Assert: no original rows remain (no row with any original `v` value).

**`test_update_rows_batch_wrong_column_count`**
- Setup: insert 1 row into `t(id INTEGER, v INTEGER)`.
- Call `update_rows_batch` with `new_values` having 3 columns instead of 2.
- Assert: `Err(DbError::TypeMismatch { .. })`.

**`test_update_rows_batch_already_deleted`**
- Setup: insert 1 row, scan its RID, manually call `delete_row` on it.
- Call `update_rows_batch` with that RID.
- Assert: `Err(DbError::AlreadyDeleted { .. })`.

### Integration tests — `tests/` directory (or existing integration test file)

**`test_update_batch_executor_n_rows`**
- Insert 1 000 rows `(i, i)` into `t(id INTEGER, val INTEGER)` via `execute`.
- Execute `UPDATE t SET val = val + 1` (no WHERE — all rows match, N == 1000 > 1).
- Execute `SELECT val FROM t ORDER BY id`.
- Assert: exactly 1 000 rows returned; for each row at position `i`, `val == i + 1`.

**`test_update_batch_executor_single_row_path`**
- Insert 10 rows.
- Execute `UPDATE t SET val = 999 WHERE id = 5` (exactly 1 match).
- Execute `SELECT val FROM t WHERE id = 5`.
- Assert: 1 row with `val == 999`; all other rows unchanged.

**`test_update_batch_executor_no_match`**
- Insert 5 rows with id 1..=5.
- Execute `UPDATE t SET val = 0 WHERE id = 999`.
- Assert: `QueryResult::Affected { count: 0 }`; scan returns original values.

**`test_update_batch_crash_recovery`**
- Insert 100 rows `(i, i)`.
- Begin an explicit transaction; execute `UPDATE t SET val = val + 1` (no commit).
- Drop the engine without committing (simulate crash).
- Reopen the engine at the same path.
- Execute `SELECT val FROM t ORDER BY id`.
- Assert: all 100 rows have original values `val == i` (undo pass recovered all
  N delete + N insert WAL entries).

### Benchmark

Location: `benches/update_bench.rs` (create if it does not exist).

**`bench_update_5000_rows`**
- Setup: 5 000-row table `(id INTEGER, score INTEGER)`.
- Measure: `UPDATE scores SET score = score + 1 WHERE active = TRUE` (all rows match).
- Report: ops/s and total latency. Compare before/after batch optimization.

---

## Anti-patterns to avoid

- **DO NOT skip the N == 1 check.** `update_rows_batch` with a 1-element vec
  works correctly but allocates a `Vec` and calls through both batch functions.
  For single-row updates (the common interactive case), `update_row()` is faster
  and produces cleaner WAL entries.

- **DO NOT apply a column mask (lazy decode) to the UPDATE scan.** `update_rows_batch`
  must receive the full `Vec<Value>` for every column in order to call
  `insert_rows_batch`, which encodes complete rows. Passing a partial column
  mask would produce incorrectly encoded rows with missing fields.

- **DO NOT change the WAL entry format.** `delete_rows_batch` and
  `insert_rows_batch` each emit their existing entry types. No new WAL entry
  type is needed or permitted in this optimization.

- **DO NOT attempt HOT (same-page) update.** Updated rows are always appended
  to the end of the chain by `insert_rows_batch`. Scan results after an UPDATE
  will return updated rows at the end of the chain, not at their original
  positions. Tests must account for this: use `ORDER BY id` or sort results
  before asserting values.

- **DO NOT call `update_rows_batch` when secondary indexes are present** (in
  `execute_update`). The per-row path in `execute_update` uses the `new_rid`
  returned by `update_row` to maintain secondary index entries
  (`insert_into_indexes(new_rid, ...)`). `update_rows_batch` does not expose
  per-row new RIDs to the executor. Until secondary index batch maintenance is
  implemented (future phase), the per-row path must be preserved for indexed
  tables.

- **DO NOT use `Vec::remove(0)` inside a hot loop.** The N == 1 dispatch branch
  uses `pending.remove(0)` (or `pending.into_iter().next().unwrap()`) exactly
  once, not in a loop. This is O(1) (single element) and is acceptable.

- **DO NOT use `unwrap()` in production code paths.** The only `unwrap()`-like
  call in the plan (`pending.into_iter().next().unwrap()` in the N==1 branch)
  is guarded by `match pending.len() { 1 => ... }` so it cannot panic at
  runtime; nonetheless, prefer `expect("len == 1 guaranteed by match arm")` or
  restructure to avoid `unwrap` entirely (e.g., `if let Some((rid, vals)) =
  heap_updates.into_iter().next() { ... }`).

---

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| Updated rows appended to chain end change scan order | Certain | Use `ORDER BY` in all verification SELECTs; document in tests with a comment explaining this is correct MVCC behavior |
| `execute_update` with secondary indexes falls back to per-row path | Certain (by design) | The fallback is intentional and documented; table with no secondary indexes gets full batch speedup; add a test for a table with a secondary index to confirm fallback works correctly |
| `delete_rows_batch` partial failure (AlreadyDeleted on row K) leaves K-1 rows deleted with no inserts | Low (double-delete is a bug elsewhere) | Unit test `test_update_rows_batch_already_deleted` covers this; transaction rollback via WAL undo recovers the deleted rows |
| Memory: collecting all matching rows before any write requires O(N) Vec<Value> in RAM | Low for typical workloads; possible for bulk UPDATE of millions of rows | Acceptable for this phase; streaming batch UPDATE (bounded buffer) is a future optimization. Document the trade-off in the spec's Out of scope. |
| `insert_rows_batch` coercion failure after `delete_rows_batch` completes leaves inconsistent state | Low (coercion errors are programming errors, not runtime surprises) | The transaction must be rolled back; the WAL undo pass recovers the deleted rows on crash. No special handling needed in this phase. |
| Benchmark does not show improvement because test table fits in OS page cache | Low (cached pages still benefit from fewer page-level operations) | Measure wall-clock time, not just I/O ops. Even with a warm cache, reducing `read_page`/`write_page` call count reduces function call overhead and lock contention. |
