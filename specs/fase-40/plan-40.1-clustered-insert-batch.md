# Plan: 40.1 — Clustered Insert Batch

## Files to create / modify

| File | Action | What changes |
|---|---|---|
| `crates/axiomdb-sql/src/session.rs` | Modify | Add `ClusteredInsertBatch` struct + `StagedClusteredRow`; add `clustered_insert_batch: Option<ClusteredInsertBatch>` to `SessionContext` |
| `crates/axiomdb-sql/src/executor/insert.rs` | Modify | Reroute clustered explicit-txn INSERTs to batch; implement `enqueue_clustered_row`, `flush_clustered_insert_batch`, `discard_clustered_insert_batch` |
| `crates/axiomdb-sql/src/executor/mod.rs` | Modify | Add barrier flush calls before SELECT/UPDATE/DELETE/DDL dispatch |
| `crates/axiomdb-sql/src/executor/update.rs` | Modify | Add flush of clustered batch at entry if same table |
| `crates/axiomdb-sql/src/executor/delete.rs` | Modify | Same as update.rs |
| `crates/axiomdb-sql/src/executor/select.rs` | Modify | Same — flush before clustered scan |
| `crates/axiomdb-sql/src/executor/ddl.rs` | Modify | Flush all pending clustered batches before DDL |
| `crates/axiomdb-wal/src/txn.rs` | Modify | Call `flush_clustered_insert_batch` before COMMIT; call `discard_clustered_insert_batch` in ROLLBACK |
| `crates/axiomdb-sql/tests/integration_clustered_insert_batch.rs` | Create | Integration tests for the batch path |
| `tools/wire-test.py` | Modify | Add ≥ 5 assertions for batch insert scenarios |

---

## Algorithm — detailed

### Phase 1 — Data structures (session.rs)

```rust
/// Pre-encoded row ready for bulk insertion into a clustered B-tree leaf.
#[derive(Debug)]
pub struct StagedClusteredRow {
    pub pk_key: Vec<u8>,
    pub header: axiomdb_storage::RowHeader,
    pub row_data: Vec<u8>,
    /// True when row_data would require an overflow chain.
    /// These rows skip the batch and are inserted immediately.
    pub has_overflow: bool,
}

/// Transaction-local staging buffer for consecutive INSERT ... VALUES into
/// the same clustered table during an explicit transaction.
///
/// Rows are enqueued here (pre-encoded) instead of being written to the
/// clustered B-tree immediately. The buffer is flushed (B-tree write + WAL)
/// before any barrier statement or at COMMIT.
/// On ROLLBACK the buffer is discarded without touching storage.
#[derive(Debug)]
pub struct ClusteredInsertBatch {
    pub table_id: u32,
    pub table_def: TableDef,
    pub columns: Vec<ColumnDef>,
    pub indexes: Vec<IndexDef>,
    pub compiled_preds: Vec<Option<Expr>>,
    /// Pre-encoded rows. NOT guaranteed to be PK-sorted; sorted at flush time.
    pub rows: Vec<StagedClusteredRow>,
    /// PK bytes of staged rows, for O(1) intra-batch duplicate detection.
    pub staged_pks: HashSet<Vec<u8>>,
    /// Index IDs whose committed BTree root was empty at batch creation.
    /// Used to skip BTree::lookup_in for committed-empty unique indexes.
    pub committed_empty: HashSet<u32>,
}

impl ClusteredInsertBatch {
    pub const MAX_ROWS: usize = 200_000;
}
```

Add to `SessionContext`:
```rust
pub clustered_insert_batch: Option<ClusteredInsertBatch>,

// And two new methods:
pub fn discard_clustered_insert_batch(&mut self) {
    self.clustered_insert_batch = None;
}
// flush is done in insert.rs (needs storage/txn/bloom)
```

---

### Phase 2 — Enqueue path (insert.rs)

**Replace** the current early-return for clustered INSERT in explicit txn:

```rust
// CURRENT (bypasses batch entirely):
if resolved.def.is_clustered() {
    if ctx.pending_inserts.is_some() {
        flush_pending_inserts_ctx(storage, txn, bloom, ctx)?;
    }
    return execute_clustered_insert_ctx(stmt, storage, txn, bloom, ctx, resolved);
}
```

**NEW** for the `InsertSource::Values(rows) if ctx.in_explicit_txn` branch, when table is clustered:

