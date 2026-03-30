//! Crash recovery — detects and undoes in-progress transactions after an abrupt crash.
//!
//! ## Scope (Phase 3.8)
//!
//! Handles **process crash recovery**: undoes heap changes made by transactions
//! that had `Begin` in the WAL but no `Commit` or `Rollback`.
//!
//! Power failure recovery (redo of committed-but-not-checkpointed data) is
//! deferred to Phase 3.8b — it requires per-page LSN tracking and `restore_tuple`.
//!
//! ## How it works
//!
//! WAL entries written by `record_insert`, `record_delete`, and `record_update`
//! carry the physical heap location (`page_id`, `slot_id`) encoded in the first
//! 10 bytes of `new_value`/`old_value`. Recovery uses these to locate and undo
//! the heap changes without any in-memory state.
//!
//! ## Idempotency
//!
//! Recovery is safe to run multiple times: `mark_slot_dead` ignores already-dead
//! slots (`AlreadyDeleted` → Ok) and `clear_deletion` is a no-op when
//! `txn_id_deleted` is already 0.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use axiomdb_core::{error::DbError, TxnId};
use axiomdb_storage::{
    clear_deletion, heap_chain::HeapChain, mark_slot_dead, restore_tuple_image, Page, StorageEngine,
};

use crate::{
    checkpoint::Checkpointer, entry::EntryType, reader::WalReader, txn::decode_physical_loc,
};

// ── Types ─────────────────────────────────────────────────────────────────────

/// A single heap operation that needs to be undone during crash recovery.
#[derive(Debug, Clone)]
pub enum RecoveryOp {
    /// Undo an INSERT: mark the heap slot dead.
    Insert { page_id: u64, slot_id: u16 },
    /// Undo a DELETE: clear `txn_id_deleted` in the RowHeader (restore the row).
    Delete { page_id: u64, slot_id: u16 },
    /// Undo a stable-RID in-place update by restoring the old tuple image.
    UpdateInPlace {
        page_id: u64,
        slot_id: u16,
        old_image: Vec<u8>,
    },
    /// Undo a full-table delete: clear txn_id_deleted for all slots deleted by
    /// this transaction in the heap chain starting at root_page_id.
    Truncate { root_page_id: u64, txn_id: TxnId },
}

/// Observable phase of a crash recovery run (informational).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryState {
    Ready,
    ScanningWal,
    UndoingInProgress,
    Verifying,
}

/// Result returned by a successful crash recovery run.
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    /// Highest `TxnId` with a `Commit` entry in the WAL scan window.
    /// Used to initialise `TxnManager::max_committed` after recovery.
    pub max_committed: TxnId,
    /// Number of in-progress transactions that were undone.
    pub undone_txns: u32,
    /// The checkpoint LSN used as the scan start point (`0` = scanned from beginning).
    pub checkpoint_lsn: u64,
}

// ── CrashRecovery ─────────────────────────────────────────────────────────────

/// Stateless crash recovery executor.
///
/// All state lives in `storage` (meta page + heap pages) and the WAL file.
pub struct CrashRecovery;

