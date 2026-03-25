# Plan: FK + Index Improvements (Phase 6.9)

## Files to create / modify

### Create
- `crates/axiomdb-sql/tests/integration_6_9.rs` — all Phase 6.9 integration tests

### Modify
- `crates/axiomdb-catalog/src/schema.rs` — `IndexDef.is_fk_index: bool` (flags bit 2)
- `crates/axiomdb-sql/src/index_maintenance.rs` — remove `!i.is_primary` filter; FK composite key
- `crates/axiomdb-sql/src/executor.rs` — re-enable FK auto-index; PK bloom; persist_fk_constraint
- `crates/axiomdb-sql/src/fk_enforcement.rs` — remove full-scan fallbacks; use B-Tree
- `crates/axiomdb-sql/src/planner.rs` — composite equality rule (Rule 0)

---

## Algorithm / Data structures

### A: Composite FK key build / lookup

```rust
/// Builds the B-Tree key for an FK auto-index entry.
/// Format: encode_index_key(&[fk_val]) ++ encode_rid(rid) (10 bytes)
fn fk_composite_key(fk_val: &Value, rid: RecordId) -> Result<Vec<u8>, DbError> {
    let mut key = encode_index_key(&[fk_val.clone()])?;
    key.extend_from_slice(&axiomdb_index::page_layout::encode_rid(rid));
    Ok(key)
}

/// Returns (lo, hi) bounds for range_in to find all children with fk_val.
fn fk_key_range(fk_val: &Value) -> Result<(Vec<u8>, Vec<u8>), DbError> {
    let prefix = encode_index_key(&[fk_val.clone()])?;
    let mut lo = prefix.clone();
    lo.extend_from_slice(&[0u8; 10]);       // page_id=0, slot_id=0 — minimum rid
    let mut hi = prefix;
    hi.extend_from_slice(&[0xFF; 10]);      // maximum rid — finds all fk_val entries
    Ok((lo, hi))
}
```

### B: `is_fk_index` in `IndexDef.flags`

| Bit | Meaning |
|-----|---------|
| 0   | `is_unique` |
| 1   | `is_primary` |
| 2   | `is_fk_index` (NEW) |

Backward-compatible: old rows have bit 2 = 0 → `is_fk_index = false`.

### C: Modified `insert_into_indexes` inner loop

```rust
// BEFORE:
.filter(|(_, i)| !i.is_primary && !i.columns.is_empty())

// AFTER:
.filter(|(_, i)| !i.columns.is_empty())
// (PK indexes now included — same code path, correct behavior)
```

For `is_fk_index` indexes, key construction differs:
```rust
let key = if idx.is_fk_index {
    fk_composite_key(&key_vals[0], rid)?  // key_vals[0] = FK column value
} else {
    encode_index_key(&key_vals)?
};
// is_fk_index indexes skip the uniqueness check (never unique by design)
```

### D: Composite planner — `collect_eq_conditions`

```rust
/// Collects all col=literal equality conditions reachable via AND-clauses.
fn collect_eq_conditions(expr: &Expr) -> Vec<(&str, Value)> {
    match expr {
        Expr::BinaryOp { op: BinaryOp::And, left, right } => {
            let mut v = collect_eq_conditions(left);
            v.extend(collect_eq_conditions(right));
            v
        }
        other => extract_eq_col_literal(other).into_iter().collect(),
    }
}
```

### E: Composite planner — Rule 0 in `plan_select`

```rust
// Rule 0: composite equality (before existing rules)
if let Some(am) = plan_composite_eq(expr, indexes, columns) {
    return am;
}

fn plan_composite_eq(
    expr: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
) -> Option<AccessMethod> {
    let eq_conds = collect_eq_conditions(expr);
    if eq_conds.len() < 2 { return None; }  // single-col handled by Rule 1

    for idx in indexes.iter().filter(|i| {
        !i.is_primary && !i.is_fk_index && i.columns.len() >= 2 && i.predicate.is_none()
    }) {
        let mut key_parts: Vec<Value> = Vec::new();
        for idx_col in &idx.columns {
            let col = columns.iter().find(|c| c.col_idx == idx_col.col_idx)?;
            match eq_conds.iter().find(|(name, _)| *name == col.name) {
                Some((_, val)) => key_parts.push(val.clone()),
                None => break,  // gap in leading columns — stop
            }
        }
        if key_parts.len() >= 2 {
            if let Ok(key) = encode_index_key(&key_parts) {
                // Use IndexRange lo=hi — works for both unique and non-unique indexes,
                // returns all matching rows (IndexLookup only returns one).
                return Some(AccessMethod::IndexRange {
                    index_def: idx.clone(),
                    lo: Some(key.clone()),
                    hi: Some(key),
                });
            }
        }
    }
    None
}
```

