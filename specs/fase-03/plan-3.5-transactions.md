# Plan: 3.5 — TxnManager

## Files to create / modify

| File | Action | What |
|---|---|---|
| `crates/nexusdb-core/src/error.rs` | modify | Add `TransactionAlreadyActive`, `NoActiveTransaction` |
| `crates/nexusdb-storage/src/heap.rs` | modify | Add `mark_slot_dead`, `clear_deletion` |
| `crates/nexusdb-wal/Cargo.toml` | modify | Add `nexusdb-storage` dependency |
| `crates/nexusdb-wal/src/txn.rs` | create | `UndoOp`, `ActiveTxn`, `TxnManager` |
| `crates/nexusdb-wal/src/lib.rs` | modify | Export `TxnManager`, `UndoOp` |

## Step 1 — New DbError variants

```rust
#[error("a transaction is already active (txn_id = {txn_id})")]
TransactionAlreadyActive { txn_id: u64 },

#[error("no active transaction — call begin() first")]
NoActiveTransaction,
```

## Step 2 — Heap helpers for undo

Add to `crates/nexusdb-storage/src/heap.rs`:

```rust
/// Marks slot `slot_id` as physically dead by zeroing its SlotEntry.
/// Used exclusively during transaction ROLLBACK to undo an INSERT.
///
/// After this call, read_tuple returns None and scan_visible skips the slot.
/// The tuple data remains in the page body (VACUUM reclaims space in Phase 7).
pub fn mark_slot_dead(page: &mut Page, slot_id: u16) -> Result<(), DbError>

/// Clears the `txn_id_deleted` field of the tuple at `slot_id` (sets it to 0).
/// Used exclusively during transaction ROLLBACK to undo a DELETE.
///
/// After this call, the row is live again and visible to future snapshots.
pub fn clear_deletion(page: &mut Page, slot_id: u16) -> Result<(), DbError>
```

Both reuse the existing `read_slot`, `write_slot` helpers and call `update_checksum()`.

## Step 3 — TxnManager (txn.rs)

### UndoOp

```rust
/// Undo operation recorded for each DML during a transaction.
/// Applied in reverse order on ROLLBACK.
#[derive(Debug, Clone)]
pub enum UndoOp {
    /// Undo an INSERT: mark the slot dead.
    UndoInsert { page_id: u64, slot_id: u16 },
    /// Undo a DELETE: clear txn_id_deleted in the RowHeader.
    UndoDelete { page_id: u64, slot_id: u16 },
    // UPDATE = UndoInsert (new slot) + UndoDelete (old slot), pushed in that order.
    // Reversed: UndoDelete first (restore old), then UndoInsert (kill new). Correct.
}
```

### ActiveTxn

```rust
struct ActiveTxn {
    txn_id: TxnId,
    /// Undo operations in chronological order; applied last-to-first on rollback.
    undo_ops: Vec<UndoOp>,
    /// snapshot_id at begin time (= max_committed_at_begin + 1).
    snapshot_id_at_begin: u64,
}
```

### TxnManager

```rust
pub struct TxnManager {
    wal: WalWriter,
    next_txn_id: u64,
    max_committed: u64,
    active: Option<ActiveTxn>,
}
```

### create / open

```rust
pub fn create(wal_path: &Path) -> Result<Self, DbError> {
    let wal = WalWriter::create(wal_path)?;
    Ok(Self { wal, next_txn_id: 1, max_committed: 0, active: None })
}

pub fn open(wal_path: &Path) -> Result<Self, DbError> {
    // Scan WAL forward to find max committed txn_id.
    // WalReader::scan_forward + filter EntryType::Commit + max(txn_id).
    let max_committed = scan_max_committed(wal_path)?;
    let next_txn_id = max_committed + 1;
    let wal = WalWriter::open(wal_path)?;
    Ok(Self { wal, next_txn_id, max_committed, active: None })
}

fn scan_max_committed(wal_path: &Path) -> Result<TxnId, DbError> {
    let reader = WalReader::open(wal_path)?;
    let mut max = 0u64;
    for entry in reader.scan_forward(0)? {
        let e = entry?;
        if e.entry_type == EntryType::Commit && e.txn_id > max {
            max = e.txn_id;
        }
    }
    Ok(max)
}
```

### begin