impl CrashRecovery {
    /// Returns `true` if the WAL contains any in-progress transaction that needs undoing.
    ///
    /// This is a fast check — returns `false` immediately for a cleanly-closed database,
    /// avoiding the cost of a full recovery scan.
    pub fn is_needed(storage: &dyn StorageEngine, wal_path: &Path) -> Result<bool, DbError> {
        let checkpoint_lsn = Checkpointer::last_checkpoint_lsn(storage)?;
        let reader = WalReader::open(wal_path)?;

        let mut begun: HashSet<u64> = HashSet::new();
        let mut ended: HashSet<u64> = HashSet::new(); // committed or rolled back

        for result in reader.scan_forward(checkpoint_lsn)? {
            match result {
                Ok(entry) => match entry.entry_type {
                    EntryType::Begin => {
                        begun.insert(entry.txn_id);
                    }
                    EntryType::Commit | EntryType::Rollback => {
                        ended.insert(entry.txn_id);
                    }
                    // PageWrite behaves like Insert for is_needed: it means
                    // the txn modified heap pages and may need undoing.
                    EntryType::Insert
                    | EntryType::Delete
                    | EntryType::Update
                    | EntryType::UpdateInPlace
                    | EntryType::Truncate
                    | EntryType::PageWrite
                    | EntryType::Checkpoint => {}
                },
                // Truncated or corrupt entry at the end of WAL (e.g. process crashed
                // mid-write). Treat as end-of-valid-WAL — stop scanning.
                Err(DbError::WalEntryTruncated { .. } | DbError::WalChecksumMismatch { .. }) => {
                    break;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(begun.iter().any(|id| !ended.contains(id)))
    }

    /// Runs crash recovery: scans the WAL, undoes in-progress transactions, and
    /// flushes the corrected heap pages to disk.
    ///
    /// Safe to call on an already-consistent database — all operations are idempotent.
    /// Returns a [`RecoveryResult`] for the caller to initialise `TxnManager`.
    pub fn recover(
        storage: &mut dyn StorageEngine,
        wal_path: &Path,
    ) -> Result<RecoveryResult, DbError> {
        let checkpoint_lsn = Checkpointer::last_checkpoint_lsn(storage)?;
        let reader = WalReader::open(wal_path)?;

        // ── Phase: ScanningWal ────────────────────────────────────────────────

        let mut committed: HashSet<u64> = HashSet::new();
        // in_progress: txn_id → ops in chronological order (will be reversed on undo)
        let mut in_progress: HashMap<u64, Vec<RecoveryOp>> = HashMap::new();

        for result in reader.scan_forward(checkpoint_lsn)? {
            let entry = match result {
                Ok(e) => e,
                // Truncated or corrupt entry at end of WAL — stop scanning.
                Err(DbError::WalEntryTruncated { .. } | DbError::WalChecksumMismatch { .. }) => {
                    break;
                }
                Err(e) => return Err(e),
            };
            match entry.entry_type {
                EntryType::Begin => {
                    in_progress.entry(entry.txn_id).or_default();
                }
                EntryType::Commit => {
                    committed.insert(entry.txn_id);
                    in_progress.remove(&entry.txn_id);
                }
                EntryType::Rollback => {
                    // Already rolled back — no undo needed.
                    in_progress.remove(&entry.txn_id);
                }
                EntryType::Insert => {
                    if let Some(ops) = in_progress.get_mut(&entry.txn_id) {
                        // new_value = [page_id:8][slot_id:2][row data...]
                        if let Some((page_id, slot_id)) = decode_physical_loc(&entry.new_value) {
                            ops.push(RecoveryOp::Insert { page_id, slot_id });
                        }
                        // If decode fails (legacy entry), skip gracefully.
                    }
                }
                EntryType::Delete => {
                    if let Some(ops) = in_progress.get_mut(&entry.txn_id) {
                        // old_value = [page_id:8][slot_id:2][original row data...]
                        if let Some((page_id, slot_id)) = decode_physical_loc(&entry.old_value) {
                            ops.push(RecoveryOp::Delete { page_id, slot_id });
                        }
                    }
                }
                EntryType::Update => {
                    // Update = delete(old) + insert(new).
                    // Undo order (reversed): UndoInsert(new_slot) then UndoDelete(old_slot).
                    if let Some(ops) = in_progress.get_mut(&entry.txn_id) {
                        if let Some((new_pid, new_slot)) = decode_physical_loc(&entry.new_value) {
                            ops.push(RecoveryOp::Insert {
                                page_id: new_pid,
                                slot_id: new_slot,
                            });
                        }
                        if let Some((old_pid, old_slot)) = decode_physical_loc(&entry.old_value) {
                            ops.push(RecoveryOp::Delete {
                                page_id: old_pid,
                                slot_id: old_slot,
                            });
                        }
                    }
                }
                EntryType::UpdateInPlace => {
                    if let Some(ops) = in_progress.get_mut(&entry.txn_id) {
                        if let Some((page_id, slot_id)) = decode_physical_loc(&entry.old_value) {
                            ops.push(RecoveryOp::UpdateInPlace {
                                page_id,
                                slot_id,
                                old_image: entry.old_value[crate::txn::PHYSICAL_LOC_LEN..].to_vec(),
                            });
                        }
                    }
                }
                EntryType::Checkpoint => {} // no heap changes to undo
                EntryType::PageWrite => {
                    // Compact new_value layout (no page bytes stored):
                    //   [0..2]           num_slots as u16 LE
                    //   [2..2+N*2]       slot_id × N as u16 LE each
                    //
                    // For an uncommitted PageWrite we undo at slot granularity:
                    // mark each embedded slot dead, identical to undoing N Insert entries.
                    if let Some(ops) = in_progress.get_mut(&entry.txn_id) {
                        // key = page_id as 8 bytes LE
                        if entry.key.len() < 8 {
                            continue; // malformed entry — skip gracefully
                        }
                        let page_id =
                            u64::from_le_bytes(entry.key[..8].try_into().unwrap_or([0u8; 8]));

                        let nv = &entry.new_value;
                        if nv.len() < 2 {
                            continue; // truncated entry — skip gracefully
                        }
                        let num_slots = u16::from_le_bytes([nv[0], nv[1]]) as usize;

                        let slots_bytes = &nv[2..];
                        for i in 0..num_slots {
                            let off = i * 2;
                            if off + 2 > slots_bytes.len() {
                                break; // partial slot list — stop
                            }
                            let slot_id =
                                u16::from_le_bytes([slots_bytes[off], slots_bytes[off + 1]]);
                            ops.push(RecoveryOp::Insert { page_id, slot_id });
                        }
                    }
                }
                EntryType::Truncate => {
                    if let Some(ops) = in_progress.get_mut(&entry.txn_id) {
                        // key = root_page_id as 8 bytes LE
                        if entry.key.len() >= 8 {
                            let root_page_id =
                                u64::from_le_bytes(entry.key[..8].try_into().unwrap());
                            ops.push(RecoveryOp::Truncate {
                                root_page_id,
                                txn_id: entry.txn_id,
                            });
                        }
                    }
                }
            }
        }

        // ── Phase: UndoingInProgress ──────────────────────────────────────────

        let undone_txns = in_progress.len() as u32;

        for (_txn_id, ops) in in_progress {
            // Apply undo in reverse: last op first.
            for op in ops.into_iter().rev() {
                match op {
                    RecoveryOp::Insert { page_id, slot_id } => {
                        let bytes = *storage.read_page(page_id)?.as_bytes();
                        let mut page = Page::from_bytes(bytes)?;
                        match mark_slot_dead(&mut page, slot_id) {
                            Ok(()) | Err(DbError::AlreadyDeleted { .. }) => {}
                            Err(e) => return Err(e),
                        }
                        storage.write_page(page_id, &page)?;
                    }
                    RecoveryOp::Delete { page_id, slot_id } => {
                        let bytes = *storage.read_page(page_id)?.as_bytes();
                        let mut page = Page::from_bytes(bytes)?;
                        match clear_deletion(&mut page, slot_id) {
                            Ok(()) | Err(DbError::AlreadyDeleted { .. }) => {}
                            Err(e) => return Err(e),
                        }
                        storage.write_page(page_id, &page)?;
                    }
                    RecoveryOp::UpdateInPlace {
                        page_id,
                        slot_id,
                        old_image,
                    } => {
                        let bytes = *storage.read_page(page_id)?.as_bytes();
                        let mut page = Page::from_bytes(bytes)?;
                        restore_tuple_image(&mut page, slot_id, &old_image)?;
                        storage.write_page(page_id, &page)?;
                    }
                    RecoveryOp::Truncate {
                        root_page_id,
                        txn_id,
                    } => {
                        HeapChain::clear_deletions_by_txn(storage, root_page_id, txn_id)?;
                    }
                }
            }
        }

        // Flush all corrected pages to disk — makes recovery durable.
        storage.flush()?;

        let max_committed = committed.into_iter().max().unwrap_or(0);

        Ok(RecoveryResult {
            max_committed,
            undone_txns,
            checkpoint_lsn,
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_storage::{
        insert_tuple, read_tuple, read_tuple_image, rewrite_tuple_same_slot, MemoryStorage,
        MmapStorage, Page, PageType,
    };

    use crate::{TxnManager, WalWriter};

    fn temp_setup() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        (dir, path)
    }

    fn fresh_data_page(storage: &mut MemoryStorage) -> u64 {
        storage.alloc_page(PageType::Data).unwrap()
    }

    // ── is_needed ─────────────────────────────────────────────────────────────

    #[test]
    fn test_is_needed_false_after_clean_commit() {
        let (_dir, wal) = temp_setup();
        let storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal).unwrap();

        mgr.begin().unwrap();
        mgr.commit().unwrap();

        assert!(!CrashRecovery::is_needed(&storage, &wal).unwrap());
    }

    #[test]
    fn test_is_needed_true_after_crash() {
        let (_dir, wal) = temp_setup();
        let storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal).unwrap();

        mgr.begin().unwrap();
        // Simulate crash: no commit
        drop(mgr);

        assert!(CrashRecovery::is_needed(&storage, &wal).unwrap());
    }

    #[test]
    fn test_is_needed_false_fresh_database() {
        let (_dir, wal) = temp_setup();
        let storage = MemoryStorage::new();
        WalWriter::create(&wal).unwrap();
        assert!(!CrashRecovery::is_needed(&storage, &wal).unwrap());
    }

    // ── recover: basic cases ──────────────────────────────────────────────────

    #[test]
    fn test_recover_fresh_database() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        WalWriter::create(&wal).unwrap();

        let result = CrashRecovery::recover(&mut storage, &wal).unwrap();
        assert_eq!(result.max_committed, 0);
        assert_eq!(result.undone_txns, 0);
    }

    #[test]
    fn test_recover_max_committed_from_wal() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal).unwrap();

        mgr.begin().unwrap(); // txn 1
        mgr.commit().unwrap();
        mgr.begin().unwrap(); // txn 2
        mgr.commit().unwrap();

        let result = CrashRecovery::recover(&mut storage, &wal).unwrap();
        assert_eq!(result.max_committed, 2);
        assert_eq!(result.undone_txns, 0);
    }

