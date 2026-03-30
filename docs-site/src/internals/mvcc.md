# MVCC and Transactions

Multi-Version Concurrency Control (MVCC) is AxiomDB's mechanism for allowing
concurrent readers and writers to operate without blocking each other. This page
documents the MVCC model, the RowHeader format, the visibility function, and the
TxnManager that coordinates transaction IDs and snapshots.

> **Implementation status:** The `axiomdb-mvcc` crate contains the TxnManager and
> snapshot logic implemented in Phase 3. Full SSI (Serializable Snapshot Isolation)
> and write-write conflict detection are planned for Phase 7. The documentation below
> describes the target design; sections marked ⚠️ indicate planned features.

---

## Core Concepts

### Transaction ID (TxnId)

Every transaction receives a unique, monotonically increasing `u64` identifier.
The value `0` is reserved for autocommit (single-statement transactions).

### Transaction Snapshot

A snapshot represents the set of transactions that were committed at the moment
`BEGIN` (or the first statement, in autocommit) was executed.

```rust
pub struct TransactionSnapshot {
    pub xmin: u64,    // All txn_ids < xmin are definitely committed
    pub xmax: u64,    // All txn_ids >= xmax are definitely not yet committed
    pub in_progress: Vec<u64>,  // txn_ids in [xmin, xmax) that are still active
}
```

A row version is visible to a snapshot if:
- The inserting transaction (`xmin`) is committed relative to the snapshot, AND
- The deleting transaction (`xmax`) is either `0` (the row is live) or is NOT
  committed relative to the snapshot.

---

## RowHeader — Per-Row Versioning

Every heap tuple begins with a `RowHeader`:

```text
Offset  Size  Field    Description
──────── ────── ──────── ─────────────────────────────────────────────────────
     0      8  xmin     txn_id of the transaction that inserted this row version
     8      8  xmax     txn_id of the transaction that deleted this row (0 = live)
    16      1  deleted  1 = this row has been logically deleted; 0 = live
    17      7  _pad     alignment padding to 24 bytes
Total: 24 bytes
```

The full lifecycle of a row version:

```
INSERT in txn T1:
    RowHeader { xmin: T1, xmax: 0, deleted: 0 }

DELETE in txn T2:
    RowHeader { xmin: T1, xmax: T2, deleted: 1 }

UPDATE in txn T2 (implemented as DELETE + INSERT):
    Old version: RowHeader { xmin: T1, xmax: T2, deleted: 1 }
    New version: RowHeader { xmin: T2, xmax: 0,  deleted: 0 }
```

### Batch DELETE and Full-Table DELETE

When a DELETE has a WHERE clause, `TableEngine::delete_rows_batch()` collects all
matching `(page_id, slot_id)` pairs and calls `HeapChain::delete_batch()` with them.
Each affected slot receives `xmax = txn_id` and `deleted = 1` in a single pass per
page. The WAL receives one `WalEntry::Delete` per matched row (for correct per-row
redo/undo).

When a DELETE has **no** WHERE clause or is a `TRUNCATE TABLE`, the executor takes a
different path:

1. `HeapChain::scan_rids_visible()` collects live `(page_id, slot_id)` pairs without
   decoding row data.
2. `HeapChain::delete_batch()` marks all slots dead in O(P) page I/O.
3. A **single** `WalEntry::Truncate` is appended to the WAL instead of N per-row
   Delete entries.

The MVCC visibility result is identical to the per-row path: every slot has
`xmax = txn_id` and `deleted = 1`, so any snapshot with `xmax ≤ txn_id` will see
the row as deleted after the transaction commits. Concurrent readers that took their
snapshot before this transaction began continue to see all rows as live throughout
the delete — standard snapshot isolation.

---

## Visibility Function

```rust
fn is_visible(row: &RowHeader, snap: &TransactionSnapshot, self_txn_id: u64) -> bool {
    // Rule 1: The inserting txn must be committed in our snapshot.
    if !is_committed(row.xmin, snap) && row.xmin != self_txn_id {
        return false;
    }

    // Rule 2: The deleting txn must NOT be committed in our snapshot
    // (or xmax must be 0, meaning the row is live).
    if row.xmax != 0 {
        if is_committed(row.xmax, snap) || row.xmax == self_txn_id {
            return false;
        }
    }

    true
}

fn is_committed(txn_id: u64, snap: &TransactionSnapshot) -> bool {
    if txn_id < snap.xmin { return true; }  // definitely committed before snapshot
    if txn_id >= snap.xmax { return false; } // definitely not committed yet
    !snap.in_progress.contains(&txn_id)      // check the in-progress set
}
```