```rust
if resolved.def.is_clustered() && ctx.in_explicit_txn {
    // Flush heap batch if targeting a different table
    if ctx.pending_inserts.is_some() {
        flush_pending_inserts_ctx(storage, txn, bloom, ctx)?;
    }
    // Flush clustered batch if it's for a different table
    if ctx
        .clustered_insert_batch
        .as_ref()
        .map(|b| b.table_id != resolved.def.id)
        .unwrap_or(false)
    {
        flush_clustered_insert_batch(storage, txn, bloom, ctx)?;
    }
    // Initialise batch if needed
    if ctx.clustered_insert_batch.is_none() {
        let committed_empty =
            detect_committed_empty_unique_indexes(storage, &secondary_indexes)?;
        ctx.clustered_insert_batch = Some(ClusteredInsertBatch {
            table_id: resolved.def.id,
            table_def: resolved.def.clone(),
            columns: resolved.columns.clone(),
            indexes: secondary_indexes.clone(),
            compiled_preds: compiled_preds.clone(),
            rows: Vec::new(),
            staged_pks: HashSet::new(),
            committed_empty,
        });
    }
    // Enqueue each row
    for value_exprs in rows {
        enqueue_clustered_row(
            value_exprs,
            &resolved,
            &col_positions,
            auto_inc_col,
            &mut first_generated,
            storage,
            txn,
            bloom,
            ctx,
        )?;
    }
    // Overflow rows are inserted immediately inside enqueue_clustered_row
    // Safety valve: flush if batch exceeds MAX_ROWS
    if ctx
        .clustered_insert_batch
        .as_ref()
        .map(|b| b.rows.len() >= ClusteredInsertBatch::MAX_ROWS)
        .unwrap_or(false)
    {
        flush_clustered_insert_batch(storage, txn, bloom, ctx)?;
    }
    return Ok(QueryResult::Affected {
        count: ...,  // rows enqueued this call
        last_insert_id: first_generated,
    });
}
// Fall through to existing path for autocommit clustered inserts
```

**`enqueue_clustered_row` function:**

```
fn enqueue_clustered_row(value_exprs, resolved, col_positions, ...) {
    1. Evaluate expressions → Vec<Value>
    2. Fill col_positions → full_values
    3. Handle AUTO_INCREMENT column
    4. Evaluate CHECK constraints
    5. Check FK child references (existing fk_enforcement::check_fk_child_insert)
    6. Encode PK key bytes via row codec
    7. Check staged_pks (intra-batch duplicate): if found → DuplicateKey, discard batch
    8. Check committed B-tree: clustered_tree::lookup_physical(storage, root, &pk_key)
       → if found → DuplicateKey, discard batch
    9. Check UNIQUE secondary indexes (same pattern as heap batch, using committed_empty)
   10. Encode RowHeader + row_data
   11. Determine has_overflow (row_data > INLINE_THRESHOLD)
   12. If has_overflow:
         → insert immediately via execute_clustered_insert_ctx (bypasses batch)
         → return
   13. Push StagedClusteredRow into batch.rows
   14. Insert pk_key bytes into staged_pks
}
```

---

### Phase 3 — Flush path (insert.rs)

```
fn flush_clustered_insert_batch(storage, txn, bloom, ctx) → Result<(), DbError> {
    let Some(batch) = ctx.clustered_insert_batch.take() else { return Ok(()); };
    if batch.rows.is_empty() { return Ok(()); }

    // 1. Sort by pk_key ascending
    let mut rows = batch.rows;
    rows.sort_unstable_by(|a, b| a.pk_key.cmp(&b.pk_key));

    // 2. Get current rightmost leaf hint
    let root_pid = // fetch from catalog / WAL clustered roots
    let hinted_pid = // last known rightmost leaf or 0

    // 3. Try rightmost batch for the LONGEST prefix of rows that are
    //    strictly > hinted_leaf's last key.
    //    Use try_insert_rightmost_leaf_batch in a loop until all rows processed.
    let mut cursor = 0usize;
    while cursor < rows.len() {
        let remaining = &rows[cursor..];

        // Build RightmostAppendRow slice
        let append_rows: Vec<RightmostAppendRow> = remaining
            .iter()
            .filter(|r| !r.has_overflow)
            .map(|r| RightmostAppendRow {
                key: &r.pk_key,
                row_header: &r.header,
                row_data: &r.row_data,
            })
            .collect();

        if append_rows.is_empty() { break; }

        let n = if hinted_pid != 0 {
            try_insert_rightmost_leaf_batch(storage, hinted_pid, &append_rows)?
        } else {
            0
        };

        if n == 0 {
            // Single normal insert to find correct leaf position
            let row = &rows[cursor];
            apply_one_clustered_row(storage, txn, bloom, &batch.table_def, &batch.columns,
                                    &batch.indexes, &batch.compiled_preds, row)?;
            // WAL + undo recorded inside apply_one_clustered_row
            cursor += 1;
            hinted_pid = // updated rightmost leaf after insert
        } else {
            // Record WAL entries for the n rows inserted
            for row in &rows[cursor..cursor+n] {
                let row_image = build_row_image_for_wal(&row);
                txn.record_clustered_insert(batch.table_id, root_pid, &row.pk_key, row_image)?;
                txn.push_undo(UndoOp::UndoClusteredInsert {
                    table_id: batch.table_id,
                    key: row.pk_key.clone(),
                });
            }
            cursor += n;
            // Update hinted_pid: read the new rightmost leaf from storage
            hinted_pid = updated_rightmost_leaf_pid;
        }
    }

    // 4. Maintain secondary indexes for all rows in sorted order
    maintain_clustered_secondary_inserts_batch(
        storage, txn, bloom, ctx, &batch, &rows
    )?;

    Ok(())
}
```