    // ── recover: undo INSERT ──────────────────────────────────────────────────

    #[test]
    fn test_recover_undoes_crashed_insert() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        let page_id = fresh_data_page(&mut storage);
        let mut mgr = TxnManager::create(&wal).unwrap();

        let txn_id = mgr.begin().unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"data", txn_id).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"key", b"data", page_id, slot_id)
            .unwrap();
        // Simulate crash: drop flushes the BufWriter to disk without fsync.
        drop(mgr);

        let result = CrashRecovery::recover(&mut storage, &wal).unwrap();
        assert_eq!(result.undone_txns, 1);

        // The slot must be dead after recovery.
        let page = storage.read_page(page_id).unwrap();
        assert!(
            read_tuple(&page, slot_id).unwrap().is_none(),
            "crashed INSERT slot must be dead after recovery"
        );
    }

    // ── recover: undo DELETE ──────────────────────────────────────────────────

    #[test]
    fn test_recover_undoes_crashed_delete() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        let page_id = fresh_data_page(&mut storage);
        let mut mgr = TxnManager::create(&wal).unwrap();

        // Commit an INSERT (txn 1).
        let txn1 = mgr.begin().unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"row", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"k", b"row", page_id, slot_id)
            .unwrap();
        mgr.commit().unwrap();

        // Crash during DELETE (txn 2).
        let txn2 = mgr.begin().unwrap();
        {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            axiomdb_storage::delete_tuple(&mut p, slot_id, txn2).unwrap();
            storage.write_page(page_id, &p).unwrap();
        }
        mgr.record_delete(1, b"k", b"row", page_id, slot_id)
            .unwrap();
        drop(mgr); // simulate crash

        CrashRecovery::recover(&mut storage, &wal).unwrap();

        // txn_id_deleted must be cleared — row is live again.
        let page = storage.read_page(page_id).unwrap();
        let (hdr, _) = read_tuple(&page, slot_id).unwrap().unwrap();
        assert_eq!(hdr.txn_id_deleted, 0, "crashed DELETE must be undone");
    }

    // ── recover: undo UPDATE ──────────────────────────────────────────────────

    #[test]
    fn test_recover_undoes_crashed_update() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        let page_id = fresh_data_page(&mut storage);
        let mut mgr = TxnManager::create(&wal).unwrap();

        // Commit original row (txn 1).
        let txn1 = mgr.begin().unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let old_slot = insert_tuple(&mut page, b"original", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"k", b"original", page_id, old_slot)
            .unwrap();
        mgr.commit().unwrap();

        // Crash during UPDATE (txn 2).
        let txn2 = mgr.begin().unwrap();
        {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            let new_slot =
                axiomdb_storage::update_tuple(&mut p, old_slot, b"updated", txn2).unwrap();
            storage.write_page(page_id, &p).unwrap();
            mgr.record_update(
                1,
                b"k",
                b"original",
                b"updated",
                page_id,
                old_slot,
                new_slot,
            )
            .unwrap();
        }
        drop(mgr); // simulate crash

        CrashRecovery::recover(&mut storage, &wal).unwrap();

        let page = storage.read_page(page_id).unwrap();
        // Old slot: live again (txn_id_deleted cleared).
        let (old_hdr, _) = read_tuple(&page, old_slot).unwrap().unwrap();
        assert_eq!(old_hdr.txn_id_deleted, 0, "old row must be restored");
        // New slot: dead.
        let new_slot = old_slot + 1;
        assert!(
            read_tuple(&page, new_slot).unwrap().is_none(),
            "new slot from crashed UPDATE must be dead"
        );
    }

    #[test]
    fn test_recover_undoes_crashed_update_in_place() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        let page_id = fresh_data_page(&mut storage);
        let mut mgr = TxnManager::create(&wal).unwrap();

        let txn1 = mgr.begin().unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"original", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"k", b"original", page_id, slot_id)
            .unwrap();
        mgr.commit().unwrap();

        let txn2 = mgr.begin().unwrap();
        {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            let old_image = rewrite_tuple_same_slot(&mut p, slot_id, b"updated", txn2)
                .unwrap()
                .unwrap();
            let new_image = read_tuple_image(&p, slot_id).unwrap().unwrap();
            storage.write_page(page_id, &p).unwrap();
            mgr.record_update_in_place(1, b"k", &old_image, &new_image, page_id, slot_id)
                .unwrap();
        }
        drop(mgr);

        CrashRecovery::recover(&mut storage, &wal).unwrap();

        let page = storage.read_page(page_id).unwrap();
        let (hdr, data) = read_tuple(&page, slot_id).unwrap().unwrap();
        assert_eq!(data, b"original");
        assert_eq!(hdr.txn_id_created, 1);
        assert_eq!(hdr.txn_id_deleted, 0);
        assert_eq!(hdr.row_version, 0);
    }

    // ── recover: idempotency ──────────────────────────────────────────────────

    #[test]
    fn test_recover_is_idempotent() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        let page_id = fresh_data_page(&mut storage);
        let mut mgr = TxnManager::create(&wal).unwrap();

        let txn_id = mgr.begin().unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"x", txn_id).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"k", b"x", page_id, slot_id).unwrap();
        drop(mgr); // simulate crash

        // First recovery.
        CrashRecovery::recover(&mut storage, &wal).unwrap();
        // Second recovery — must not panic or error.
        // The WAL still has the in-progress txn (no Rollback written), so
        // undone_txns = 1 again, but all undo ops are no-ops (slot already dead).
        let result2 = CrashRecovery::recover(&mut storage, &wal).unwrap();
        assert_eq!(result2.undone_txns, 1); // txn still in WAL, but undo is idempotent
    }

    // ── recover: multiple ops ─────────────────────────────────────────────────

    #[test]
    fn test_recover_multiple_ops_reversed() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        let page_id = fresh_data_page(&mut storage);
        let mut mgr = TxnManager::create(&wal).unwrap();

        // Commit a row to delete later.
        let txn1 = mgr.begin().unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let del_slot = insert_tuple(&mut page, b"deleteme", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(1, b"d", b"deleteme", page_id, del_slot)
            .unwrap();
        mgr.commit().unwrap();

        // Crash during txn2: INSERT row1, DELETE del_slot.
        let txn2 = mgr.begin().unwrap();
        {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            let ins_slot = insert_tuple(&mut p, b"newrow", txn2).unwrap();
            axiomdb_storage::delete_tuple(&mut p, del_slot, txn2).unwrap();
            storage.write_page(page_id, &p).unwrap();
            mgr.record_insert(1, b"n", b"newrow", page_id, ins_slot)
                .unwrap();
            mgr.record_delete(1, b"d", b"deleteme", page_id, del_slot)
                .unwrap();
        }
        drop(mgr); // crash

        CrashRecovery::recover(&mut storage, &wal).unwrap();

        let page = storage.read_page(page_id).unwrap();
        // del_slot must be live again (undo of Delete).
        let (hdr, _) = read_tuple(&page, del_slot).unwrap().unwrap();
        assert_eq!(hdr.txn_id_deleted, 0);
        // ins_slot (= del_slot + 1) must be dead (undo of Insert).
        let ins_slot = del_slot + 1;
        assert!(read_tuple(&page, ins_slot).unwrap().is_none());
    }

    // ── recover: respects checkpoint_lsn ─────────────────────────────────────

    #[test]
    fn test_recover_respects_checkpoint_lsn() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal).unwrap();

        // Commit txn 1, then checkpoint.
        mgr.begin().unwrap();
        mgr.commit().unwrap();
        mgr.rotate_wal(&mut storage, &wal).unwrap(); // checkpoint embedded in rotation

        // Crash txn 2 (in new WAL segment after rotation).
        mgr.begin().unwrap();
        drop(mgr); // crash

        let result = CrashRecovery::recover(&mut storage, &wal).unwrap();
        // Only txn 2 is in-progress (txn 1 is before checkpoint).
        assert_eq!(result.undone_txns, 1);
    }

    // ── open_with_recovery ────────────────────────────────────────────────────

    #[test]
    fn test_open_with_recovery_initializes_txn_manager() {
        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal).unwrap();

        // Commit txn 1.
        mgr.begin().unwrap();
        mgr.commit().unwrap();
        // Crash txn 2.
        mgr.begin().unwrap();
        drop(mgr);

        let (mgr2, result) = TxnManager::open_with_recovery(&mut storage, &wal).unwrap();
        assert_eq!(result.max_committed, 1);
        assert_eq!(result.undone_txns, 1);
        // TxnManager should start with max_committed = 1, next txn = 2.
        assert_eq!(mgr2.max_committed(), 1);
    }

    // ── MmapStorage integration ───────────────────────────────────────────────

    #[test]
    fn test_mmap_crash_recovery_integration() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let wal_path = dir.path().join("test.wal");

        // Session 1: insert a row, crash before commit.
        let crashed_slot = {
            let mut storage = MmapStorage::create(&db_path).unwrap();
            let mut mgr = TxnManager::create(&wal_path).unwrap();

            let page_id = storage.alloc_page(PageType::Data).unwrap();
            let txn_id = mgr.begin().unwrap();
            let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut page = Page::from_bytes(page_bytes).unwrap();
            let slot_id = insert_tuple(&mut page, b"crash row", txn_id).unwrap();
            storage.write_page(page_id, &page).unwrap();
            storage.flush().unwrap(); // ensure page is on disk
            mgr.record_insert(1, b"k", b"crash row", page_id, slot_id)
                .unwrap();
            // Flush WAL buffer to OS (simulates kernel flushing on process exit).
            // Not fsynced — a real crash would not guarantee durability.
            mgr.wal_mut().flush_buffer().unwrap();
            drop(mgr);
            (page_id, slot_id)
        };

        // Session 2: recovery.
        {
            let mut storage = MmapStorage::open(&db_path).unwrap();
            let (page_id, slot_id) = crashed_slot;

            let result = CrashRecovery::recover(&mut storage, &wal_path).unwrap();
            assert_eq!(result.undone_txns, 1);

            // The slot must be dead (insert was rolled back by recovery).
            let page = storage.read_page(page_id).unwrap();
            assert!(
                read_tuple(&page, slot_id).unwrap().is_none(),
                "crashed insert must be dead after mmap recovery"
            );
        }
    }

    // ── recover: undo TRUNCATE ────────────────────────────────────────────────

    /// Verifies that a crash during a Truncate (no commit) is fully undone by
    /// crash recovery — all rows that were logically deleted are restored.
    #[test]
    fn test_crash_during_truncate_recovers_rows() {
        use axiomdb_core::TransactionSnapshot;
        use axiomdb_storage::{heap_chain::HeapChain, PageType};

        let (_dir, wal) = temp_setup();
        let mut storage = MemoryStorage::new();

        // Allocate root heap page and seed it.
        let root_page_id = storage.alloc_page(PageType::Data).unwrap();
        let init_page = Page::new(PageType::Data, root_page_id);
        storage.write_page(root_page_id, &init_page).unwrap();

        let mut mgr = TxnManager::create(&wal).unwrap();

        // Txn 1: insert 5 rows + commit.
        let txn1 = mgr.begin().unwrap();
        for i in 0u8..5 {
            HeapChain::insert(&mut storage, root_page_id, &[i; 8], txn1).unwrap();
        }
        mgr.commit().unwrap();

        // Txn 2: delete_batch + record_truncate — then CRASH (no commit).
        let txn2 = mgr.begin().unwrap();
        let snap = mgr.active_snapshot().unwrap();
        let raw_rids = HeapChain::scan_rids_visible(&mut storage, root_page_id, snap).unwrap();
        HeapChain::delete_batch(&mut storage, root_page_id, &raw_rids, txn2).unwrap();
        mgr.record_truncate(1, root_page_id).unwrap();
        // Flush WAL buffer to disk (simulate kernel flush on crash).
        mgr.wal_mut().flush_buffer().unwrap();
        drop(mgr); // crash — no commit

        // Recovery must undo the truncate.
        let result = CrashRecovery::recover(&mut storage, &wal).unwrap();
        assert_eq!(result.undone_txns, 1, "one in-progress txn must be undone");

        // After recovery: all 5 rows must be visible to a committed snapshot.
        let snap_after = TransactionSnapshot::committed(result.max_committed);
        let visible = HeapChain::scan_rids_visible(&mut storage, root_page_id, snap_after).unwrap();
        assert_eq!(
            visible.len(),
            5,
            "all 5 rows must be visible after crash recovery of truncate"
        );
    }
}
