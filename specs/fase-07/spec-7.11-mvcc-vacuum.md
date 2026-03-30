# Spec: 7.11 — Basic MVCC Vacuum

## Context

After Phase 7.3b (lazy index deletion), deleted rows and dead index entries
accumulate in the database:

- **Heap pages**: rows with `txn_id_deleted != 0` are logically deleted but
  physically present. The space they occupy cannot be reused for new inserts.
  The code comment explicitly states: *"we never reuse space in this
  implementation (VACUUM handles compaction in Phase 7)"*.
- **Secondary indexes**: non-unique index entries pointing to deleted rows
  remain in the B-Tree (Phase 7.3b lazy delete). Range scans traverse these
  dead entries before filtering them via heap visibility.

Without vacuum, disk usage grows without bound and scan performance degrades
linearly with the number of dead entries.

**Reference:** PostgreSQL uses a 3-phase lazy vacuum (heap scan → index
cleanup → space reclaim). InnoDB uses a continuous purge thread. AxiomDB
implements a synchronous on-demand vacuum callable via SQL `VACUUM [table]`.

---

## What to build (not how)

### A. SQL `VACUUM` command

```sql
VACUUM;                    -- vacuum all tables in the current database
VACUUM table_name;         -- vacuum a specific table
```

Runs synchronously within the current connection (like PostgreSQL's manual
`VACUUM`). Requires exclusive write access (uses the existing `db.write()` lock).

### B. Heap vacuum

For each heap page of the target table:
1. Read the page
2. For each slot: if `txn_id_deleted != 0` AND `txn_id_deleted < oldest_safe_txn`,
   call `mark_slot_dead()` to zero the slot entry
3. Write the page back

After vacuum, `read_tuple()` returns `None` for vacuumed slots and `scan_visible()`
skips them without reading the RowHeader.

### C. Index vacuum

For each secondary index on the table:
1. Full B-Tree scan (`BTree::range_in(None, None)`)
2. For each `(RecordId, key_bytes)` entry: check heap visibility via
   `is_slot_visible(storage, rid, snap)`
3. If not visible (dead): `BTree::delete_in(storage, root, &key_bytes)`

### D. Vacuum statistics

Return a result showing what was cleaned:

```sql
VACUUM orders;
-- Result: Vacuumed 'orders': 42 dead rows removed, 3 dead index entries cleaned
```

---

## Inputs / Outputs

### `vacuum_table`
- **Input:** table name (or all tables), storage, txn_manager
- **Output:** `VacuumResult { table: String, dead_rows_removed: u64, dead_index_entries_removed: u64 }`
- **Errors:** `DbError::TableNotFound` if table doesn't exist

### `vacuum_heap_page`
- **Input:** mutable page reference, `oldest_safe_txn: u64`
- **Output:** count of slots vacuumed on this page
- **Side effect:** dead slots zeroed via `mark_slot_dead()`

### `vacuum_index`
- **Input:** index definition, storage, snapshot for dead detection
- **Output:** count of dead entries removed
- **Side effect:** B-Tree entries deleted

### SQL `VACUUM`
- **Input:** optional table name
- **Output:** `QueryResult::Rows` with vacuum statistics per table

---

## `oldest_safe_txn` — when is a deleted row safe to vacuum?

A deleted row is safe to physically remove when **no active or future snapshot
can ever see it**. This means:

```
txn_id_deleted != 0                          -- row was deleted
AND txn_id_deleted < oldest_safe_txn         -- deletion is committed and invisible
```

Under the current `Arc<RwLock<Database>>` architecture:
- Only one writer at a time, no concurrent readers during writes
- All committed transactions are visible to the next reader
- `oldest_safe_txn = max_committed + 1`

Under future concurrent readers (Phase 7.8+):
- `oldest_safe_txn = min(active_snapshot_ids)` across all connections
- A row deleted by txn T is safe only when no connection holds a snapshot ≤ T

For this spec, we use `max_committed + 1` which is correct under RwLock.

---

## Use Cases

### 1. Vacuum after bulk delete
```sql
DELETE FROM logs WHERE created_at < '2025-01-01';
-- 10,000 rows deleted, heap pages still hold dead tuples
VACUUM logs;
-- Dead rows removed, index entries cleaned
-- Heap pages have zeroed slots (space not yet reusable for inserts)
```

