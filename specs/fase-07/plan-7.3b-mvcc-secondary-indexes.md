# Plan: 7.3b — MVCC on Secondary Indexes

## Files to create/modify

| File | Action | Purpose |
|------|--------|---------|
| `crates/axiomdb-sql/src/executor/delete.rs` | Modify | Remove `delete_from_indexes` / `delete_many_from_indexes` calls |
| `crates/axiomdb-sql/src/executor/update.rs` | Modify | Remove old-key deletion, add HOT skip, add undo tracking |
| `crates/axiomdb-sql/src/executor/insert.rs` | Modify | Heap-aware uniqueness check |
| `crates/axiomdb-sql/src/index_maintenance.rs` | Modify | Add `insert_into_indexes_with_undo`, heap-aware unique check, `vacuum_index` |
| `crates/axiomdb-wal/src/txn.rs` | Modify | Add `UndoOp::IndexInsert`, rollback handler |
| `docs-site/src/internals/mvcc.md` | Modify | MVCC index section |
| `docs/progreso.md` | Modify | Mark 7.3b |

---

## Algorithm / Data Structures

### New UndoOp variant

```rust
// In crates/axiomdb-wal/src/txn.rs
pub enum UndoOp {
    // ... existing variants ...

    /// Undo an index INSERT: remove the entry from the B-Tree.
    /// Recorded when INSERT or UPDATE adds a new index entry.
    /// On ROLLBACK, `BTree::delete_in(storage, root, &key)` is called.
    UndoIndexInsert {
        index_id: u32,
        root_page_id: u64,
        key: Vec<u8>,
    },
}
```

**Why `root_page_id` instead of looking up catalog?** During rollback, the catalog
may not be in a consistent state (the transaction is being aborted). Storing the
root at recording time avoids a catalog lookup during undo.

### HOT check

```rust
// In update.rs, before index maintenance:
fn indexed_columns_changed(
    indexes: &[IndexDef],
    old_values: &[Value],
    new_values: &[Value],
) -> bool {
    indexes.iter().any(|idx| {
        idx.columns.iter().any(|c| {
            let ci = c.col_idx as usize;
            old_values.get(ci) != new_values.get(ci)
        })
    })
}

// If !indexed_columns_changed → skip ALL index maintenance for this row
```

### Heap-aware uniqueness enforcement

```rust
// In index_maintenance.rs, during unique index insert:
if idx.is_unique && !idx.is_fk_index {
    if let Some(existing_rid) = BTree::lookup_in(storage, idx.root_page_id, &key)? {
        // Check if the existing row is actually visible (alive)
        if HeapChain::is_slot_visible(storage, existing_rid.page_id,
                                       existing_rid.slot_id, snap)? {
            return Err(DbError::UniqueViolation { ... });
        }
        // Dead entry — allow the insert (vacuum will clean the old one later)
    }
}
```

**Requires:** `TransactionSnapshot` passed to insert functions.

### Basic index vacuum

```rust
pub fn vacuum_index(
    storage: &mut dyn StorageEngine,
    index: &IndexDef,
    oldest_visible_snapshot: u64,
) -> Result<usize, DbError> {
    let snap = TransactionSnapshot {
        snapshot_id: oldest_visible_snapshot,
        current_txn_id: 0,
    };
    let root_pid = AtomicU64::new(index.root_page_id);
    let all_entries = BTree::range_in(storage, index.root_page_id, None, None)?;

    let mut removed = 0;
    for (rid, key_bytes) in &all_entries {
        if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap)? {
            let _ = BTree::delete_in(storage, &root_pid, key_bytes);
            removed += 1;
        }
    }
    Ok(removed)
}
```

**Note:** This is a basic O(N) scan. Phase 7.11 will add incremental/smart vacuum.

---

## Implementation Phases

### Phase 1: UndoOp::UndoIndexInsert + rollback handler

1. Add `UndoIndexInsert { index_id: u32, root_page_id: u64, key: Vec<u8> }` to `UndoOp` enum
2. Add rollback handler in `TxnManager::rollback()`: for each `UndoIndexInsert`,
   call `BTree::delete_in(storage, &AtomicU64::new(root_page_id), &key)`
3. Add rollback handler in `TxnManager::rollback_to_savepoint()` (same logic)
4. Unit test: record UndoIndexInsert, rollback, verify entry removed

### Phase 2: DELETE executor — stop removing index entries

5. In `execute_delete_ctx()`: remove the block that calls `collect_delete_keys_by_index`
   + `delete_many_from_indexes` for secondary indexes
6. In `execute_delete()`: same — remove `delete_from_indexes` call
7. Keep FK auto-index deletion for CASCADE (FK indexes need special handling — see
   note below)
8. Test: DELETE row → verify index entry still exists → verify heap row invisible →
   verify SELECT via index skips the dead entry

