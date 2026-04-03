# Spec: 39.16 — Executor Integration: UPDATE on Clustered Table

## What to build (not how)

Route UPDATE queries on clustered tables through the clustered B-tree storage layer.
The executor must handle three update scenarios:

1. **Non-key in-place update**: SET only non-PK, non-indexed columns → rewrite cell in leaf
2. **Non-key relocation**: row grows beyond leaf capacity → delete + re-insert (same key)
3. **Key change**: SET modifies PK column → delete-mark old + insert new + update all secondaries

## Research findings

### InnoDB UPDATE decision tree (primary reference)
- `row_upd()` (row0upd.cc:2715): checks `row_upd_changes_some_index_ord_field_binary()`:
  - If ANY index ordering field changes → `cmpl_info = 0` → delete+insert path
  - Else → `cmpl_info = UPD_NODE_NO_ORD_CHANGE` → in-place path
- In-place: `row_upd_clust_rec()` calls `btr_cur_update_in_place()` or `btr_cur_optimistic_update()`
  based on `row_upd_changes_field_size_or_external()` (size change detection)
- Key change: `row_upd_clust_rec_by_insert()` — delete-mark old, insert new, update secondaries
- Secondary index: only updated if `!(cmpl_info & UPD_NODE_NO_ORD_CHANGE)` — smart skip

### PostgreSQL (contrast)
- HOT optimization: if no indexed columns change AND new tuple fits on same page → no index update
- No equivalent to "clustered in-place" — always creates new tuple version on heap
- Version chain via ctid links

### AxiomDB storage layer (already complete)
- `clustered_tree::update_in_place(key, new_row_data, txn_id, snapshot)` — same-leaf rewrite
- `clustered_tree::update_with_relocation(key, new_row_data, txn_id, snapshot)` — fallback delete+insert
- `clustered_tree::delete_mark(key, txn_id, snapshot)` — MVCC soft delete
- `clustered_tree::insert(key, row_header, row_data)` — full B-tree insert
- `ClusteredSecondaryLayout::update_row(old_row, new_row)` — secondary index maintenance
- All with WAL + undo support (39.11)

## Inputs / Outputs

- Input: `UPDATE table SET col=expr WHERE predicate` on clustered table
- Output: `QueryResult::Affected { count, last_insert_id: None }`
- Errors: UniqueViolation (PK duplicate on key change), NotNullViolation, type mismatch

## Three update paths

### Path 1: Non-key in-place (most common, ~95% of UPDATEs)

```
When: SET columns are NOT in PK and NOT in any secondary index key
  1. Find candidate rows (scan or PK lookup)
  2. For each candidate:
     a. Encode new_row_data from merged old+new values
     b. Encode PK key from old values (unchanged)
     c. Call clustered_tree::update_in_place(key, new_row_data, txn_id, snapshot)
     d. If Ok(true) → success, record WAL
     e. If HeapPageFull → call update_with_relocation()
  3. No secondary index maintenance needed
```

### Path 2: Non-key with relocation (~4% of UPDATEs)

```
When: Row grows beyond leaf capacity during in-place attempt
  1. update_in_place() returns HeapPageFull
  2. Call update_with_relocation(key, new_row_data, txn_id, snapshot)
     → Internally: delete_physical(old) + insert(new)
  3. Update catalog root if tree structure changed
  4. Secondary index: still no update needed (key unchanged)
```

### Path 3: Key change (~1% of UPDATEs)

```
When: SET modifies PK column or secondary index key column
  1. Find candidate rows
  2. For each candidate:
     a. Delete-mark old row: clustered_tree::delete_mark(old_pk_key, txn_id)
     b. Insert new row: clustered_tree::insert(new_pk_key, new_header, new_row_data)
     c. Update all secondary indexes:
        - For each secondary: ClusteredSecondaryLayout::update_row(old_row, new_row)
  3. Update catalog root if tree structure changed
```

## Candidate collection for clustered tables

**Full scan (WHERE on non-PK column):**
- Use `clustered_tree::range(Unbounded, Unbounded, snapshot)` → iterate all visible rows
- Apply WHERE filter on decoded values
- Collect (pk_key, old_values) pairs

**PK point lookup (WHERE pk = literal):**
- Use `clustered_tree::lookup(pk_key, snapshot)` → one row
- Apply remaining WHERE filter

**PK range (WHERE pk BETWEEN lo AND hi):**
- Use `clustered_tree::range(lo, hi, snapshot)` → iterate matching rows
- Apply remaining WHERE filter

## WAL integration

- In-place update: `txn.record_clustered_update(table_id, key, old_row_image, new_row_image)`
- Delete+insert (key change): `txn.record_clustered_delete_mark()` + `txn.record_clustered_insert()`
- Relocation: same as delete+insert WAL entries
- All with undo support for ROLLBACK

## Acceptance criteria

- [ ] `ensure_heap_runtime` guard removed for UPDATE on clustered tables
- [ ] Non-key in-place update via `clustered_tree::update_in_place()`
- [ ] Relocation fallback via `clustered_tree::update_with_relocation()`
- [ ] PK change via delete-mark + insert
- [ ] Secondary index maintenance via `ClusteredSecondaryLayout::update_row()`
- [ ] Catalog root updated when tree structure changes
- [ ] MVCC: only visible rows updated (snapshot check)
- [ ] WAL entries generated for all update paths
- [ ] Rollback undoes clustered updates correctly
- [ ] Existing heap UPDATE paths unchanged
- [ ] Integration test: in-place update on clustered table
- [ ] Integration test: update that causes relocation
- [ ] Integration test: PK change update
- [ ] Integration test: update with secondary index maintenance
- [ ] Integration test: UPDATE with WHERE on PK (point + range)

## Out of scope

- Field-patch fast path on clustered leaves (optimization for future)
- Fused index-range patch on clustered (optimization for future)
- UPDATE with JOIN on clustered tables

## Dependencies

- 39.15 (SELECT from clustered) — candidate collection reuses scan paths ✅
- 39.6 (Update in-place) — storage layer ✅
- 39.8 (Split & merge) — relocation may trigger structural changes ✅
- 39.9 (Secondary PK bookmarks) — secondary index maintenance ✅
- 39.11 (WAL) — clustered WAL entries ✅
