# Plan: 39.15 — Executor Integration: SELECT from Clustered Table

## Files to modify

| File | Change |
|---|---|
| `crates/axiomdb-sql/src/executor/select.rs` | Remove ensure_heap_runtime guard; add clustered dispatch in each access path |
| `crates/axiomdb-sql/src/table.rs` | Add `decode_clustered_row()` helper to decode ClusteredRow → Vec<Value> |
| `crates/axiomdb-sql/src/executor/mod.rs` | Remove ensure_heap_runtime calls if any at dispatch level |
| `crates/axiomdb-sql/tests/integration_clustered_select.rs` | **NEW**: 10+ integration tests |

## Implementation phases

### Phase 1: Remove the guard

In `select.rs`, remove or conditionalize the `ensure_heap_runtime()` call:
```rust
// BEFORE (line 37):
resolved.def.ensure_heap_runtime("SELECT from clustered table — Phase 39.15")?;

// AFTER:
// Guard removed — clustered tables now handled below.
```

Also remove similar guards in GROUP BY / ORDER BY contexts if present.

### Phase 2: Add clustered decode helper in table.rs

```rust
/// Decodes a ClusteredRow into a Vec<Value> using the table's column definitions.
pub fn decode_clustered_row(
    row: &clustered_tree::ClusteredRow,
    columns: &[ColumnDef],
) -> Result<Vec<Value>, DbError> {
    let col_types = column_data_types(columns);
    codec::decode_row(&row.row_data, &col_types)
}
```

### Phase 3: Full table scan (AccessMethod::Scan) on clustered

In `select.rs`, the `AccessMethod::Scan` branch currently calls `scan_table_filtered_parallel()`.
Add a check:

```rust
AccessMethod::Scan | AccessMethod::IndexOnlyScan { .. } => {
    if resolved.def.is_clustered() {
        // Clustered full scan: iterate all leaves via range(Unbounded, Unbounded)
        let snap = txn.active_snapshot()?;
        let iter = clustered_tree::range(
            storage,
            Some(resolved.def.root_page_id),
            std::ops::Bound::Unbounded,
            std::ops::Bound::Unbounded,
            &snap,
        )?;
        let col_types = column_data_types(&resolved.columns);
        let mut rows = Vec::new();
        for result in iter {
            let clustered_row = result?;
            let values = codec::decode_row(&clustered_row.row_data, &col_types)?;
            // Apply WHERE filter if present
            if let Some(ref wc) = where_clause {
                if !is_truthy(&eval(wc, &values)?) {
                    continue;
                }
            }
            rows.push(values);
        }
        rows
    } else {
        // Existing heap scan path (unchanged)
        TableEngine::scan_table_filtered_parallel(...)
    }
}
```

### Phase 4: PK point lookup (AccessMethod::IndexLookup) on clustered

```rust
AccessMethod::IndexLookup { index_def, key } => {
    if resolved.def.is_clustered() && index_def.is_primary {
        // Clustered PK lookup: direct B-tree search, no heap
        let snap = txn.active_snapshot()?;
        match clustered_tree::lookup(
            storage,
            Some(resolved.def.root_page_id),
            key,
            &snap,
        )? {
            Some(row) => {
                let values = decode_clustered_row(&row, &resolved.columns)?;
                if let Some(ref wc) = where_clause {
                    if !is_truthy(&eval(wc, &values)?) {
                        vec![]
                    } else {
                        vec![values]
                    }
                } else {
                    vec![values]
                }
            }
            None => vec![],
        }
    } else if resolved.def.is_clustered() && !index_def.is_primary {
        // Secondary index on clustered table: extract PK bookmark → clustered lookup
        // ... secondary → PK → clustered path ...
    } else {
        // Existing heap path (unchanged)
        // ...
    }
}
```

### Phase 5: PK range scan (AccessMethod::IndexRange) on clustered

