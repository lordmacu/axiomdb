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

The `TxnManager` is the single coordinator for all transaction state. It is shared
via `Arc<Mutex<TxnManagerInner>>` (will be replaced with a lock-free structure in
Phase 7).

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

In Phase 3, old page reclamation is deferred — old pages are freed immediately after
the root swap, and readers are serialized (single writer at a time with `&mut self`).
Phase 7 will add epoch-based reclamation for true concurrent reads and writes.

---

## Isolation Levels — Implementation

### READ COMMITTED

On every statement start within a transaction, a new snapshot is taken. The
`TransactionSnapshot` passed to the analyzer and executor is refreshed per statement.

### REPEATABLE READ

The snapshot is taken once at `BEGIN` and held for the entire transaction's lifetime.
All statements use the same snapshot.

The default isolation level is READ COMMITTED (matching MySQL's default and most common
OLTP workload requirements).

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
