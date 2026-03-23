# Plan: 3.8 — Crash Recovery State Machine

## Files to create / modify

| File | Action | What |
|---|---|---|
| `crates/nexusdb-wal/src/txn.rs` | modify | `PHYSICAL_LOC_LEN`, update `record_insert/delete/update` |
| `crates/nexusdb-wal/src/recovery.rs` | create | `RecoveryOp`, `RecoveryResult`, `RecoveryState`, `CrashRecovery` |
| `crates/nexusdb-wal/src/lib.rs` | modify | Export recovery types |

## Step 1 — Physical location encoding in WAL entries (txn.rs)

### Constant

```rust
/// Bytes prepended to `new_value` (Insert/Update) and `old_value` (Delete)
/// to encode the heap physical location: [page_id: u64 LE][slot_id: u16 LE].
/// This allows crash recovery to undo operations without an in-memory undo log.
pub const PHYSICAL_LOC_LEN: usize = 10;

fn encode_physical_loc(page_id: u64, slot_id: u16) -> [u8; PHYSICAL_LOC_LEN] {
    let mut loc = [0u8; PHYSICAL_LOC_LEN];
    loc[0..8].copy_from_slice(&page_id.to_le_bytes());
    loc[8..10].copy_from_slice(&slot_id.to_le_bytes());
    loc
}

pub fn decode_physical_loc(bytes: &[u8]) -> Option<(u64, u16)> {
    if bytes.len() < PHYSICAL_LOC_LEN {
        return None;
    }
    let page_id = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let slot_id = u16::from_le_bytes([bytes[8], bytes[9]]);
    Some((page_id, slot_id))
}
```

### Updated record_insert

```rust
pub fn record_insert(&mut self, table_id, key, value, page_id, slot_id) -> Result<()> {
    let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
    let txn_id = active.txn_id;

    // Prepend physical location to new_value for crash recovery.
    let mut new_value = Vec::with_capacity(PHYSICAL_LOC_LEN + value.len());
    new_value.extend_from_slice(&encode_physical_loc(page_id, slot_id));
    new_value.extend_from_slice(value);

    let mut entry = WalEntry::new(0, txn_id, EntryType::Insert, table_id,
        key.to_vec(), vec![], new_value);
    self.wal.append(&mut entry)?;
    active.undo_ops.push(UndoOp::UndoInsert { page_id, slot_id });
    Ok(())
}
```

### Updated record_delete

```rust
pub fn record_delete(&mut self, table_id, key, old_value, page_id, slot_id) -> Result<()> {
    ...
    // Prepend physical location to old_value.
    let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + old_value.len());
    ov.extend_from_slice(&encode_physical_loc(page_id, slot_id));
    ov.extend_from_slice(old_value);

    let mut entry = WalEntry::new(0, txn_id, EntryType::Delete, table_id,
        key.to_vec(), ov, vec![]);
    ...
}
```

### Updated record_update

```rust
pub fn record_update(&mut self, table_id, key, old_value, new_value,
                     page_id, old_slot, new_slot) -> Result<()> {
    ...
    // Prepend physical locations to both sides.
    let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + old_value.len());
    ov.extend_from_slice(&encode_physical_loc(page_id, old_slot));
    ov.extend_from_slice(old_value);

    let mut nv = Vec::with_capacity(PHYSICAL_LOC_LEN + new_value.len());
    nv.extend_from_slice(&encode_physical_loc(page_id, new_slot));
    nv.extend_from_slice(new_value);

    let mut entry = WalEntry::new(0, txn_id, EntryType::Update, table_id,
        key.to_vec(), ov, nv);
    ...
}
```

**Public export**: `decode_physical_loc` and `PHYSICAL_LOC_LEN` exported from lib.rs
for use in recovery.rs.

## Step 2 — recovery.rs types

```rust
/// A single operation that needs to be undone during crash recovery.
#[derive(Debug, Clone)]
pub enum RecoveryOp {
    /// Undo an INSERT: mark the heap slot dead.
    Insert { page_id: u64, slot_id: u16 },
    /// Undo a DELETE: clear txn_id_deleted in the RowHeader.
    Delete { page_id: u64, slot_id: u16 },
}

/// Current phase of crash recovery (for observability).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryState {
    Ready,
    ScanningWal,
    UndoingInProgress,
    Verifying,
}

/// Result of a successful crash recovery.
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    /// Highest committed TxnId found in the WAL scan.
    pub max_committed: TxnId,
    /// Number of in-progress transactions that were undone.
    pub undone_txns: u32,
    /// The checkpoint LSN used as the scan start point.
    pub checkpoint_lsn: u64,
}
```

## Step 3 — CrashRecovery::is_needed