**`apply_one_clustered_row`** — wraps existing `apply_clustered_insert_rows` for a single pre-encoded row (non-rightmost / fallback). Internally calls `clustered_tree::insert()`.

---

### Phase 4 — Barrier flush in dispatcher (executor/mod.rs + per-executor)

In `execute_with_ctx` or each executor entry point:

```rust
// Before SELECT execution:
fn execute_select_ctx(...) {
    flush_clustered_insert_batch_if_table(storage, txn, bloom, ctx, target_table_id)?;
    // ... existing select logic ...
}

// Before UPDATE execution:
fn execute_update_ctx(...) {
    flush_clustered_insert_batch_if_table(storage, txn, bloom, ctx, target_table_id)?;
    // ...
}

// Before DELETE execution:
fn execute_delete_ctx(...) {
    flush_clustered_insert_batch_if_table(storage, txn, bloom, ctx, target_table_id)?;
    // ...
}

// Before any DDL:
fn execute_ddl_ctx(...) {
    if ctx.clustered_insert_batch.is_some() {
        flush_clustered_insert_batch(storage, txn, bloom, ctx)?;
    }
    // ...
}
```

Helper:
```rust
fn flush_clustered_insert_batch_if_table(
    storage, txn, bloom, ctx, table_id: u32
) -> Result<(), DbError> {
    if ctx
        .clustered_insert_batch
        .as_ref()
        .map(|b| b.table_id == table_id)
        .unwrap_or(false)
    {
        flush_clustered_insert_batch(storage, txn, bloom, ctx)?;
    }
    Ok(())
}
```

---

### Phase 5 — COMMIT / ROLLBACK hooks (txn.rs)

The COMMIT / ROLLBACK calls go through `SessionContext`-aware code in
`axiomdb-network` or `axiomdb-sql`. Add flush/discard there:

```rust
// In the COMMIT execution path (executor or network layer):
flush_clustered_insert_batch(storage, txn, bloom, ctx)?;  // writes + WAL
// then: txn.commit() as usual → WAL fsync

// In the ROLLBACK execution path:
ctx.discard_clustered_insert_batch();  // no storage write
// then: txn.rollback() as usual

// In the SAVEPOINT creation path:
flush_clustered_insert_batch(storage, txn, bloom, ctx)?;  // flush before savepoint marker
// then: txn.create_savepoint(name) as usual
```

---

### Phase 6 — Secondary index maintenance

`maintain_clustered_secondary_inserts_batch` inserts secondary index entries for
all flushed rows in one go:

```rust
fn maintain_clustered_secondary_inserts_batch(
    storage, txn, bloom, ctx, batch: &ClusteredInsertBatch, rows: &[StagedClusteredRow]
) -> Result<(), DbError> {
    for idx in &batch.indexes {
        if idx.columns.is_empty() { continue; }
        let mut keys_to_insert: Vec<(Vec<u8>, RecordId)> = Vec::with_capacity(rows.len());
        for row in rows {
            // Decode only the columns needed for this index from row.row_data
            let index_key = extract_index_key_from_row_data(
                &row.row_data, &row.pk_key, idx, &batch.columns
            )?;
            // Check partial-index predicate
            if let Some(pred) = &batch.compiled_preds[idx_pos] {
                if !eval_predicate(pred, &decoded_values) { continue; }
            }
            keys_to_insert.push((index_key, RecordId::clustered_bookmark(&row.pk_key)));
        }
        // Bulk insert into secondary index BTree (sorted for cache efficiency)
        keys_to_insert.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (key, rid) in keys_to_insert {
            BTree::insert_in(storage, &idx_root, &key, rid, FILL_FACTOR)?;
        }
    }
    Ok(())
}
```

---

## Implementation phases

1. **Data structures** — `StagedClusteredRow` + `ClusteredInsertBatch` + new field in `SessionContext`. Compile but no behavior yet.