---

## TxnManager

The `TxnManager` is the single coordinator for all transaction state. Read-only
operations access it via `&TxnManager` (shared ref) for snapshot creation; write
operations access it via `&mut TxnManager` for begin/commit/rollback.

```rust
pub struct TxnManager {
    next_txn_id:   AtomicU64,
    active_txns:   Mutex<HashSet<u64>>,   // set of currently running txn_ids
    committed_txns: Mutex<BTreeSet<u64>>, // set of committed txn_ids (pruned periodically)
    wal_writer:    WalWriter,
}
```

### BEGIN

```
1. Increment next_txn_id atomically → new txn_id
2. Insert txn_id into active_txns
3. Append WalEntry { type: Begin, txn_id, ... }
4. Build TransactionSnapshot:
     xmin = min of active_txns ∪ {txn_id}
     xmax = next_txn_id (snapshot taken after incrementing)
     in_progress = active_txns - {txn_id}
5. Return (txn_id, snapshot)
```

### COMMIT

```
1. Append WalEntry { type: Commit, txn_id, ... }
2. Flush WAL (fsync or group commit)
3. Remove txn_id from active_txns
4. Insert txn_id into committed_txns
```

### ROLLBACK

```
1. Append WalEntry { type: Rollback, txn_id, ... }
2. Remove txn_id from active_txns
3. Undo all mutations made by txn_id (undo pass, Phase 7)
```

---

## Copy-on-Write B+ Tree and MVCC

The B+ Tree's CoW semantics interact naturally with MVCC. When a writer creates a
new page for an insert, concurrent readers continue accessing the old tree structure
through the old root pointer they loaded at query start. The old pages are freed only
when the writer's root swap is complete AND all readers that loaded the old root have
finished.

Since Phase 7.4, old pages enter the **deferred free queue** instead of being returned
to the freelist immediately. This allows concurrent readers to continue accessing old
tree structures through their snapshot while the writer has already swapped the root.
Pages are released for reuse only when no active reader snapshot predates the free
operation.

### Lock-Free Readers (Phase 7.4)

The server wraps `Database` in `Arc<RwLock<Database>>`:

- **SELECT, SHOW, system variable queries** acquire a **read lock** (`db.read()`).
  Multiple readers execute concurrently with zero coordination.
- **INSERT, UPDATE, DELETE, DDL, BEGIN/COMMIT/ROLLBACK** acquire a **write lock**
  (`db.write()`). Only one writer at a time.
- **Readers never wait for writers.** A SELECT that starts before an INSERT commits
  sees the pre-INSERT snapshot via MVCC visibility. The writer modifies pages via
  `pwrite()` while readers access the old data through their owned `PageRef` copies.
- **Writers wait only for the write lock**, never for readers.

The read-only executor path (`execute_read_only_with_ctx`) takes `&dyn StorageEngine`
(shared ref) and `&TxnManager` (shared ref), ensuring it cannot mutate any state.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Zero-Lock Reads — Better Than PostgreSQL/InnoDB</span>
PostgreSQL requires per-page lightweight locks (LWLock) for every buffer access. InnoDB
requires per-page RwLock latches inside mini-transactions. AxiomDB readers need no
per-page locks at all — the combination of read-only mmap, owned PageRef copies, MVCC
visibility, and B-Tree CoW provides equivalent isolation with zero lock overhead per
page access.
</div>
</div>

---

## Isolation Levels — Implementation

### READ COMMITTED

On every statement start within a transaction, a new snapshot is taken. The
`TransactionSnapshot` passed to the analyzer and executor is refreshed per statement.

### REPEATABLE READ

The snapshot is taken once at `BEGIN` and held for the entire transaction's lifetime.
All statements use the same snapshot.