```rust
pub fn is_needed(storage: &dyn StorageEngine, wal_path: &Path) -> Result<bool, DbError> {
    let checkpoint_lsn = Checkpointer::last_checkpoint_lsn(storage)?;
    let reader = WalReader::open(wal_path)?;

    let mut committed: HashSet<u64> = HashSet::new();
    let mut rolled_back: HashSet<u64> = HashSet::new();
    let mut begun: HashSet<u64> = HashSet::new();

    for result in reader.scan_forward(checkpoint_lsn)? {
        let entry = result?;
        match entry.entry_type {
            EntryType::Begin => { begun.insert(entry.txn_id); }
            EntryType::Commit => { committed.insert(entry.txn_id); }
            EntryType::Rollback => { rolled_back.insert(entry.txn_id); }
            _ => {}
        }
    }

    // In-progress = begun but not committed and not rolled back.
    Ok(begun.iter().any(|id| !committed.contains(id) && !rolled_back.contains(id)))
}
```

## Step 4 — CrashRecovery::recover (core algorithm)

```rust
pub fn recover(
    storage: &mut dyn StorageEngine,
    wal_path: &Path,
) -> Result<RecoveryResult, DbError> {
    let checkpoint_lsn = Checkpointer::last_checkpoint_lsn(storage)?;
    let reader = WalReader::open(wal_path)?;

    // Phase: ScanningWal
    let mut committed: HashSet<u64> = HashSet::new();
    let mut rolled_back: HashSet<u64> = HashSet::new();
    // ops in chronological order per txn
    let mut in_progress_ops: HashMap<u64, Vec<RecoveryOp>> = HashMap::new();

    for result in reader.scan_forward(checkpoint_lsn)? {
        let entry = result?;
        match entry.entry_type {
            EntryType::Begin => {
                in_progress_ops.entry(entry.txn_id).or_default();
            }
            EntryType::Commit => {
                committed.insert(entry.txn_id);
                in_progress_ops.remove(&entry.txn_id);
            }
            EntryType::Rollback => {
                rolled_back.insert(entry.txn_id);
                in_progress_ops.remove(&entry.txn_id);
            }
            EntryType::Insert => {
                if let Some(ops) = in_progress_ops.get_mut(&entry.txn_id) {
                    if let Some((page_id, slot_id)) = decode_physical_loc(&entry.new_value) {
                        ops.push(RecoveryOp::Insert { page_id, slot_id });
                    }
                }
            }
            EntryType::Delete => {
                if let Some(ops) = in_progress_ops.get_mut(&entry.txn_id) {
                    if let Some((page_id, slot_id)) = decode_physical_loc(&entry.old_value) {
                        ops.push(RecoveryOp::Delete { page_id, slot_id });
                    }
                }
            }
            EntryType::Update => {
                if let Some(ops) = in_progress_ops.get_mut(&entry.txn_id) {
                    // Update = delete(old) + insert(new). Undo = reverse order.
                    if let Some((new_pid, new_slot)) = decode_physical_loc(&entry.new_value) {
                        ops.push(RecoveryOp::Insert { page_id: new_pid, slot_id: new_slot });
                    }
                    if let Some((old_pid, old_slot)) = decode_physical_loc(&entry.old_value) {
                        ops.push(RecoveryOp::Delete { page_id: old_pid, slot_id: old_slot });
                    }
                }
            }
            EntryType::Checkpoint => {} // nothing to do
        }
    }

    // Phase: UndoingInProgress
    let undone_txns = in_progress_ops.len() as u32;

    for (_txn_id, ops) in in_progress_ops {
        // Apply undo in reverse (last op first)
        for op in ops.into_iter().rev() {
            match op {
                RecoveryOp::Insert { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    // Idempotent: if already dead, skip.
                    match mark_slot_dead(&mut page, slot_id) {
                        Ok(()) | Err(DbError::AlreadyDeleted { .. }) => {}
                        Err(e) => return Err(e),
                    }
                    storage.write_page(page_id, &page)?;
                }
                RecoveryOp::Delete { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    // Idempotent: if txn_id_deleted is already 0, skip.
                    // clear_deletion returns AlreadyDeleted if slot is dead —
                    // for recovery this means the row is already restored, skip.
                    match clear_deletion(&mut page, slot_id) {
                        Ok(()) | Err(DbError::AlreadyDeleted { .. }) => {}
                        Err(e) => return Err(e),
                    }
                    storage.write_page(page_id, &page)?;
                }
            }
        }
    }

    // Flush corrections to disk.
    storage.flush()?;

    // max_committed = highest committed txn_id in WAL scan.
    let max_committed = committed.into_iter().max().unwrap_or(0);

    Ok(RecoveryResult {
        max_committed,
        undone_txns,
        checkpoint_lsn,
    })
}
```

