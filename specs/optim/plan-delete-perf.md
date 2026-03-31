# Plan: DELETE Performance — Batch WAL + Deferred PK Index Deletion

## Files to modify

| File | Change |
|------|--------|
| `crates/axiomdb-sql/src/executor/delete.rs` | Remove ALL index maintenance from DELETE |
| `crates/axiomdb-sql/src/table.rs` | Return (page_id, slot_ids) from delete_rows_batch for batch WAL |
| `crates/axiomdb-wal/src/lib.rs` | Add `EntryType::PageDelete` variant |
| `crates/axiomdb-wal/src/txn.rs` | Add `record_delete_batch()` using PageDelete entries |
| `crates/axiomdb-sql/src/vacuum.rs` | Remove `is_unique` / `is_fk_index` filter (vacuum ALL indexes) |
| `docs/progreso.md` | Update PERF note |

## Implementation Phases

### Phase 1: Defer ALL index deletion in DELETE

1. In `execute_delete_ctx`: remove the `immediate_delete_indexes` block entirely
   (lines 111-151). DELETE touches heap only — zero index I/O.
2. In `execute_delete` (non-ctx): same change.
3. Update vacuum_index filter: remove `if idx.is_unique || idx.is_fk_index { continue; }`
   so VACUUM cleans ALL index types.

**Risk: FK auto-indexes.** FK enforcement uses reverse-lookup indexes to find
child rows. Dead entries in FK indexes would cause false positives. BUT: the FK
checker already reads the heap row after finding the index entry. If the heap row
is deleted (txn_id_deleted set), FK enforcement should skip it.

**Verification needed:** Check if `enforce_fk_on_parent_delete` and FK child
enforcement handle dead entries correctly. If they use heap visibility
(is_slot_visible), they're already correct.

### Phase 2: Batch WAL for DELETE

4. Add `EntryType::PageDelete = 10` to WAL entry types.
5. In `delete_rows_batch` (table.rs): return `Vec<(u64, Vec<u16>)>` — page_id + slot_ids.
6. Add `record_delete_batch(table_id, pages: &[(u64, Vec<u16>)])` to TxnManager:
   - For each page: one WAL entry with key = page_id bytes, value = [count, slot_ids...]
   - Undo: one `UndoOp::UndoDelete { page_id, slot_id }` per slot (same as before)
7. In DELETE executor: call `record_delete_batch` instead of per-row `record_delete`.

### Phase 3: Reduce allocations

8. Remove `collect_delete_keys_by_index` call entirely (no longer needed — no
   index maintenance in DELETE).
9. This eliminates 10,000+ allocations and 2 sort operations.

## Tests

```
test_delete_where_no_index_ops
  — DELETE 100 rows → verify B-Tree scan count unchanged

test_delete_then_insert_same_pk
  — DELETE row, INSERT same PK → succeeds (has_visible_duplicate)

test_vacuum_cleans_pk_entries
  — DELETE rows, VACUUM → PK index entries removed

test_batch_wal_page_delete
  — DELETE 100 rows → WAL has ~5 PageDelete entries (not 100 Delete entries)

test_crash_recovery_page_delete
  — Write PageDelete entries, simulate crash, recover → rows correctly deleted
```

## Anti-patterns

- DO NOT skip FK enforcement. FK checking must still happen BEFORE heap delete.
  The change is: FK index ENTRIES are not deleted from B-Tree, but FK CHECKING
  still works because it reads the heap after finding the index entry.

- DO NOT change the bulk-empty fast path (no-WHERE DELETE). It already uses
  root rotation and is optimal.

## Expected impact

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| WAL bytes (5000 rows) | 750 KB | ~5 KB | 150× |
| Index page ops | 120 | 0 | ∞ |
| Allocations | 10,000+ | ~5,000 | 2× |
| Total page I/O | ~175 | ~55 | 3× |
