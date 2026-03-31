# Spec: DELETE Performance — Batch WAL + Deferred PK Index Deletion

## Context

AxiomDB's range DELETE (`DELETE FROM t WHERE id <= 5000`) runs at 886K rows/s
vs MariaDB's 1.15M — a 1.30× gap. Investigation across all four research
databases (InnoDB, PostgreSQL, SQLite, DuckDB) identified three bottlenecks:

1. **Per-row WAL entries** (750 KB for 5000 rows vs InnoDB's ~50 KB)
2. **10,000 allocations** in `collect_delete_keys_by_index` (Vec<Value> + Vec<u8>
   per row × per index)
3. **120 B-Tree page ops** for PK/unique index deletion (PostgreSQL does ZERO
   index ops on DELETE — defers to VACUUM)

**Reference:**
- PostgreSQL: marks xmax on heap tuple only; indexes untouched until VACUUM
- InnoDB: soft-delete mark on clustered index; secondary indexes deferred to purge
- SQLite: physical cell removal, hard delete
- DuckDB: deletion vector, columnar, zero WAL

## What to build

### A. Batch WAL for DELETE

Instead of 5000 individual `WalEntry::Delete` entries, write **one WAL entry per
affected page** listing the slot_ids deleted on that page. Same pattern as
INSERT's `record_page_writes()`.

```
Before: 5000 × WalEntry::Delete (~150 bytes each) = 750 KB
After:  25 × WalEntry::PageDelete (~200 bytes each) = 5 KB
```

New entry type: `EntryType::PageDelete` — key = page_id, value = [slot_id_count,
slot_id_0, slot_id_1, ...]. Crash recovery replays by calling `mark_deleted` on
each listed slot.

### B. Defer PK/unique index deletion (PostgreSQL model)

Extend Phase 7.3b lazy deletion to ALL indexes (including PK and unique).
Currently unique/PK indexes are deleted immediately because `BTree::insert_in`
rejects duplicate keys. But `has_visible_duplicate()` (from 7.3b) already handles
this: it checks heap visibility before raising UniqueViolation.

With this change:
- DELETE: marks heap row only (zero index I/O for ANY index type)
- INSERT after DELETE of same key: `has_visible_duplicate()` sees dead row → allows
- VACUUM: cleans dead entries from ALL indexes (not just non-unique)

This eliminates ~120 page ops (60 reads + 60 writes) for a 5000-row range DELETE
with 2 indexes.

### C. Reduce allocations in collect_delete_keys_by_index

Pre-allocate key encoding buffer, avoid per-row Vec<Value> intermediate.
Encode keys directly from the row's Vec<Value> without cloning.

## Inputs / Outputs

### Batch WAL
- **Input:** grouped (page_id, Vec<slot_id>) from `delete_batch`
- **Output:** one `WalEntry::PageDelete` per page
- **WAL size reduction:** ~150× fewer entries, ~150× smaller WAL

### Deferred PK index deletion
- **Input:** none (remove code that calls `delete_many_from_indexes` for
  unique/PK in DELETE executor)
- **Output:** zero index I/O during DELETE
- **Correctness:** `has_visible_duplicate()` ensures INSERT of same key works

### Allocation reduction
- **Input:** rewrite `collect_delete_keys_by_index` loop
- **Output:** fewer heap allocations per row

## Acceptance Criteria

- [ ] DELETE with WHERE produces batch WAL entries (one per page, not per row)
- [ ] Crash recovery handles `PageDelete` entries correctly
- [ ] PK/unique index entries NOT deleted during DELETE (deferred to VACUUM)
- [ ] INSERT after DELETE of same unique key succeeds (has_visible_duplicate)
- [ ] VACUUM cleans PK/unique dead entries (extend vacuum_index filter)
- [ ] collect_delete_keys no longer called for DELETE (no index maintenance)
- [ ] All existing DELETE tests pass
- [ ] `cargo test --workspace` passes

## Out of Scope

- Batch WAL for UPDATE (separate optimization)
- Parallel DELETE across pages
- Free space reclamation after DELETE (VACUUM FULL)

## Dependencies

- `has_visible_duplicate()` from Phase 7.3b — already exists
- `vacuum_index()` from Phase 7.11 — already exists (needs filter change)
- `EntryType` enum in WAL — needs new variant
- `record_page_writes` pattern — already exists for INSERT