```rust
pub fn begin(&mut self) -> Result<TxnId, DbError> {
    if let Some(ref active) = self.active {
        return Err(DbError::TransactionAlreadyActive { txn_id: active.txn_id });
    }
    let txn_id = self.next_txn_id;
    self.next_txn_id += 1;

    let mut entry = WalEntry::new(0, txn_id, EntryType::Begin, 0, vec![], vec![], vec![]);
    self.wal.append(&mut entry)?;

    self.active = Some(ActiveTxn {
        txn_id,
        undo_ops: Vec::new(),
        snapshot_id_at_begin: self.max_committed + 1,
    });
    Ok(txn_id)
}
```

### record_insert / record_delete / record_update

```rust
pub fn record_insert(&mut self, table_id: u32, key: &[u8], value: &[u8],
                      page_id: u64, slot_id: u16) -> Result<(), DbError> {
    let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
    let txn_id = active.txn_id;

    let mut entry = WalEntry::new(0, txn_id, EntryType::Insert,
        table_id, key.to_vec(), vec![], value.to_vec());
    self.wal.append(&mut entry)?;
    active.undo_ops.push(UndoOp::UndoInsert { page_id, slot_id });
    Ok(())
}

pub fn record_delete(&mut self, table_id: u32, key: &[u8], old_value: &[u8],
                      page_id: u64, slot_id: u16) -> Result<(), DbError> {
    let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
    let txn_id = active.txn_id;

    let mut entry = WalEntry::new(0, txn_id, EntryType::Delete,
        table_id, key.to_vec(), old_value.to_vec(), vec![]);
    self.wal.append(&mut entry)?;
    active.undo_ops.push(UndoOp::UndoDelete { page_id, slot_id });
    Ok(())
}

pub fn record_update(&mut self, table_id: u32, key: &[u8],
                      old_value: &[u8], new_value: &[u8],
                      page_id: u64, old_slot: u16, new_slot: u16) -> Result<(), DbError> {
    let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
    let txn_id = active.txn_id;

    let mut entry = WalEntry::new(0, txn_id, EntryType::Update,
        table_id, key.to_vec(), old_value.to_vec(), new_value.to_vec());
    self.wal.append(&mut entry)?;
    // UPDATE = delete(old) + insert(new).
    // Undo order (last-to-first): UndoInsert(new) first, then UndoDelete(old).
    active.undo_ops.push(UndoOp::UndoInsert { page_id, slot_id: new_slot });
    active.undo_ops.push(UndoOp::UndoDelete { page_id, slot_id: old_slot });
    Ok(())
}
```

### commit

```rust
pub fn commit(&mut self) -> Result<(), DbError> {
    let active = self.active.take().ok_or(DbError::NoActiveTransaction)?;
    let txn_id = active.txn_id;

    let mut entry = WalEntry::new(0, txn_id, EntryType::Commit, 0, vec![], vec![], vec![]);
    self.wal.append(&mut entry)?;
    self.wal.commit()?;  // fsync — durability guarantee

    self.max_committed = txn_id;
    Ok(())
}
```

### rollback

```rust
pub fn rollback(&mut self, storage: &mut dyn StorageEngine) -> Result<(), DbError> {
    let active = self.active.take().ok_or(DbError::NoActiveTransaction)?;
    let txn_id = active.txn_id;

    // Write Rollback entry (informational for crash recovery — no fsync needed).
    let mut entry = WalEntry::new(0, txn_id, EntryType::Rollback, 0, vec![], vec![], vec![]);
    self.wal.append(&mut entry)?;
    // No wal.commit() here — rolled-back entries are not durable on purpose.

    // Apply undo operations in reverse (last DML first).
    for op in active.undo_ops.into_iter().rev() {
        match op {
            UndoOp::UndoInsert { page_id, slot_id } => {
                let page = storage.read_page_mut(page_id)?;  // or read+modify+write
                heap::mark_slot_dead(page, slot_id)?;
                storage.write_page(page_id, page)?;
            }
            UndoOp::UndoDelete { page_id, slot_id } => {
                let page = storage.read_page_mut(page_id)?;
                heap::clear_deletion(page, slot_id)?;
                storage.write_page(page_id, page)?;
            }
        }
    }
    // max_committed unchanged — rolled-back txn is never visible.
    Ok(())
}
```

Note: `read_page_mut` doesn't exist yet on StorageEngine. We need:
- Option A: add `read_page_mut(&mut self, page_id) -> Result<&mut Page, DbError>` to the trait
- Option B: read → clone → modify → write (avoids trait change)

**Use Option B**: read → copy bytes → modify in-place locally → write_page.
Avoids changing the StorageEngine trait (disruptive) and is correct:

