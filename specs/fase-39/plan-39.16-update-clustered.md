# Plan: 39.16 — Executor Integration: UPDATE on Clustered Table

## Files to modify

| File | Change |
|---|---|
| `crates/axiomdb-sql/src/executor/update.rs` | Remove guard, add clustered dispatch, candidate collection via clustered scan |
| `crates/axiomdb-sql/src/table.rs` | Add `update_clustered_row()` helper |
| `crates/axiomdb-sql/tests/integration_clustered_update.rs` | **NEW**: 8+ integration tests |

## Implementation phases

### Phase 1: Remove ensure_heap_runtime guard
Find and remove `ensure_heap_runtime("UPDATE on clustered table — Phase 39.16")` in update.rs.

### Phase 2: Clustered candidate collection
When `resolved.def.is_clustered()`, replace `collect_delete_candidates()` with
clustered-aware candidate collection:

```rust
if resolved.def.is_clustered() {
    match &update_access {
        AccessMethod::Scan => {
            // Full clustered scan + WHERE filter
            let iter = clustered_tree::range(storage, Some(root_pid), Unbounded, Unbounded, &snap)?;
            for row in iter {
                let values = decode_row(&row.row_data, &col_types)?;
                if matches WHERE clause → collect (row.key, values)
            }
        }
        AccessMethod::IndexLookup { index_def, key } if index_def.is_primary => {
            // PK point lookup
            if let Some(row) = clustered_tree::lookup(storage, Some(root_pid), key, &snap)? {
                let values = decode_row(&row.row_data, &col_types)?;
                collect (row.key, values)
            }
        }
        AccessMethod::IndexRange { index_def, lo, hi } if index_def.is_primary => {
            // PK range scan
            let iter = clustered_tree::range(storage, Some(root_pid), lo, hi, &snap)?;
            for row in iter → decode → collect
        }
        _ => {
            // Secondary index → decode PK bookmark → clustered lookup
            // (same as heap path but with PK-based candidate collection)
        }
    }
}
```

### Phase 3: Clustered update execution
After collecting candidates as `Vec<(Vec<u8>, Vec<Value>, Vec<Value>)>` (pk_key, old, new):

```rust
for (pk_key, old_values, new_values) in clustered_candidates {
    let pk_changed = pk_cols.iter().any(|&col| old_values[col] != new_values[col]);
    let new_row_data = encode_row(&new_values, &col_types)?;

    if pk_changed {
        // Key change: delete-mark + insert
        clustered_tree::delete_mark(storage, Some(root_pid), &pk_key, txn_id, &snap)?;
        let new_pk_key = encode_pk_key(&new_values, &pk_cols)?;
        root_pid = clustered_tree::insert(storage, Some(root_pid), &new_pk_key, &new_header, &new_row_data)?;
        // Update ALL secondary indexes
    } else {
        // Non-key: in-place or relocation
        match clustered_tree::update_in_place(storage, Some(root_pid), &pk_key, &new_row_data, txn_id, &snap) {
            Ok(true) => { /* success */ }
            Ok(false) => { /* visibility issue, skip */ }
            Err(DbError::HeapPageFull { .. }) => {
                // Relocation fallback
                root_pid = clustered_tree::update_with_relocation(
                    storage, Some(root_pid), &pk_key, &new_row_data, txn_id, &snap
                )?.unwrap_or(root_pid);
            }
            Err(e) => return Err(e),
        }
        // Update secondary indexes only if indexed columns changed
    }

    // Update catalog root if changed
    update_catalog_root_if_changed(storage, txn, &resolved.def, root_pid)?;
}
```

### Phase 4: WAL integration
Use existing `txn.record_clustered_update()` for in-place and
`txn.record_clustered_delete_mark()` + `txn.record_clustered_insert()` for key changes.

### Phase 5: Secondary index maintenance
For clustered tables, use `ClusteredSecondaryLayout::update_row(old_row, new_row)`
instead of the RecordId-based index maintenance. Only call when indexed columns changed.

### Phase 6: Catalog root persistence
After any structural change (relocation, key change insert), update the table's
root_page_id in the catalog if it changed.

## Tests to write

1. **In-place update**: `UPDATE SET name = 'NewName' WHERE id = 3` — non-PK column
2. **Update with WHERE range**: `UPDATE SET score = score + 1 WHERE id >= 2 AND id < 5`
3. **Update all rows**: `UPDATE SET age = age + 1` — full scan
4. **PK change**: `UPDATE SET id = 100 WHERE id = 3` — delete + insert
5. **Relocation on growth**: update with large value that causes page overflow
6. **Secondary index update**: update indexed column, verify secondary reflects change
7. **No-op update**: `UPDATE SET name = name` — no physical change
8. **Rollback**: BEGIN → UPDATE → ROLLBACK → verify original values
9. **MVCC**: update in open txn, SELECT from different snapshot sees old values
10. **Update after splits**: insert 200 rows, update row in middle of tree

## Anti-patterns to avoid

- DO NOT call heap-based `update_rows_preserve_rid` on clustered tables
- DO NOT use RecordId for clustered row identification — use PK key bytes
- DO NOT skip WAL entries — every clustered modification must be WAL-logged
- DO NOT forget catalog root update after relocation/key-change

## Risks

- **PK change + duplicate detection**: If `UPDATE SET id = X` and X already exists,
  must return UniqueViolation. The `insert()` call already handles this via B-tree
  duplicate detection.
- **Root change propagation**: Multiple updates in one statement may each change the root.
  Must track current root across the loop and persist final root to catalog.
