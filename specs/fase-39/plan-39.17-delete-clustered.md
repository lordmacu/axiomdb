# Plan: 39.17 — Executor Integration: DELETE from Clustered Table

## Files to modify

| File | Change |
|---|---|
| `crates/axiomdb-sql/src/executor/delete.rs` | Remove guard, add `execute_clustered_delete()` function |
| `crates/axiomdb-sql/tests/integration_clustered_delete.rs` | **NEW**: 6+ integration tests |

## Implementation phases

### Phase 1: Remove ensure_heap_runtime guard
Remove the guard in `execute_delete_ctx()` and the non-ctx `execute_delete()`.
Add `if resolved.def.is_clustered() { return execute_clustered_delete(...); }` dispatch.

### Phase 2: Implement execute_clustered_delete

```rust
fn execute_clustered_delete(
    where_clause: Option<Expr>,
    schema_cols: &[ColumnDef],
    secondary_indexes: &[IndexDef],
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    snap: TransactionSnapshot,
    resolved: &ResolvedTable,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // 1. Collect candidates from clustered scan
    let iter = clustered_tree::range(storage, Some(root_pid), Unbounded, Unbounded, &snap)?;
    let mut candidates = Vec::new();
    for row in iter {
        let values = decode_row(&row.row_data, &col_types)?;
        if let Some(ref wc) = where_clause {
            if !is_truthy(&eval(wc, &values)?) { continue; }
        }
        candidates.push((row.key, values));
    }

    // 2. FK parent enforcement (before any deletes)
    // ... check child tables for references ...

    // 3. Delete-mark each candidate
    for (pk_key, old_values) in &candidates {
        clustered_tree::delete_mark(storage, Some(root_pid), pk_key, txn_id, &snap)?;
        // WAL
        txn.record_clustered_delete_mark(table_id, pk_key, &old_image, &new_image)?;
    }

    // 4. Secondary index entries left in place (MVCC deferred cleanup)

    Ok(QueryResult::Affected { count: candidates.len(), last_insert_id: None })
}
```

### Phase 3: Non-ctx path guard
For `execute_delete()` (non-ctx), add NotImplemented for clustered
(same pattern as UPDATE — ctx path required for session state).

### Phase 4: Integration tests

1. **DELETE with PK WHERE**: `DELETE FROM users WHERE id = 3`
2. **DELETE all rows**: `DELETE FROM users`
3. **DELETE with non-PK WHERE**: `DELETE FROM users WHERE age > 30`
4. **Verify MVCC**: deleted row invisible to new snapshot
5. **Rollback**: BEGIN → DELETE → ROLLBACK → row restored
6. **DELETE from empty table**: 0 affected
7. **Count after DELETE**: INSERT 5 → DELETE 2 → SELECT COUNT(*) = 3

## Anti-patterns to avoid

- DO NOT physically remove cells from clustered leaf (MVCC violation)
- DO NOT modify secondary index entries during DELETE (deferred to VACUUM)
- DO NOT skip FK enforcement — check before first delete-mark
- DO NOT call heap delete functions (HeapChain::delete_batch) on clustered tables

## Risks

- **Rollback**: Same known issue as 39.16 — root_pid tracking in undo may not
  reconstruct correctly after ROLLBACK. Mark rollback test as `#[ignore]` if needed.
- **Accumulation**: Without VACUUM (39.18), dead cells accumulate. Acceptable for
  correctness; performance degrades over time until VACUUM is implemented.