---

## Implementation phases

### Phase 1 — Task A: PK B-Tree population

**Step 1.1** — `index_maintenance.rs`:
Change line 75: `.filter(|(_, i)| !i.is_primary && !i.columns.is_empty())`
→ `.filter(|(_, i)| !i.columns.is_empty())`

Same change at line 150 in `delete_from_indexes`.

**Step 1.2** — `fk_enforcement.rs` in `check_fk_child_insert`:
Remove the `if is_primary { full scan fallback }` branch. After Step 1.1, PK
B-Trees are populated → `BTree::lookup_in` works for all indexes.

Restore the Bloom shortcut for all index types (the TODO comment said "re-enable
when PK B-Trees are populated"):
```rust
// Bloom shortcut restored (PK indexes now populated via insert_into_indexes).
if !bloom.might_exist(parent_idx.index_id, &key) {
    return Err(ForeignKeyViolation { ... });
}
if BTree::lookup_in(storage, parent_index_root, &key)?.is_none() {
    return Err(ForeignKeyViolation { ... });
}
```

**Step 1.3** — `executor.rs` in PK/UNIQUE index creation (the `create_empty_index`
helper inside `execute_create_table`): no change needed — those indexes start
empty. `insert_into_indexes` will populate them on first INSERT.

**Verify:** `cargo test --workspace`. Existing FK tests still pass.
New behavior: PK B-Tree has entries after INSERT.

---

### Phase 2 — Task B: FK composite key index

**Step 2.1** — `schema.rs`:
Add `is_fk_index: bool` to `IndexDef`.
Update `to_bytes()`: `if self.is_fk_index { flags |= 0x04; }`
Update `from_bytes()`: `is_fk_index = flags & 0x04 != 0`
Update all `IndexDef { ... }` literals with `is_fk_index: false`.

**Step 2.2** — `index_maintenance.rs`:
Add `fk_composite_key` and `fk_key_range` free functions.
In the `insert_into_indexes` loop: skip uniqueness check for `idx.is_fk_index`;
use composite key instead of plain `encode_index_key`.
Same in `delete_from_indexes` — use composite key for deletion.

**Step 2.3** — `executor.rs` in `persist_fk_constraint`:
Replace `let fk_index_id: u32 = 0; // ⚠️ DEFERRED` with the full
index-creation code (similar to what existed in the spec but using composite keys):

```rust
let fk_index_id: u32 = {
    // Check if child already has an index covering child_col_idx.
    let existing = { /* list_indexes */ };
    let has_covering = existing.iter().any(|i| {
        !i.columns.is_empty() && i.columns[0].col_idx == child_col_idx
        && !i.is_fk_index  // don't reuse an existing FK index
    });

    if has_covering {
        0 // reuse existing index (user-provided)
    } else {
        // Build FK auto-index with composite keys.
        // Allocate B-Tree leaf root.
        let root_pid = /* alloc + init empty leaf */;

        // Scan child table, insert composite key entries for existing rows.
        let rows = TableEngine::scan_table(/* child */)?;
        for (rid, row_vals) in rows {
            let fk_val = &row_vals[child_col_idx as usize];
            if matches!(fk_val, Value::Null) { continue; }
            let key = fk_composite_key(fk_val, rid)?;
            BTree::insert_in(storage, &root_pid, &key, rid, 90)?;
        }

        let final_root = root_pid.load(Acquire);
        let new_idx_id = writer.create_index(IndexDef {
            index_id: 0, table_id: child_table_id,
            name: format!("_fk_{constraint_name}"),
            root_page_id: final_root,
            is_unique: false, is_primary: false, is_fk_index: true,
            columns: vec![CatIndexColumnDef { col_idx: child_col_idx, order: Asc }],
            predicate: None, fillfactor: 90,
        })?;
        new_idx_id
    }
};
```

**Step 2.4** — `fk_enforcement.rs` in `enforce_fk_on_parent_delete`:
Replace full-scan calls with FK index range scan:

```rust
// For RESTRICT: use FK index if available
let has_child = if let Some(idx) = fk_child_idx {
    let (lo, hi) = fk_key_range(parent_key_val)?;
    !BTree::range_in(storage, idx.root_page_id, Some(&lo), Some(&hi))?.is_empty()
} else {
    children_exist_via_scan(/* fallback for pre-6.9 indexes */)
};

// For CASCADE: collect child RecordIds from FK index
let child_rids: Vec<RecordId> = if let Some(idx) = fk_child_idx {
    let (lo, hi) = fk_key_range(parent_key_val)?;
    BTree::range_in(storage, idx.root_page_id, Some(&lo), Some(&hi))?
        .into_iter().map(|(rid, _)| rid).collect()
} else {
    find_children_via_scan(/* fallback */).into_iter().map(|(rid, _)| rid).collect()
};
// Then read full row values from heap for secondary index maintenance
```

Note: `find_children_via_scan` remains for backward compat (pre-6.9 FKs with
`fk_index_id = 0`). New FKs always have `fk_index_id != 0`.

**Verify:** `cargo test --workspace`. FK tests pass with O(log n) lookups.

---

### Phase 3 — Task C: Composite index planner

**Step 3.1** — `planner.rs`:
Add `collect_eq_conditions(expr: &Expr) -> Vec<(&str, Value)>` private function.
Add `plan_composite_eq(expr, indexes, columns) -> Option<AccessMethod>` private function.
In `plan_select`, add Rule 0 call before Rule 1.

**Step 3.2** — The `execute_select_ctx` IndexRange handling already correctly
fetches multiple rows (Phase 6.3 implemented range scan). No executor changes needed.

**Verify:** `cargo test --workspace`. Planner tests pass, no regressions on
single-column queries.

---

## Tests to write (`tests/integration_6_9.rs`)

### Task A tests
```rust
fn test_pk_btree_populated_on_insert()
fn test_pk_unique_violation_via_btree()
fn test_fk_parent_lookup_uses_btree_not_scan()
fn test_bloom_shortcut_works_for_pk_index()
```

### Task B tests
```rust
fn test_fk_auto_index_created_with_composite_key()
fn test_fk_auto_index_handles_duplicate_fk_values()
fn test_fk_restrict_uses_btree_range_scan()
fn test_fk_cascade_uses_btree_range_scan()
fn test_fk_set_null_uses_btree_range_scan()
fn test_fk_auto_index_is_fk_index_flag()
fn test_pre_69_fk_still_works_full_scan()  // fk_index_id=0 FK still correct
```

### Task C tests
```rust
fn test_composite_index_eq_two_columns()
fn test_composite_index_eq_three_columns()
fn test_composite_index_reversed_where_order()
fn test_composite_index_partial_match_falls_to_single_col()
fn test_composite_index_gap_falls_to_scan()
fn test_composite_index_not_used_for_partial_indexes()
fn test_single_col_rules_unchanged_after_composite()
fn test_composite_returns_multiple_rows()  // non-unique composite index
```

### Catalog serde tests
```rust
fn test_index_def_is_fk_index_roundtrip()
fn test_pre_69_index_row_reads_is_fk_index_false()
```

---

## Anti-patterns to avoid

- **DO NOT** use `fk_composite_key` for non-FK indexes. Only `is_fk_index` indexes
  use the composite key format. Other indexes use plain `encode_index_key`.
- **DO NOT** apply the uniqueness check for `is_fk_index` indexes — they are
  intentionally non-unique at the `fk_val` level. Skip the check when `idx.is_fk_index`.
- **DO NOT** change `delete_from_indexes` for PK indexes when called from non-ctx
  DELETE paths (e.g., `execute_delete`) — those paths don't pass row values for PK
  key extraction. Wait: `delete_from_indexes` already receives the row values;
  PK column value is available → composite key = `encode_index_key(&[pk_val]) + encode_rid(rid)`.
  Actually PK deletion from the B-Tree just needs `encode_index_key(&[pk_val])`
  (plain key, not composite) because the PK B-Tree uses plain keys.
  The composite key is ONLY for `is_fk_index` indexes. PK uses plain key.
- **DO NOT** break the partial index predicate check — `compiled_preds` still needed
  for partial indexes after removing the `!is_primary` filter. The filter change
  is about `is_primary`, not `is_fk_index`.
- **DO NOT** apply Rule 0 to `is_fk_index` indexes — FK auto-indexes are internal
  implementation detail, not for planner selection.

## Risks

| Risk | Mitigation |
|------|-----------|
| PK B-Tree now gets uniqueness check in `insert_into_indexes` → false UniqueViolation for valid AUTO_INCREMENT | AUTO_INCREMENT assigns unique values → no false violation. PK uniqueness via B-Tree is correct by design. |
| FK composite key bytes cause sort-order issues in range scan | `lo = prefix + [0x00; 10]` and `hi = prefix + [0xFF; 10]` always bound the correct range because RecordId bytes are appended AFTER the fk_val encoding (which ends at a defined position) |
| Composite planner returns IndexRange for a unique composite index → always correct | IndexRange lo=hi returns at most 1 entry for unique indexes |
| Composite planner masks Rule 1 for single-col indexes | Rule 0 only activates when `key_parts.len() >= 2`; single-col remains Rule 1 |
| `find_index_on_col` (Rule 1) still skips `is_primary` | Correct — Rule 1 is for secondary indexes. PK is never used for planner optimization directly (full PK range scans aren't meaningful for point lookups in the current system) |