```rust
AccessMethod::IndexRange { index_def, lo, hi } => {
    if resolved.def.is_clustered() && index_def.is_primary {
        let snap = txn.active_snapshot()?;
        let from = lo.as_ref().map(|k| Bound::Included(k.clone())).unwrap_or(Bound::Unbounded);
        let to = hi.as_ref().map(|k| Bound::Included(k.clone())).unwrap_or(Bound::Unbounded);
        let iter = clustered_tree::range(
            storage,
            Some(resolved.def.root_page_id),
            from, to,
            &snap,
        )?;
        let col_types = column_data_types(&resolved.columns);
        let mut rows = Vec::new();
        for result in iter {
            let clustered_row = result?;
            let values = codec::decode_row(&clustered_row.row_data, &col_types)?;
            if let Some(ref wc) = where_clause {
                if !is_truthy(&eval(wc, &values)?) {
                    continue;
                }
            }
            rows.push(values);
        }
        rows
    } else {
        // Existing heap path
    }
}
```

### Phase 6: Secondary index → clustered lookup

When `index_def.is_primary == false` on a clustered table:
```rust
// 1. Get key bytes from secondary B-tree
let (rid, key_bytes) = BTree::lookup_in(storage, index_def.root_page_id, key)?;

// 2. Decode PK bookmark from key bytes
let primary_idx = resolved.indexes.iter().find(|i| i.is_primary).unwrap();
let layout = ClusteredSecondaryLayout::derive(index_def, primary_idx)?;
let entry = layout.decode_entry_key(&key_bytes)?;

// 3. Encode PK as key for clustered lookup
let pk_key = encode_index_key(&entry.primary_key)?;

// 4. Clustered lookup
let row = clustered_tree::lookup(storage, Some(resolved.def.root_page_id), &pk_key, &snap)?;
```

### Phase 7: Return format (RecordId vs no RecordId)

The existing executor expects `Vec<(RecordId, Vec<Value>)>` from scan paths.
Clustered tables don't have RecordIds (no heap slots).

**Solution**: Use a dummy RecordId for compatibility:
```rust
const CLUSTERED_DUMMY_RID: RecordId = RecordId { page_id: 0, slot_id: 0 };
```

Or change the internal format to `Vec<Vec<Value>>` (skip RecordId entirely for clustered).
The RecordId is only used internally for UPDATE/DELETE targeting — not needed for SELECT output.

**Chosen approach**: Use dummy RID for now. Phase 39.16/39.17 will handle UPDATE/DELETE
which need a different targeting mechanism for clustered rows (by PK key, not by RID).

## Tests to write

1. **SELECT * from clustered table** — insert 5 rows, SELECT all, verify all returned
2. **SELECT with WHERE on PK** — point lookup: `WHERE id = 3`
3. **SELECT with PK range** — `WHERE id BETWEEN 2 AND 4`
4. **SELECT with non-PK WHERE** — full scan + filter: `WHERE name = 'Alice'`
5. **SELECT COUNT(*)** — full scan, count all visible rows
6. **SELECT with ORDER BY PK** — already ordered (clustered = PK order)
7. **SELECT with LIMIT** — `LIMIT 3` stops after 3 rows
8. **SELECT with GROUP BY** — `GROUP BY age` on clustered table
9. **SELECT via secondary index** — `WHERE email = 'alice@example.com'` on UNIQUE email index
10. **MVCC visibility** — insert in open txn, SELECT from different snapshot sees nothing
11. **Empty table** — `SELECT * FROM empty_clustered` returns 0 rows
12. **SELECT after split** — insert enough to trigger splits, then SELECT all

## Anti-patterns to avoid

- DO NOT duplicate the entire heap SELECT code — reuse WHERE eval, projection, ORDER BY, LIMIT
- DO NOT break heap table SELECT — all existing paths must remain unchanged
- DO NOT skip MVCC — clustered_tree::range() already filters, but WHERE recheck on decoded values still needed
- DO NOT use clustered_tree::range_in (index B-tree) — use clustered_tree::range (clustered B-tree)
- DO NOT forget secondary index path — it must go through PK bookmark, not RecordId

## Risks

- **RecordId compatibility**: some executor code may assume RecordId is valid (for UPDATE/DELETE targeting).
  Mitigation: use dummy RID; real targeting via PK key in 39.16/39.17.
- **WHERE evaluation on decoded values**: the WHERE clause operates on `Vec<Value>`, not on raw bytes.
  For clustered full scan, this means full decode of every row before WHERE check (same cost as heap).
  Future optimization: BatchPredicate on clustered leaf raw bytes (39.20).
- **GROUP BY / ORDER BY / LIMIT**: these operate on the `Vec<Vec<Value>>` output, not on the scan.
  They should work unchanged as long as the scan returns the right format.