The default isolation level is REPEATABLE READ (matching MySQL's default). Autocommit
single-statement queries always use READ COMMITTED semantics since there is only one
statement to see.

---

## INSERT ... SELECT — Snapshot Isolation

`INSERT INTO target SELECT ... FROM source` executes the SELECT under the same
snapshot that was fixed at `BEGIN`. This is critical for correctness:

**The Halloween problem** is a classic database bug where an `INSERT ... SELECT`
on the same table re-reads rows it just inserted, causing an infinite loop (the
database inserts rows, those rows qualify the SELECT condition, they get inserted
again, ad infinitum).

AxiomDB prevents this automatically through MVCC snapshot semantics:

1. The snapshot is fixed at `BEGIN`: `snapshot_id = max_committed + 1`
2. Rows inserted by this statement get `txn_id_created = current_txn_id`
3. The MVCC visibility rule: a row is visible only if `txn_id_created < snapshot_id`
4. Since `current_txn_id ≥ snapshot_id`, newly inserted rows are **never** visible
   to the SELECT scan within the same transaction

```
Before BEGIN:    source = {row_A (xmin=1), row_B (xmin=2)}
Snapshot taken:  snapshot_id = 3

INSERT INTO source SELECT * FROM source:
  SELECT sees:  row_A (1 < 3 ✅), row_B (2 < 3 ✅)   → 2 rows
  Inserts:      row_C (xmin=3), row_D (xmin=3)        → 3 ≮ 3 ❌ not re-read
  SELECT stops:  only 2 original rows were seen

After COMMIT:    source = {row_A, row_B, row_C, row_D}  ← exactly 4 rows
```

This also means rows inserted by a **concurrent transaction** that commits after
this transaction's `BEGIN` are not seen by the SELECT — consistent snapshot
throughout the entire INSERT operation.

---

## MVCC on Secondary Indexes (Phase 7.3b)

Secondary indexes store `(key, RecordId)` pairs — they do **not** contain transaction
IDs or version information. Visibility is always determined at the heap via the row's
`txn_id_created` / `txn_id_deleted` fields.

### Lazy Index Deletion

When a row is DELETEd, non-unique secondary index entries are **not** removed. The
heap row is marked deleted (`txn_id_deleted = T`), and the index entry becomes a
"dead" entry. Readers filter dead entries via `is_slot_visible()` during index scans.

Unique, primary key, and FK auto-indexes still have their entries deleted immediately
because the B-Tree enforces key uniqueness internally.

### UPDATE and Dead Entries

When an UPDATE changes an indexed column:

- **Unique/PK/FK indexes:** old entry deleted, new entry inserted (immediate)
- **Non-unique indexes:** old entry left in place (lazy), new entry inserted

Both old and new entries coexist in the B-Tree. The old entry points to a heap row
whose values no longer match the index key; `is_slot_visible()` filters it out.

### Heap-Aware Uniqueness

When inserting into a unique index, if the key already exists, AxiomDB checks heap
visibility before raising a `UniqueViolation`. If the existing entry points to a dead
row (deleted or uncommitted), the insert proceeds — dead entries don't block re-use
of the same key value.

### HOT Optimization

If an UPDATE does not change any column that participates in any secondary index,
all index maintenance is skipped for that row — no B-Tree reads or writes. This is
inspired by PostgreSQL's Heap-Only Tuple (HOT) optimization.

### ROLLBACK Support

Every new index entry (from INSERT or UPDATE) is recorded as
`UndoOp::UndoIndexInsert` in the transaction's undo log. On ROLLBACK, these entries
are physically removed from the B-Tree. Old entries (from lazy delete) were never
removed, so they're naturally restored.

### Vacuum

Dead index entries accumulate until vacuum removes them. A dead entry is one where
`is_slot_visible(entry.rid, oldest_active_snapshot)` returns false — the pointed-to
heap row is deleted and no active snapshot can see it.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — No txn_id in Index Entries (InnoDB Model)</span>
InnoDB, PostgreSQL, DuckDB, and SQLite all keep secondary indexes free of version
information. AxiomDB follows this industry consensus: the heap is the source of truth
for row visibility, and indexes are simple key-to-RecordId mappings. This avoids 8
bytes of overhead per index entry (which would reduce ORDER_LEAF from 217 to ~190)
and simplifies the B-Tree implementation.
</div>
</div>

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Zero-Cost DELETE for Non-Unique Indexes</span>
DELETE operations on tables with non-unique secondary indexes require zero index I/O
— only the heap row is modified. InnoDB must write a delete-mark to each secondary
index entry; AxiomDB skips the index entirely. For DELETE-heavy workloads with many
non-unique indexes, this eliminates O(K × log N) B-Tree operations per deleted row
(where K is the number of indexes).
</div>
</div>

---

## VACUUM — Dead Row and Index Cleanup (Phase 7.11)

The `VACUUM` command physically removes dead rows and dead index entries:

```sql
VACUUM orders;     -- vacuum a specific table
VACUUM;            -- vacuum all tables
```

### Heap Vacuum

For each page in the heap chain, VACUUM finds slots where `txn_id_deleted != 0`
and `txn_id_deleted < oldest_safe_txn` (the deletion is committed and no active
snapshot can see it). These slots are zeroed via `mark_slot_dead()`, making them
invisible to `read_tuple()` without even reading the RowHeader.

Under the current `Arc<RwLock<Database>>` architecture, `oldest_safe_txn =
max_committed + 1` — all committed deletions are safe because no reader holds an
older snapshot.

### Index Vacuum

For each non-unique, non-FK secondary index, VACUUM performs a full B-Tree scan
and checks heap visibility for each entry. Dead entries (pointing to vacuumed or
deleted heap slots) are batch-deleted from the B-Tree.

Unique, PK, and FK index entries are skipped — they were already deleted
immediately during DML (Phase 7.3b).

### What VACUUM Does Not Do (Yet)

- **Page compaction:** dead slots are zeroed but the physical space is not
  reclaimed for new inserts. A future `VACUUM FULL` will defragment pages.
- **Automatic triggering:** VACUUM must be invoked manually via SQL. Autovacuum
  with threshold-based triggering is planned.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Slot-Level Vacuum Without Compaction</span>
PostgreSQL's lazy vacuum marks dead line pointers as <code>LP_UNUSED</code> and
updates a free space map (FSM) so the space can be reused. AxiomDB takes the
simpler first step: dead slots are zeroed (making scans faster) but the physical
space is not yet reusable. This avoids the complexity of a free space map and
RecordId stability during compaction. Full space reclamation is planned as a
separate enhancement.
</div>
</div>

---

## Epoch-Based Page Reclamation (Phase 7.8)

When a writer performs Copy-on-Write on a B-Tree node, old pages are deferred
(not immediately freed) because a concurrent reader might still reference them.
The `SnapshotRegistry` tracks which snapshots are active across all connections:

```rust
pub struct SnapshotRegistry {
    slots: Vec<AtomicU64>,  // slot[conn_id] = snapshot_id or 0
}
```

- **Register:** connection sets its slot before executing a read query
- **Unregister:** connection clears its slot after the query completes
- **oldest_active():** returns the minimum non-zero slot, or `u64::MAX` if idle

On `flush()`, the storage layer calls `release_deferred_frees(oldest_active())`
to return only pages freed before the oldest active snapshot to the freelist.
Pages freed after the oldest snapshot remain queued until all readers advance.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Atomic Slot Array (DuckDB Model)</span>
DuckDB tracks <code>lowest_active_start</code> via an active transaction list.
InnoDB uses <code>clone_oldest_view()</code> to merge all active ReadViews.
AxiomDB uses a fixed-size atomic slot array (1024 slots) — O(N) scan without
locking. Under the current RwLock model all slots are 0 during flush (writer
has exclusive access), so the behavior is identical to the previous
<code>u64::MAX</code> sentinel. The infrastructure is forward-compatible with
future concurrent reader+writer models.
</div>
</div>

---

## ⚠️ Planned: Serializable Snapshot Isolation (Phase 7)

SSI detects read-write dependencies between concurrent transactions and aborts
transactions that form a dangerous cycle. The implementation follows the algorithm
from Cahill et al. (2008):

- Each transaction tracks its `rw-antidependencies` (read sets and write sets).
- At commit time, if the dependency graph contains a dangerous cycle (two transactions
  where each reads something the other wrote), one transaction is aborted with
  `40001 serialization_failure`.

SSI provides true serializability (the strongest isolation level) with overhead
proportional to the number of concurrent transactions and conflicts, not to the
total number of rows.