Note: `clear_deletion` currently returns `AlreadyDeleted` when the slot is dead (offset=0,
length=0). For recovery, we treat this as "already restored" — same as idempotent.
However, `clear_deletion` doesn't handle the case where `txn_id_deleted` is already 0
(row is live, no deletion to undo). We need to add this check or make it a no-op.

**Required fix to `heap::clear_deletion`**: if slot is live and `txn_id_deleted == 0`,
return `Ok(())` (no-op) instead of an error. This makes it fully idempotent for recovery.

## Step 5 — TxnManager::open_with_recovery

```rust
/// Opens an existing WAL and runs crash recovery if needed.
/// Returns a TxnManager initialized with max_committed from the WAL.
pub fn open_with_recovery(
    storage: &mut dyn StorageEngine,
    wal_path: &Path,
) -> Result<(Self, RecoveryResult), DbError> {
    let recovery_result = CrashRecovery::recover(storage, wal_path)?;
    let wal = WalWriter::open(wal_path)?;
    let next_txn_id = recovery_result.max_committed + 1;
    Ok((
        Self {
            wal,
            next_txn_id,
            max_committed: recovery_result.max_committed,
            active: None,
        },
        recovery_result,
    ))
}
```

## Step 6 — Tests

**Unit tests in recovery.rs** (`#[cfg(test)]` with MemoryStorage + tempfile):

```rust
// Clean shutdown: is_needed returns false
fn test_is_needed_false_after_clean_commit()

// Fresh DB (no WAL entries): is_needed false, recover returns max_committed=0
fn test_recover_fresh_database()

// Crashed INSERT: slot is dead after recovery
fn test_recover_undoes_crashed_insert()

// Crashed DELETE: txn_id_deleted is 0 after recovery
fn test_recover_undoes_crashed_delete()

// Crashed UPDATE: new slot dead, old slot live
fn test_recover_undoes_crashed_update()

// Multiple ops in one crashed txn, reversed correctly
fn test_recover_multiple_ops_reversed()

// Recovery is idempotent (call twice = same result)
fn test_recover_is_idempotent()

// Recovery after rotation: starts from checkpoint_lsn, not from 0
fn test_recover_respects_checkpoint_lsn()

// max_committed set correctly from committed txn in WAL
fn test_recover_max_committed()

// open_with_recovery initializes TxnManager correctly
fn test_open_with_recovery_initializes_txn_manager()

// Crashed txn with no ops (Begin only): undone_txns=1, no-op undo
fn test_recover_begin_only_crashed_txn()
```

**Integration test** (MmapStorage + WAL file):
```rust
// Simulate crash: write to heap + WAL, drop without commit, recover, verify state
fn test_mmap_crash_recovery_integration()
```

## Anti-patterns to avoid

- **NO** removing/ignoring `AlreadyDeleted` errors in undo — treat as "already undone" (idempotent)
- **NO** `decode_physical_loc` panic on short payload — return `None` and skip (WAL may have non-DML entries or legacy entries)
- **NO** undo of committed or rolled-back txns — only in-progress ones
- **NO** changing the WAL binary format — physical location is a payload convention, not a new field
- **NO** assuming in-progress_ops is empty after a clean shutdown — always scan to be sure
- **NO** `unwrap()` in src/

## Risks

| Risk | Mitigation |
|---|---|
| WAL entry without physical loc (e.g., pre-3.8 WAL or control entries) | `decode_physical_loc` returns `None` → skip gracefully |
| `clear_deletion` on slot with txn_id_deleted=0 errors instead of no-op | Fix clear_deletion to be fully idempotent (txn_id_deleted==0 → Ok(())) |
| `mark_slot_dead` on already-dead slot → `AlreadyDeleted` during redo | Already handled: match arm ignores `AlreadyDeleted` |
| Recovery of partially-written WAL entry (CRC mismatch at crash point) | `WalReader` stops at first invalid entry → safe (partial write at end of WAL is ignored) |
| `read_page(page_id)` for a page_id that doesn't exist (e.g., storage shrunk) | Returns `PageNotFound` → propagated as error (shouldn't happen in Phase 3) |

## Implementation order

```
1. txn.rs: PHYSICAL_LOC_LEN, encode_physical_loc, decode_physical_loc
2. txn.rs: update record_insert, record_delete, record_update
3. lib.rs: export decode_physical_loc, PHYSICAL_LOC_LEN
4. heap.rs: fix clear_deletion to be idempotent (txn_id_deleted==0 → Ok(()))
5. recovery.rs: RecoveryOp, RecoveryState, RecoveryResult
6. recovery.rs: CrashRecovery::is_needed
7. recovery.rs: CrashRecovery::recover
8. txn.rs: TxnManager::open_with_recovery
9. lib.rs: export CrashRecovery, RecoveryResult, RecoveryState
10. Tests: 11 unit tests + 1 integration test
11. cargo test --workspace + clippy + fmt
```