**FK auto-index note:** FK reverse-lookup indexes (`is_fk_index = true`) are used by
the FK checker to find child rows. Dead entries in FK indexes cause false positives
during FK enforcement (parent delete → finds dead child → blocks delete). Two options:
- (A) Keep deleting FK index entries immediately (they're always consistent under RwLock)
- (B) Make FK checker also heap-aware

Option A is simpler and correct for single-writer. We keep `delete_from_indexes` only
for `is_fk_index` indexes.

### Phase 3: UPDATE executor — lazy delete + HOT

9. Add `indexed_columns_changed()` helper function
10. In `apply_update_index_maintenance()`: remove the delete phase
    (`delete_many_from_single_index` calls) for non-FK indexes
11. In `apply_update_index_maintenance()`: add HOT check — if
    `!indexed_columns_changed` for this specific index, skip it entirely
12. After each `insert_many_into_single_index` call, push `UndoOp::UndoIndexInsert`
    for each new entry added (need to track which entries were inserted)
13. Test: UPDATE indexed col → verify old and new entries both exist →
    verify only new entry visible via heap check
14. Test: UPDATE non-indexed col → verify NO index modifications (HOT)
15. Test: UPDATE indexed col then ROLLBACK → verify new entry removed, old entry intact

### Phase 4: INSERT executor — heap-aware uniqueness

16. Pass `TransactionSnapshot` to `insert_into_indexes` (and batch variant)
17. Change uniqueness check: after `BTree::lookup_in` finds existing entry,
    call `HeapChain::is_slot_visible` — only raise `UniqueViolation` if visible
18. After successful index insert, push `UndoOp::UndoIndexInsert` to undo log
19. Test: DELETE row with unique key → INSERT same key → succeeds
20. Test: INSERT duplicate of live row → still fails with UniqueViolation
21. Test: INSERT then ROLLBACK → index entry stays but heap row invisible

### Phase 5: Basic vacuum_index

22. Add `vacuum_index()` function in `index_maintenance.rs`
23. Add SQL `VACUUM` command support (or internal-only API for now)
24. Test: INSERT rows, DELETE some, vacuum → verify dead entries removed
25. Test: vacuum with no dead entries → returns 0

### Phase 6: Documentation + close

26. Update `docs-site/src/internals/mvcc.md` — add "MVCC on Secondary Indexes" section
27. Update `docs/progreso.md` — mark 7.3b `[x] ✅`

---

## Tests to Write

### Unit tests

```
test_undo_index_insert_on_rollback
  — begin, insert row+index, record UndoIndexInsert, rollback → index entry gone

test_undo_index_insert_on_savepoint_rollback
  — begin, savepoint, insert, rollback to savepoint → index entry gone, heap row dead
```

### Integration tests (in executor tests)

```
test_delete_leaves_index_entry
  — INSERT row, DELETE row → index entry exists, SELECT via index returns nothing

test_delete_then_reinsert_unique
  — INSERT (email='x'), DELETE, INSERT (email='x') → no UniqueViolation

test_update_indexed_col_keeps_both_entries
  — INSERT (name='A'), UPDATE name='B' → index has 'A' and 'B' entries
  — SELECT WHERE name='A' → 0 rows (heap check filters dead)
  — SELECT WHERE name='B' → 1 row

test_update_non_indexed_col_hot
  — INSERT row, UPDATE non-indexed col → index untouched (verify via BTree scan count)

test_update_indexed_col_rollback
  — BEGIN, UPDATE name='A'→'B', ROLLBACK → only 'A' entry in index, 'B' gone

test_insert_rollback_leaves_orphan
  — BEGIN, INSERT row, ROLLBACK → index entry exists but SELECT returns nothing

test_unique_check_with_dead_entry
  — INSERT (email='x'), DELETE (email='x'), INSERT (email='x') → succeeds

test_vacuum_removes_dead_entries
  — INSERT 10 rows, DELETE 5 → vacuum → 5 entries remain in index

test_vacuum_no_dead_entries
  — INSERT 10 rows → vacuum → 10 entries remain, returns 0
```

---

## Anti-patterns to Avoid

- **DO NOT** add `txn_id` to index entries. The spec explicitly rejected this based on
  research of all four production databases. Heap is the source of truth.

- **DO NOT** remove FK auto-index entries lazily. FK enforcement requires checking child
  existence via reverse-lookup indexes. Dead entries in FK indexes would cause false
  FK violations. Keep immediate deletion for `is_fk_index = true` indexes only.

- **DO NOT** call `vacuum_index` inside `flush()`. Vacuum is a background operation
  that should run separately, not on the critical commit path.

- **DO NOT** store catalog references in `UndoOp`. During rollback the catalog may be
  inconsistent. Store the `root_page_id` at recording time.

- **DO NOT** assume `is_slot_visible` is infallible. If the page was freed and
  reallocated, the slot might contain garbage. Handle errors gracefully (treat as
  not-visible).

---

## Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Dead entries accumulate in indexes | Larger indexes, slower scans | vacuum_index cleans them; Phase 7.11 adds auto-vacuum |
| FK checker sees dead entries as live | Spurious FK violations | Keep immediate delete for is_fk_index indexes |
| UndoIndexInsert key data is large | More memory per transaction | Bounded by transaction size; same cost as row undo images |
| root_page_id in UndoOp becomes stale after split | Rollback deletes from wrong root | AtomicU64 in BTree handles root changes transparently |
| Vacuum runs while writer is active | Index corruption from concurrent modification | Vacuum requires write lock (same as any DML) |
| Range scan performance degrades with many dead entries | Slower queries | Monitor via bloat metrics (7.11); vacuum aggressively |