```rust
UndoOp::UndoInsert { page_id, slot_id } => {
    let bytes = *storage.read_page(page_id)?.as_bytes();
    let mut page = Page::from_bytes(bytes)?;
    heap::mark_slot_dead(&mut page, slot_id)?;
    storage.write_page(page_id, &page)?;
}
```

This is one extra copy per undo op — acceptable since rollback is not the hot path.

### autocommit

```rust
pub fn autocommit<F, T>(
    &mut self,
    storage: &mut dyn StorageEngine,
    f: F,
) -> Result<T, DbError>
where
    F: FnOnce(&mut Self) -> Result<T, DbError>,
{
    self.begin()?;
    match f(self) {
        Ok(result) => {
            self.commit()?;
            Ok(result)
        }
        Err(e) => {
            // Best-effort rollback — if rollback itself fails, return original error.
            let _ = self.rollback(storage);
            Err(e)
        }
    }
}
```

### snapshot helpers

```rust
pub fn snapshot(&self) -> TransactionSnapshot {
    TransactionSnapshot::committed(self.max_committed)
}

pub fn active_snapshot(&self) -> Result<TransactionSnapshot, DbError> {
    let active = self.active.as_ref().ok_or(DbError::NoActiveTransaction)?;
    Ok(TransactionSnapshot {
        snapshot_id: active.snapshot_id_at_begin,
        current_txn_id: active.txn_id,
    })
}

pub fn max_committed(&self) -> TxnId {
    self.max_committed
}

pub fn current_lsn(&self) -> u64 {
    self.wal.current_lsn()
}
```

## Step 4 — Tests

**Unit tests in txn.rs** (`#[cfg(test)]` using tempfile):

```rust
// begin → commit advances max_committed
fn test_begin_commit_advances_max_committed()
// begin → rollback does NOT advance max_committed
fn test_begin_rollback_no_advance()
// rollback undoes INSERT: slot becomes dead
fn test_rollback_undo_insert_marks_slot_dead()
// rollback undoes DELETE: txn_id_deleted cleared
fn test_rollback_undo_delete_clears_deletion()
// rollback undoes UPDATE: new slot dead, old slot restored
fn test_rollback_undo_update()
// snapshot() returns correct snapshot_id
fn test_snapshot_returns_committed_snapshot()
// active_snapshot() includes current_txn_id
fn test_active_snapshot_has_current_txn_id()
// double begin returns TransactionAlreadyActive
fn test_double_begin_error()
// commit without begin returns NoActiveTransaction
fn test_commit_without_begin_error()
// rollback without begin returns NoActiveTransaction
fn test_rollback_without_begin_error()
// open recovers max_committed from existing WAL
fn test_open_recovers_max_committed()
// autocommit commits on Ok
fn test_autocommit_commits_on_ok()
// autocommit rollbacks on Err
fn test_autocommit_rollbacks_on_err()
// WAL entries are written in correct order (Begin, DML, Commit)
fn test_wal_entry_order()
// uncommitted row not visible via snapshot
fn test_uncommitted_not_visible()
```

## Anti-patterns to avoid

- **NO** fsync on rollback — only commit needs durability guarantee
- **NO** `read_page_mut` on StorageEngine trait — avoids breaking change; use read+copy+write
- **NO** advancing max_committed on rollback — rolled-back txns must be invisible
- **NO** `unwrap()` in src/

## Risks

| Risk | Mitigation |
|---|---|
| WalWriter BufWriter not flushed on crash | fsync in commit(); Rollback entry deliberately not fsynced |
| Undo ops applied in wrong order | Vec reversed with `.rev()` — tested explicitly |
| read+copy+write in undo is slow | Acceptable: rollback is rare, not hot path |
| nexusdb-storage circular dep with nexusdb-wal | nexusdb-storage doesn't depend on nexusdb-wal; safe |

## Implementation order

```
1. DbError: TransactionAlreadyActive, NoActiveTransaction
2. heap.rs: mark_slot_dead + clear_deletion + export
3. nexusdb-wal/Cargo.toml: add nexusdb-storage dependency
4. txn.rs: UndoOp + ActiveTxn + TxnManager struct
5. txn.rs: create/open
6. txn.rs: begin/commit/rollback/record_*
7. txn.rs: snapshot helpers + autocommit
8. lib.rs: export TxnManager
9. Tests: all 15 cases
10. cargo test --workspace + clippy + fmt
```
