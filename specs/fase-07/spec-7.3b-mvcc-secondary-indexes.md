# Spec: 7.3b — MVCC on Secondary Indexes

## Context

AxiomDB's secondary indexes currently modify B-Tree entries **immediately** during
DML: INSERT adds an entry, DELETE removes it, UPDATE deletes the old key and inserts
the new one. This is correct under the single-writer `Arc<RwLock<Database>>` model
because readers never see intermediate state. However:

1. **ROLLBACK (Phase 7.6)** cannot undo physical index deletions — the entries are
   already gone from the B-Tree.
2. **Future concurrent writers** (Phase 13.7, row-level locking) would see
   inconsistent index state during concurrent transactions.
3. **DELETE + re-INSERT of the same unique key** within a transaction causes a
   spurious uniqueness violation because the old entry was already removed.

All four production databases studied (InnoDB, PostgreSQL, DuckDB, SQLite) solve this
the same way: **indexes do NOT store transaction IDs**. Visibility is always determined
at the heap via the row's `txn_id_created` / `txn_id_deleted` fields. Index entries for
deleted or rolled-back rows are "dead" entries that are filtered out by heap visibility
checks and cleaned up by vacuum.

AxiomDB adopts this proven approach: **lazy index deletion** (InnoDB model) combined
with a **HOT optimization** (PostgreSQL model) that skips index maintenance entirely
when UPDATE does not change any indexed column.

---

## What to build (not how)

### A. Lazy index deletion

When a row is DELETEd, do NOT remove its index entries. The heap row is marked as
deleted (`txn_id_deleted = T`), and index entries become "dead" — they still exist in
the B-Tree but are invisible to readers because `is_slot_visible()` returns false.

When a row is UPDATEd and an indexed column changes, insert the new index entry
WITHOUT removing the old one. Both entries coexist in the B-Tree. The old entry
points to the same RecordId as before; heap visibility determines which version is
current.

### B. Heap-aware uniqueness enforcement

When inserting into a unique index, if the key already exists, check heap visibility
before raising a `UniqueViolation`. If the existing entry points to a dead row
(deleted or uncommitted by another transaction), allow the insert.

### C. Index undo tracking for ROLLBACK

When INSERT or UPDATE adds a new index entry, record it in the transaction's undo
log as `UndoOp::IndexInsert`. On ROLLBACK, these entries are physically removed from
the B-Tree (the old entries are still in place for UPDATE; for INSERT, the heap row
becomes uncommitted → invisible via heap check).

### D. HOT optimization (skip index maintenance)

If an UPDATE does not change any column that participates in any secondary index,
skip all index maintenance for that row. This is PostgreSQL's Heap-Only Tuple (HOT)
optimization — the most impactful optimization for UPDATE-heavy workloads.

### E. Basic index vacuum

A vacuum function that scans index entries and removes those pointing to dead heap
rows (rows where `txn_id_deleted` is committed and older than the oldest active
snapshot). This prevents unbounded growth of dead entries.

---

## Inputs / Outputs

### DELETE executor
- **Input:** rows to delete (RecordIds + row values)
- **Output:** rows deleted from heap only; indexes NOT touched
- **Change:** Remove call to `delete_from_indexes()`

### UPDATE executor
- **Input:** rows to update (old values, new values, RecordIds)
- **Output:** heap updated; for each index where key changed: new entry inserted
  (old entry left in place); for indexes where key did NOT change: nothing
- **Change:** Remove old-key deletion; add HOT check; add undo tracking

### INSERT executor (uniqueness check)
- **Input:** new row values, unique index key
- **Output:** `Ok(())` if no visible duplicate; `Err(UniqueViolation)` if visible
  duplicate exists
- **Change:** After `BTree::lookup` finds existing entry, check `is_slot_visible`

### ROLLBACK (undo replay)
- **Input:** list of `UndoOp::IndexInsert { index_id, key, rid }`
- **Output:** each recorded entry removed from its B-Tree
- **Change:** New undo op type + handler in `TxnManager::rollback()`

### Vacuum
- **Input:** index definition + oldest active snapshot
- **Output:** count of dead entries removed
- **Change:** New function in executor or storage layer

---

## Design decisions (with rationale from research/)

### No txn_id in index entries

The original 7.3b description proposed `(key, RecordId, txn_id_visible_from)`.

**Rejected.** All four databases studied (InnoDB, PostgreSQL, DuckDB, SQLite) keep
indexes free of version info. Reasons:

- **+8 bytes per entry** reduces ORDER_LEAF from 217 to ~190 (12% fewer entries per
  leaf page → taller tree → more I/O)
- **Heap visibility is already cheap**: `is_slot_visible()` reads only the 24-byte
  slot header, not the full row
- **Complexity**: index comparison would need to handle txn_id in sort order; vacuum
  would need to reason about index-level visibility
- **Industry consensus**: InnoDB has served billions of transactions without txn_id
  in secondary indexes

### Lazy delete vs immediate delete + undo

**Chosen: lazy delete** (leave dead entries in the index).

Alternative: keep immediate delete and re-insert on rollback. Rejected because:
- Re-inserting a key can trigger B-Tree splits (non-deterministic undo)
- Not forward-compatible with concurrent writers
- InnoDB uses lazy delete for exactly these reasons

### HOT optimization scope

Only skip index maintenance when **no indexed column changed**. Unlike PostgreSQL's
full HOT (which also requires same-page insertion), AxiomDB's simpler check is
sufficient because the heap already supports in-place updates via
`update_rows_preserve_rid`.

---

## Use Cases

