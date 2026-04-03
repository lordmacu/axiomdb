# Spec: 39.17 — Executor Integration: DELETE from Clustered Table

## What to build (not how)

Route DELETE queries on clustered tables through the clustered B-tree storage layer.
DELETE on clustered tables is a soft delete (MVCC mark) — sets `txn_id_deleted` in
the RowHeader without physically removing the cell from the leaf page. Physical
removal deferred to VACUUM (39.18).

## Research findings

### InnoDB DELETE (primary reference)
- DELETE is implemented as UPDATE: `row_upd_del_mark_clust_rec()` sets delete flag +
  txn_id + roll_ptr in the clustered record header. Row data preserved for undo/FK checks.
- Single page X-latch during mark. Single WAL entry. Undo log entry for rollback.
- Secondary index entries left in place — purged by background thread later.
- Physical removal by `row_purge_del_mark()` → `btr_cur_pessimistic_delete()` when
  `txn_id < oldest_active_snapshot`.

### PostgreSQL DELETE
- `heap_delete()` sets `xmax = current_txn_id` in tuple header. No undo log.
- Secondary indexes completely untouched. VACUUM handles all cleanup.
- Physical removal: `lazy_vacuum_heap_rel()` marks `LP_UNUSED` line pointers.

### AxiomDB storage (already complete)
- `clustered_tree::delete_mark(key, txn_id, snapshot)` — sets `txn_id_deleted` ✅
- Preserves overflow chains for dead rows ✅
- WAL: `txn.record_clustered_delete_mark()` ✅
- Undo: `UndoClusteredRestore` restores pre-delete state ✅
- Recovery: `RecoveryOp::ClusteredRestore` handles crash ✅
- Secondary indexes: deferred cleanup (PostgreSQL model) ✅

## Inputs / Outputs

- Input: `DELETE FROM table WHERE predicate` on clustered table
- Output: `QueryResult::Affected { count }`
- Errors: ForeignKeyViolation if referenced by child rows

## Execution flow

```
1. Resolve table → verify is_clustered()
2. Candidate collection:
   - No WHERE → full clustered scan (Unbounded, Unbounded)
   - WHERE on PK → clustered range or lookup
   - WHERE on non-PK → full scan + filter
3. FK parent enforcement (if table referenced by FKs):
   - For each candidate, check child tables have no matching rows
   - RESTRICT: abort if referenced. CASCADE: propagate delete.
4. For each candidate (pk_key, old_values):
   a. clustered_tree::delete_mark(key, txn_id, snapshot) → sets txn_id_deleted
   b. Record WAL: txn.record_clustered_delete_mark()
5. Secondary index entries: left in place (filtered on read by MVCC)
6. Update catalog root if tree structure changed (unlikely for delete-mark)
7. Return affected count
```

## Acceptance criteria

- [ ] `ensure_heap_runtime` guard removed for DELETE on clustered tables
- [ ] Candidate collection via clustered scan (full/PK range/PK lookup)
- [ ] Soft delete via `clustered_tree::delete_mark()` per candidate
- [ ] WAL entries via `txn.record_clustered_delete_mark()`
- [ ] FK parent enforcement before delete (if applicable)
- [ ] Secondary index entries left in place (deferred cleanup)
- [ ] Existing heap DELETE paths unchanged
- [ ] Integration test: DELETE with WHERE on PK
- [ ] Integration test: DELETE all rows
- [ ] Integration test: DELETE with non-PK WHERE
- [ ] Integration test: Verify deleted rows invisible to new snapshot
- [ ] Integration test: Rollback restores deleted row

## Out of scope

- Physical cell removal (39.18 VACUUM)
- Bulk-empty fast path for clustered tables (future optimization)
- CASCADE/SET NULL FK propagation on clustered tables (existing heap FK logic)
- DELETE with LIMIT on clustered tables

## Dependencies

- 39.7 (Delete mark) — storage layer ✅
- 39.11 (WAL) — clustered WAL entries ✅
- 39.15 (SELECT) — candidate collection reuses scan paths ✅