2. **Enqueue path** — `enqueue_clustered_row` function + routing in `execute_insert_ctx`. Should make explicit-txn clustered INSERT enqueue rows (no flush yet → rows lost on commit). Tests will fail at this point.

3. **Flush path** — `flush_clustered_insert_batch` with the loop over `try_insert_rightmost_leaf_batch` + fallback. Add secondary index maintenance. Wire into COMMIT path.

4. **Discard path** — ROLLBACK calls `discard_clustered_insert_batch`. Add to all rollback code paths.

5. **Barrier flush** — Add `flush_clustered_insert_batch_if_table` to SELECT / UPDATE / DELETE / DDL executors and SAVEPOINT.

6. **Integration tests** — Write `integration_clustered_insert_batch.rs` covering all use cases.

7. **Wire test** — Update `tools/wire-test.py`.

8. **Benchmarks** — Run local bench + Criterion, report results.

---

## Tests to write

### Unit tests (insert.rs or session.rs)
- `batch_enqueue_then_discard` — batch created, ROLLBACK → 0 rows in table
- `batch_pk_duplicate_within_batch` — second insert with same PK → error immediately
- `batch_pk_duplicate_committed` — committed row exists → error at enqueue

### Integration tests (tests/integration_clustered_insert_batch.rs)
- `clustered_batch_sequential_commit` — 1000 sequential PKs, COMMIT, SELECT COUNT = 1000
- `clustered_batch_rollback_leaves_empty` — 500 inserts, ROLLBACK, SELECT COUNT = 0
- `clustered_batch_select_barrier` — 2 inserts, SELECT (sees both), 1 more insert, COMMIT, COUNT = 3
- `clustered_batch_savepoint_rollback` — 2 inserts, SAVEPOINT, 2 more, ROLLBACK TO SAVEPOINT, 2 more, COMMIT, COUNT = 4 (not 6)
- `clustered_batch_secondary_indexes` — bulk insert, SELECT on indexed column returns correct rows
- `clustered_batch_non_monotonic_pk` — random PK order, COMMIT, all rows present
- `clustered_batch_large` — 10K rows, COMMIT, COUNT = 10K, spot-check values
- `clustered_batch_table_switch` — insert 100 to table A, insert 100 to table B, COMMIT → both have 100 rows
- `clustered_batch_overflow_bypass` — row with large TEXT field inserted immediately, rest batched
- `clustered_batch_autocommit_unchanged` — autocommit mode: inserts go direct (no batch regression)

### Benchmarks
- Run `cargo bench --bench executor_e2e -p axiomdb-sql -- clustered_update` (already exists)
- Run `cargo bench --bench executor_e2e -p axiomdb-sql -- "insert_sequential"` for baseline
- Add `bench_clustered_insert_batch` to executor_e2e.rs targeting explicit-txn bulk insert
- Run local bench: `python3 benches/comparison/local_bench.py --scenario insert --rows 50000 --table`

---

## Anti-patterns to avoid

- **DO NOT** flush the batch on every single-row enqueue — defeats the purpose.
- **DO NOT** sort rows at enqueue time (O(N log N) amortized) — sort once at flush.
- **DO NOT** re-read `resolve_table_cached` inside flush — batch already stores `table_def + columns + indexes`.
- **DO NOT** call `apply_clustered_insert_rows` on ALL rows at flush — use `try_insert_rightmost_leaf_batch` for the rightmost prefix; only fall back to individual inserts for non-monotonic rows.
- **DO NOT** forget to flush the batch when the connection drops or errors — ensure discard is called in all error paths.
- **DO NOT** write WAL entries at enqueue time — write them at flush time (rows are not yet in storage).
- **DO NOT** skip `flush_clustered_insert_batch_if_table` for UPDATE on a table that has staged inserts — reads inside the UPDATE executor need to see committed data.

---

## Risks

| Risk | Mitigation |
|---|---|
| Non-monotonic PKs degrade to individual inserts | `try_insert_rightmost_leaf_batch` returns 0 → fallback to `apply_clustered_insert_rows` — correct, just no batch speedup |
| Savepoint + batch interaction | Flush at SAVEPOINT creation; undo via existing WAL undo path |
| Memory for very large batches | `MAX_ROWS = 200_000` hard cap; flush and re-init |
| Missed flush barrier in some executor path | Systematic review: grep for `execute_*_ctx` and verify each one calls `flush_clustered_insert_batch_if_table` |
| Secondary index key extraction from pre-encoded row_data | Implement `extract_index_key_from_row_data` using the column offset machinery in `field_patch.rs` or decode the minimal needed columns |