### 1. DELETE + re-INSERT same unique key (same transaction)
```sql
BEGIN;
DELETE FROM users WHERE email = 'foo@bar.com';
INSERT INTO users (email, name) VALUES ('foo@bar.com', 'New Name');
COMMIT;
```
- DELETE: heap row marked deleted, index entry stays
- INSERT: uniqueness check → finds old entry → heap check → dead → allow
- INSERT: new entry added to index, new heap row created
- After commit: two index entries for same key, one dead, one alive
- Vacuum: removes dead entry

### 2. UPDATE indexed column, then ROLLBACK
```sql
BEGIN;
UPDATE users SET email = 'new@bar.com' WHERE id = 42;
-- old entry (foo@bar.com, rid=42) stays in index
-- new entry (new@bar.com, rid=42) added to index
-- undo log: IndexInsert(idx_email, new@bar.com, rid=42)
ROLLBACK;
-- undo removes new entry (new@bar.com)
-- old entry (foo@bar.com) still in index → correct
```

### 3. UPDATE non-indexed column (HOT)
```sql
UPDATE users SET last_login = NOW() WHERE id = 42;
-- email index: no change (HOT skip)
-- heap: row updated in place
-- Result: zero index I/O
```

### 4. Range scan with dead entries
```sql
SELECT * FROM orders WHERE status = 'pending';
-- Index range scan returns 1000 entries (800 alive + 200 dead)
-- For each entry: is_slot_visible() → 200 skipped, 800 returned
-- Performance: slightly worse than without dead entries
-- Fix: vacuum removes dead entries
```

### 5. INSERT into unique index with dead duplicate
```sql
-- Previous transaction deleted row with email='x@y.com' and committed
-- Index still has the dead entry
INSERT INTO users (email) VALUES ('x@y.com');
-- Uniqueness check: lookup finds entry → heap visibility → dead → allow insert
```

### 6. Concurrent readers during write (RwLock)
```sql
-- Writer (holds write lock):
DELETE FROM users WHERE id = 42;
-- Index entry stays, heap row marked deleted

-- Readers (waiting for read lock):
-- After writer commits and releases lock:
-- Reader takes read lock, scans index
-- Finds entry for id=42 → heap check → deleted → skip
-- Correct behavior without any index modification
```

---

## Acceptance Criteria

- [ ] DELETE does NOT call `BTree::delete_in` for secondary index entries.
- [ ] DELETE only marks heap rows as deleted via `txn_id_deleted`.
- [ ] UPDATE of indexed column inserts new key WITHOUT deleting old key.
- [ ] UPDATE of non-indexed column skips ALL index maintenance (HOT).
- [ ] INSERT into unique index with dead duplicate succeeds (heap visibility check).
- [ ] INSERT into unique index with live duplicate fails with `UniqueViolation`.
- [ ] `UndoOp::IndexInsert` recorded for every new index entry (INSERT and UPDATE).
- [ ] ROLLBACK removes index entries recorded in undo log.
- [ ] ROLLBACK of DELETE: index entry intact, heap row's `txn_id_deleted` reset → row visible via index.
- [ ] ROLLBACK of INSERT: index entry stays but heap row invisible → correct.
- [ ] ROLLBACK of UPDATE: new entry removed via undo, old entry intact → correct.
- [ ] Index-only scans still work (already filter by `is_slot_visible`).
- [ ] Regular index scans still work (already filter by heap visibility).
- [ ] Basic `vacuum_index` function removes entries pointing to dead rows.
- [ ] `cargo test -p axiomdb-sql` passes clean.
- [ ] `cargo clippy --workspace -- -D warnings` passes clean.
- [ ] `docs-site/src/internals/mvcc.md` updated with MVCC index section.

---

## Out of Scope

- **Concurrent writers**: This spec operates under the single-writer `Arc<RwLock>`
  model. The lazy delete design is forward-compatible with concurrent writers but
  does not implement concurrent access. That belongs in Phase 13.7.
- **Automatic vacuum scheduling**: The `vacuum_index` function is callable but not
  scheduled. Automatic vacuum belongs in Phase 7.11.
- **Index-level visibility bit**: An optimization where the index entry itself carries
  a "definitely visible" flag to skip the heap check. Deferred — the heap check is
  already cheap (24 bytes).
- **INCLUDE column storage in B-Tree**: Phase 6.13 only stores INCLUDE column
  metadata in the catalog. Storing actual values in B-Tree leaf nodes is separate work.
- **Partial index predicate re-evaluation on vacuum**: When vacuuming a partial index,
  dead entries are identified by heap visibility, not by re-evaluating the WHERE
  predicate. This is correct because dead rows are dead regardless of the predicate.

---

## Dependencies

- **7.1-7.3** (MVCC visibility, snapshot isolation) — already completed
- **7.4** (RwLock architecture) — already completed
- **`HeapChain::is_slot_visible`** — already exists, used by index-only scans
- **`BTree::delete_in`** — already exists, used by undo rollback
- **`BTree::insert_in`** — already exists, no change
- **`TxnManager` undo log** — already has `UndoOp` enum, needs new variant

---

## ⚠️ DEFERRED

- **Index vacuum triggered by bloat threshold**: Phase 7.11 will add automatic vacuum
  with bloat detection. This spec only provides the `vacuum_index` function.
- **Index-level dead entry counter**: Tracking dead entries per index for vacuum
  prioritization. Deferred to 7.11.
- **Concurrent writer safety**: Lazy delete is designed for it, but actual concurrent
  access testing and lock protocol belong in Phase 13.7+.