### 2. Vacuum all tables
```sql
VACUUM;
-- Iterates all tables in current database, vacuums each one
```

### 3. Vacuum after UPDATE (lazy delete entries)
```sql
UPDATE users SET email = 'new@x.com' WHERE id = 42;
-- Old non-unique index entry left in place (7.3b lazy delete)
VACUUM users;
-- Old index entry removed (heap row was updated, old entry is dead)
```

### 4. Vacuum with nothing to clean
```sql
VACUUM orders;
-- Result: 0 dead rows, 0 dead index entries
```

### 5. Vacuum non-existent table
```sql
VACUUM nonexistent;
-- Error: table 'nonexistent' not found
```

---

## Acceptance Criteria

- [ ] SQL `VACUUM` parsed and dispatched (with and without table name).
- [ ] `vacuum_table()` scans all heap pages and marks dead slots physically dead.
- [ ] `vacuum_index()` removes dead B-Tree entries for each secondary index.
- [ ] Only rows with `txn_id_deleted < oldest_safe_txn` are vacuumed (correctness).
- [ ] Rows with `txn_id_deleted == 0` (alive) are never touched.
- [ ] Rows with `txn_id_deleted >= oldest_safe_txn` (recently deleted) are preserved.
- [ ] `VACUUM` without table name vacuums all tables in current database.
- [ ] `VACUUM nonexistent` returns `TableNotFound` error.
- [ ] Vacuum returns statistics (dead rows removed, dead index entries removed).
- [ ] After vacuum, `scan_visible()` is faster (fewer dead tuples to check).
- [ ] After vacuum, index scans are faster (fewer dead entries to traverse).
- [ ] Unique/PK index entries are NOT vacuumed (they were already deleted in 7.3b).
- [ ] FK auto-index entries are NOT vacuumed (they were already deleted in 7.3b).
- [ ] `cargo test -p axiomdb-sql` passes clean.
- [ ] `cargo clippy --workspace -- -D warnings` passes clean.
- [ ] `docs-site/src/internals/mvcc.md` updated with vacuum section.

---

## Out of Scope

- **Page compaction / defragmentation:** Dead slots are zeroed but the physical
  space between them is not reclaimed. Future Phase 7.11b will add page defrag
  that moves live tuples to fill gaps (keeping same RecordIds via slot reuse).
- **Free space map (FSM):** No free space tracking is added. `insert_tuple()` still
  appends to `free_end`. FSM is a separate optimization.
- **Autovacuum:** No automatic triggering. Vacuum is manual via SQL `VACUUM`.
  Autovacuum with threshold-based triggering is deferred.
- **`axiom_bloat` system view:** The progreso.md description mentions a bloat view.
  Deferred — the vacuum function itself is the priority.
- **Parallel vacuum:** Single-threaded scan. No worker threads.
- **VACUUM FREEZE:** Freezing old txn_ids to prevent wraparound is Phase 34.

---

## Dependencies

- **7.3b** (lazy index deletion) — completed; generates the dead entries vacuum cleans
- **`mark_slot_dead()`** in `heap.rs` — already exists
- **`BTree::delete_in()`** — already exists
- **`HeapChain::is_slot_visible()`** — already exists
- **`TxnManager::max_committed()`** — already exists
- **SQL parser** — needs `VACUUM` keyword added
- **`Stmt` enum** — needs `Vacuum(VacuumStmt)` variant

---

## ⚠️ DEFERRED

- **Page compaction (7.11b):** Physical space reclamation by moving live tuples
  within a page and updating `free_start`/`free_end`. Requires careful handling
  of slot indices (RecordIds must not change).
- **Autovacuum (7.11c):** Background task that triggers vacuum when
  `dead_rows > threshold`. Requires connection-independent execution context.
- **`axiom_bloat` view (7.11d):** System view showing per-table dead/live/bloat
  statistics. Requires the statistics infrastructure from Phase 6.12.
- **VACUUM FULL:** Rewrite the entire table to a new heap chain, reclaiming all
  dead space. Extremely expensive but maximizes space savings.
