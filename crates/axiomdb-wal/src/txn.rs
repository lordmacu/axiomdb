//! Transaction manager — coordinates BEGIN / COMMIT / ROLLBACK.
//!
//! ## Responsibilities
//!
//! - Assigns globally monotonic [`TxnId`]s.
//! - Buffers WAL entries for the active transaction (fsynced only on COMMIT).
//! - Maintains an **undo log** per transaction: each DML records the inverse
//!   operation needed to restore the heap pages if the transaction is rolled back.
//! - Tracks `max_committed` — the TxnId of the last committed transaction.
//!   Used to construct [`TransactionSnapshot`]s for MVCC visibility checks.
//!
//! ## Single-writer constraint (Phase 3)
//!
//! At most one explicit transaction can be active at a time.
//! Concurrent readers use [`TxnManager::snapshot`] — which requires no locking
//! because `max_committed` only advances on commit, which requires `&mut self`.
//!
//! ## Autocommit
//!
//! Use [`TxnManager::autocommit`] to wrap a single operation in an implicit
//! BEGIN / COMMIT (with automatic ROLLBACK on error).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot, TxnId};
use axiomdb_storage::{
    clear_deletion, clustered_tree, heap_chain::HeapChain, mark_slot_dead, restore_tuple_image,
    Page, StorageEngine, WalDurabilityPolicy,
};

use crate::{
    checkpoint::Checkpointer,
    clustered::{ClusteredFieldPatchEntry, ClusteredRowImage, FieldDelta},
    concurrent_writer::ConcurrentWalWriter,
    entry::{EntryType, WalEntry},
    reader::WalReader,
    recovery::{CrashRecovery, RecoveryResult},
};

// ── Savepoint ─────────────────────────────────────────────────────────────────

/// An in-memory statement-level savepoint.
///
/// Created by [`TxnManager::savepoint`] before executing a statement inside an
/// explicit transaction. Passing it to [`TxnManager::rollback_to_savepoint`]
/// undoes only that statement's writes, leaving the transaction active.
///
/// Savepoints are **not persisted** to the WAL — they are valid only within the
/// lifetime of the current `TxnManager` instance. Crash recovery handles
/// transactions at transaction granularity (full redo/undo), not statement level.
///
/// In Phase 5.16 the savepoint also records the length of the deferred-free
/// page queue so that bulk-empty pages allocated after the savepoint are
/// discarded (not freed) on `rollback_to_savepoint`.
#[derive(Debug, Clone, Copy)]
pub struct Savepoint {
    /// Index into `ActiveTxn::undo_ops` at savepoint creation time.
    pub(crate) undo_len: usize,
    /// Length of `ActiveTxn::deferred_free_pages` at savepoint creation time.
    pub(crate) deferred_free_len: usize,
    /// Length of `LocalPageBatch::freed` at savepoint creation time.
    /// Used by `rollback_to_savepoint` to truncate freed-after-savepoint entries.
    pub(crate) batch_freed_len: usize,
}

// ── UndoOp ───────────────────────────────────────────────────────────────────

/// A single undo operation recorded for each DML within a transaction.
///
/// Applied in **reverse chronological order** on ROLLBACK to restore the
/// heap pages to their pre-transaction state.
#[derive(Debug, Clone)]
pub enum UndoOp {
    /// Undo an INSERT: zero out the slot entry so the row becomes dead.
    UndoInsert { page_id: u64, slot_id: u16 },
    /// Undo a DELETE: clear `txn_id_deleted` in the RowHeader (row is live again).
    UndoDelete { page_id: u64, slot_id: u16 },
    /// Undo a stable-RID in-place update by restoring the previous tuple image.
    UndoUpdateInPlace {
        page_id: u64,
        slot_id: u16,
        old_image: Vec<u8>,
    },
    // UPDATE is recorded as UndoInsert(new_slot) + UndoDelete(old_slot).
    // Reversed: UndoDelete(old_slot) runs first (restores old), then
    // UndoInsert(new_slot) (kills the replacement). Correct MVCC undo.
    /// Undo a full-table delete: scan the heap chain and clear txn_id_deleted
    /// for every slot deleted by this transaction.
    UndoTruncate { root_page_id: u64 },
    /// Undo an index INSERT: remove the entry from the B-Tree (Phase 7.3b).
    ///
    /// Recorded when INSERT or UPDATE adds a new secondary index entry.
    /// On ROLLBACK, the entry is deleted from the B-Tree so the index
    /// returns to its pre-transaction state. The `root_page_id` is captured
    /// at recording time to avoid catalog lookups during undo.
    UndoIndexInsert {
        index_id: u32,
        root_page_id: u64,
        key: Vec<u8>,
    },
    /// Undo an index DELETE by restoring the removed entry to the B-Tree.
    ///
    /// Recorded when UPDATE rewrites a secondary key and must remove the
    /// previous physical entry immediately.
    UndoIndexDelete {
        index_id: u32,
        root_page_id: u64,
        key: Vec<u8>,
        rid: RecordId,
        fillfactor: u8,
    },
    /// Undo a clustered insert by removing the inserted row by primary key.
    UndoClusteredInsert { table_id: u32, key: Vec<u8> },
    /// Restore the exact previous clustered row image by primary key.
    UndoClusteredRestore {
        table_id: u32,
        key: Vec<u8>,
        old_row: ClusteredRowImage,
    },
    /// Lightweight undo for clustered delete-mark: clear txn_id_deleted to 0.
    /// InnoDB-inspired: only stores PK key, no full row image.
    /// On rollback: descend to leaf, find cell by key, patch txn_id_deleted = 0.
    UndoClusteredUndelete { table_id: u32, key: Vec<u8> },
    /// Undo a zero-alloc in-place field patch by reversing each field delta.
    /// InnoDB-inspired: writes back only the changed bytes (old_bytes per delta)
    /// instead of restoring a full row image. O(fields_changed) not O(row_size).
    UndoClusteredFieldPatch {
        table_id: u32,
        key: Vec<u8>,
        old_header: axiomdb_storage::RowHeader,
        /// Each delta carries the offset within row_data and the original bytes.
        field_deltas: Vec<FieldDelta>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexUndoRecord {
    DeleteInserted {
        index_id: u32,
        root_page_id: u64,
        key: Vec<u8>,
    },
    RestoreDeleted {
        index_id: u32,
        root_page_id: u64,
        key: Vec<u8>,
        rid: RecordId,
        fillfactor: u8,
    },
}

// ── LocalPageBatch (re-exported from axiomdb-storage) ──────────────────────
//
// The struct lives in `axiomdb-storage::local_page_batch` so that hot-path
// functions in axiomdb-storage and axiomdb-index can accept it without a
// dependency cycle. Re-export here for convenience.
pub use axiomdb_storage::LocalPageBatch;

// ── ConnectionTxn ────────────────────────────────────────────────────────────

/// Per-connection transaction state returned by [`TxnManager::begin`].
///
/// Holds the undo log, WAL scratch buffer, and all mutable state that belongs
/// to a single in-flight transaction. Passed by `&mut` to all `record_*`
/// methods and consumed by `commit()` / `rollback()`.
///
/// Moving per-txn state out of `TxnManager` is the first step toward
/// multi-writer support (Phase 40.10): each connection owns its
/// `ConnectionTxn` independently of others.
#[derive(Debug)]
pub struct ConnectionTxn {
    /// Globally monotonic transaction identifier.
    pub txn_id: TxnId,
    /// Snapshot id captured at BEGIN — used for read-your-own-writes.
    pub snapshot_id_at_begin: u64,
    /// Isolation level controlling snapshot freshness.
    pub isolation_level: axiomdb_core::IsolationLevel,
    /// Undo ops in chronological order; applied last-to-first on rollback.
    pub undo_ops: Vec<UndoOp>,
    /// Pages to free **after** this transaction is durably committed.
    pub deferred_free_pages: Vec<u64>,
    /// Per-savepoint stack (for internal use).
    pub savepoints: Vec<Savepoint>,
    /// Latest clustered root per table touched by this transaction.
    pub clustered_roots: HashMap<u32, u64>,
    /// Frozen active-txn set at BEGIN for RR/Serializable (excludes self).
    /// `None` for READ COMMITTED (fresh snapshot per statement).
    pub(crate) active_ids_at_begin: Option<Arc<HashSet<TxnId>>>,
    /// Reusable WAL scratch buffer (per-connection, zero contention).
    pub(crate) wal_scratch: Vec<u8>,
    /// Phase 40.9: per-connection page allocation batch. Pre-allocated page
    /// IDs are drawn from here instead of the global `Mutex<FreeList>`.
    /// Drained on commit / rollback.
    pub local_page_batch: LocalPageBatch,
    /// Copied from `TxnManager::deferred_commit_mode` at BEGIN time.
    pub(crate) deferred_commit_mode: bool,
    /// Set by `commit()` in deferred mode. Taken by `take_pending_deferred_commit()`.
    pub(crate) pending_deferred_txn_id: Option<TxnId>,
    /// Attack 6 (perf-sqlite-gap deferred-fsync): per-transaction override
    /// of the WAL durability policy. When `Some`, replaces the instance-
    /// wide `TxnManager.durability_policy` at commit time. Used by the SQL
    /// layer to honor session-level `SET synchronous = '<value>'` without
    /// mutating the instance default (which is shared across all sessions
    /// of a `Db`). `None` = use the instance policy. Mirrors SQLite's
    /// per-pager `pDb->safety_level` (research/sqlite/src/pager.c:3590).
    pub durability_override: Option<WalDurabilityPolicy>,
}

impl ConnectionTxn {
    /// Returns the transaction's id.
    pub fn txn_id(&self) -> TxnId {
        self.txn_id
    }

    /// Takes and returns the pending deferred commit txn_id, if any.
    ///
    /// Returns `Some(txn_id)` if the last `commit()` was a DML transaction in
    /// deferred mode (the Commit entry is in the BufWriter but not fsynced).
    pub fn take_pending_deferred_commit(&mut self) -> Option<TxnId> {
        self.pending_deferred_txn_id.take()
    }
}

// ── PostCommitBatches (Phase 40.10) ──────────────────────────────────────────

/// Post-commit housekeeping data protected by a single `Mutex`.
///
/// Contains page-free batches and page-batch draining data that accumulate
/// during `commit()` and are drained asynchronously afterward. Grouped under
/// one lock to avoid per-Vec Mutex overhead — the critical section is O(push).
#[derive(Debug, Default)]
pub(crate) struct PostCommitBatches {
    /// Pages waiting to be freed after their transaction is durably committed.
    pub(crate) committed_free_batches: Vec<(TxnId, Vec<u64>)>,
    /// Phase 40.9: leftover `LocalPageBatch.available` pages from committed
    /// transactions, awaiting `free_page_batch` to return them to the bitmap.
    pub(crate) committed_steal_protection: Vec<Vec<u64>>,
    /// Phase 40.9: `LocalPageBatch.freed` pages from committed transactions,
    /// awaiting `recycle_page` to push them to the lock-free queue.
    pub(crate) committed_recycle_pages: Vec<Vec<u64>>,
}

// ── TxnManager ───────────────────────────────────────────────────────────────

/// Coordinates the transaction lifecycle over the WAL and heap pages.
///
/// ## Interior mutability (Phase 40.10)
///
/// All methods take `&self`. Concurrent access is safe:
/// - `next_txn_id`: `AtomicU64` — lock-free txn ID allocation.
/// - `max_committed`: `AtomicU64` — lock-free snapshot reads.
/// - `active_set`: `RwLock` — concurrent snapshot reads, exclusive for begin/commit.
/// - `wal`: `ConcurrentWalWriter` — internal Mutex for group commit.
/// - `post_commit`: `Mutex<PostCommitBatches>` — brief lock for page-free bookkeeping.
/// - `last_clustered_roots`: `Mutex<HashMap>` — brief lock for root tracking.
pub struct TxnManager {
    wal: ConcurrentWalWriter,
    /// Monotonically increasing transaction ID counter.
    /// `AtomicU64` for lock-free allocation across concurrent connections.
    next_txn_id: AtomicU64,
    /// TxnId of the last committed transaction.
    /// `AtomicU64` allows `snapshot()` to read without `&mut self`.
    /// Written under `active_set.write()` lock for atomicity (DuckDB + PostgreSQL pattern).
    /// Read under `active_set.read()` lock when building MVCC snapshots.
    max_committed: AtomicU64,
    /// Set of currently in-flight transaction IDs.
    /// Protected by `RwLock` to allow concurrent snapshot reads.
    active_set: RwLock<HashSet<TxnId>>,
    /// Lowest in-flight txn_id — used as GC horizon (DuckDB-style).
    /// 0 means no active transactions.
    lowest_active_id: AtomicU64,
    /// When `true`, DML `commit()` skips inline flush+fsync.
    /// Set once at construction, immutable afterward.
    deferred_commit_mode: bool,
    /// Post-commit housekeeping data (page frees, page batch draining).
    post_commit: Mutex<PostCommitBatches>,
    /// WAL durability policy for committed DML. Set once at construction.
    durability_policy: WalDurabilityPolicy,
    /// Last known clustered root per table after the most recent commit or rollback.
    last_clustered_roots: Mutex<HashMap<u32, u64>>,
}

include!("txn_construction.rs");
include!("txn_begin_commit.rs");
include!("txn_rollback.rs");
include!("txn_record_heap.rs");
include!("txn_record_clustered.rs");
include!("txn_record_index.rs");
include!("txn_inspect.rs");

// ── Physical location helpers ─────────────────────────────────────────────────

/// Bytes prepended to `new_value` (Insert/Update) and `old_value` (Delete)
/// to encode the heap physical location for crash recovery:
/// `[page_id: u64 LE][slot_id: u16 LE]` = 10 bytes.
pub const PHYSICAL_LOC_LEN: usize = 10;

/// Encodes `(page_id, slot_id)` into a 10-byte array.
pub(crate) fn encode_physical_loc(page_id: u64, slot_id: u16) -> [u8; PHYSICAL_LOC_LEN] {
    let mut loc = [0u8; PHYSICAL_LOC_LEN];
    loc[0..8].copy_from_slice(&page_id.to_le_bytes());
    loc[8..10].copy_from_slice(&slot_id.to_le_bytes());
    loc
}

/// Decodes `(page_id, slot_id)` from the first 10 bytes of a WAL payload.
/// Returns `None` if the slice is too short (e.g. legacy or control entries).
pub fn decode_physical_loc(bytes: &[u8]) -> Option<(u64, u16)> {
    if bytes.len() < PHYSICAL_LOC_LEN {
        return None;
    }
    let page_id = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let slot_id = u16::from_le_bytes([bytes[8], bytes[9]]);
    Some((page_id, slot_id))
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn clustered_root_for_undo(
    roots: &HashMap<u32, u64>,
    table_id: u32,
    stage: &str,
) -> Result<u64, DbError> {
    roots
        .get(&table_id)
        .copied()
        .ok_or_else(|| DbError::BTreeCorrupted {
            msg: format!("clustered {stage} missing current root for table {table_id}"),
        })
}

/// Scans the WAL forward and returns the highest committed TxnId plus the
/// latest committed clustered root per table that still exists in the WAL.
fn scan_committed_state(wal_path: &Path) -> Result<(TxnId, HashMap<u32, u64>), DbError> {
    let reader = WalReader::open(wal_path)?;
    let mut max = 0u64;
    let mut active_clustered_roots: HashMap<u64, HashMap<u32, u64>> = HashMap::new();
    let mut committed_clustered_roots: HashMap<u32, u64> = HashMap::new();

    for result in reader.scan_forward(0)? {
        let entry = match result {
            Ok(entry) => entry,
            Err(DbError::WalEntryTruncated { .. } | DbError::WalChecksumMismatch { .. }) => break,
            Err(e) => return Err(e),
        };
        match entry.entry_type {
            EntryType::Begin => {
                active_clustered_roots.entry(entry.txn_id).or_default();
            }
            EntryType::Commit => {
                if entry.txn_id > max {
                    max = entry.txn_id;
                }
                if let Some(roots) = active_clustered_roots.remove(&entry.txn_id) {
                    for (table_id, root_pid) in roots {
                        committed_clustered_roots.insert(table_id, root_pid);
                    }
                }
            }
            EntryType::Rollback => {
                active_clustered_roots.remove(&entry.txn_id);
            }
            EntryType::ClusteredInsert
            | EntryType::ClusteredDeleteMark
            | EntryType::ClusteredUpdate => {
                if let Some(roots) = active_clustered_roots.get_mut(&entry.txn_id) {
                    let new_row = ClusteredRowImage::from_bytes(&entry.new_value)?;
                    roots.insert(entry.table_id, new_row.root_pid);
                }
            }
            EntryType::ClusteredFieldPatch => {
                // Field-patch updates rewrite bytes in the current clustered row
                // in place; they never change the tree shape or root page.
            }
            _ => {}
        }
    }

    Ok((max, committed_clustered_roots))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_storage::Page;
    use axiomdb_storage::{
        insert_tuple, read_tuple, read_tuple_image, rewrite_tuple_same_slot, MemoryStorage,
        PageType,
    };

    fn temp_wal() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        (dir, path)
    }

    // ── begin / commit ────────────────────────────────────────────────────────

    #[test]
    fn test_begin_commit_advances_max_committed() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        assert_eq!(mgr.max_committed(), 0);

        let conn = mgr.begin().unwrap();
        assert_eq!(conn.txn_id, 1);
        mgr.commit(conn).unwrap();
        assert_eq!(mgr.max_committed(), 1);

        let conn2 = mgr.begin().unwrap();
        assert_eq!(conn2.txn_id, 2);
        mgr.commit(conn2).unwrap();
        assert_eq!(mgr.max_committed(), 2);
    }

    #[test]
    fn test_begin_rollback_does_not_advance_max_committed() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        let conn = mgr.begin().unwrap();
        mgr.rollback(conn, &storage).unwrap();
        assert_eq!(mgr.max_committed(), 0);
    }

    // ── undo INSERT ───────────────────────────────────────────────────────────

    #[test]
    fn test_rollback_undo_insert_marks_slot_dead() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();
        let mut conn = mgr.begin().unwrap();
        let txn_id = conn.txn_id;

        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"hello", txn_id).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(&mut conn, 1, b"key", b"hello", page_id, slot_id)
            .unwrap();

        mgr.rollback(conn, &storage).unwrap();

        let page = storage.read_page(page_id).unwrap();
        let result = read_tuple(&page, slot_id).unwrap();
        assert!(
            result.is_none(),
            "slot must be dead after rollback of insert"
        );
    }

    // ── undo DELETE ───────────────────────────────────────────────────────────

    #[test]
    fn test_rollback_undo_delete_clears_deletion() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();

        // Insert row in txn 1, commit.
        let mut conn1 = mgr.begin().unwrap();
        let txn1 = conn1.txn_id;
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"data", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(&mut conn1, 1, b"k", b"data", page_id, slot_id)
            .unwrap();
        mgr.commit(conn1).unwrap();

        // Delete row in txn 2, then rollback.
        let mut conn2 = mgr.begin().unwrap();
        let txn2 = conn2.txn_id;
        {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            axiomdb_storage::delete_tuple(&mut p, slot_id, txn2).unwrap();
            storage.write_page(page_id, &p).unwrap();
        }
        mgr.record_delete(&mut conn2, 1, b"k", b"data", page_id, slot_id)
            .unwrap();
        mgr.rollback(conn2, &storage).unwrap();

        let page = storage.read_page(page_id).unwrap();
        let (hdr, _) = read_tuple(&page, slot_id).unwrap().unwrap();
        assert_eq!(
            hdr.txn_id_deleted, 0,
            "txn_id_deleted must be cleared after rollback"
        );
    }

    // ── undo UPDATE ───────────────────────────────────────────────────────────

    #[test]
    fn test_rollback_undo_update() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();

        // Insert original row in txn 1.
        let mut conn1 = mgr.begin().unwrap();
        let txn1 = conn1.txn_id;
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let old_slot = insert_tuple(&mut page, b"original", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(&mut conn1, 1, b"k", b"original", page_id, old_slot)
            .unwrap();
        mgr.commit(conn1).unwrap();

        // Update in txn 2: delete old + insert new.
        let mut conn2 = mgr.begin().unwrap();
        let txn2 = conn2.txn_id;
        {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            let new_slot =
                axiomdb_storage::update_tuple(&mut p, old_slot, b"updated", txn2).unwrap();
            storage.write_page(page_id, &p).unwrap();
            mgr.record_update(
                &mut conn2,
                HeapUpdateRecord {
                    table_id: 1,
                    key: b"k",
                    old_value: b"original",
                    new_value: b"updated",
                    page_id,
                    old_slot,
                    new_slot,
                },
            )
            .unwrap();
        }
        mgr.rollback(conn2, &storage).unwrap();

        let page = storage.read_page(page_id).unwrap();
        let (old_hdr, old_data) = read_tuple(&page, old_slot).unwrap().unwrap();
        assert_eq!(old_data, b"original");
        assert_eq!(
            old_hdr.txn_id_deleted, 0,
            "old row must be live after update rollback"
        );
        let new_slot = old_slot + 1;
        assert!(
            read_tuple(&page, new_slot).unwrap().is_none(),
            "new slot must be dead after update rollback"
        );
    }

    #[test]
    fn test_rollback_undo_update_in_place_restores_old_tuple_image() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();

        let mut conn1 = mgr.begin().unwrap();
        let txn1 = conn1.txn_id;
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"original", txn1).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(&mut conn1, 1, b"k", b"original", page_id, slot_id)
            .unwrap();
        mgr.commit(conn1).unwrap();

        let mut conn2 = mgr.begin().unwrap();
        let txn2 = conn2.txn_id;
        let old_image = {
            let bytes = *storage.read_page(page_id).unwrap().as_bytes();
            let mut p = Page::from_bytes(bytes).unwrap();
            let old_image = rewrite_tuple_same_slot(&mut p, slot_id, b"updated", txn2)
                .unwrap()
                .unwrap();
            let new_image = read_tuple_image(&p, slot_id).unwrap().unwrap();
            storage.write_page(page_id, &p).unwrap();
            mgr.record_update_in_place(
                &mut conn2, 1, b"k", &old_image, &new_image, page_id, slot_id,
            )
            .unwrap();
            old_image
        };

        mgr.rollback(conn2, &storage).unwrap();

        let page = storage.read_page(page_id).unwrap();
        let (hdr, data) = read_tuple(&page, slot_id).unwrap().unwrap();
        assert_eq!(data, b"original");
        assert_eq!(hdr.txn_id_created, 1);
        assert_eq!(hdr.txn_id_deleted, 0);
        assert_eq!(hdr.row_version, 0);
        assert_eq!(
            read_tuple_image(&page, slot_id).unwrap().unwrap(),
            old_image
        );
    }

    // ── snapshots ─────────────────────────────────────────────────────────────

    #[test]
    fn test_snapshot_returns_committed_snapshot() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();

        let snap = mgr.snapshot();
        assert_eq!(snap.snapshot_id, 1); // max_committed=0 → snapshot_id=1
        assert_eq!(snap.current_txn_id, 0);

        let conn = mgr.begin().unwrap();
        mgr.commit(conn).unwrap(); // max_committed=1

        let snap2 = mgr.snapshot();
        assert_eq!(snap2.snapshot_id, 2);
    }

    #[test]
    fn test_active_snapshot_has_current_txn_id() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();

        let conn = mgr.begin().unwrap();
        let txn_id = conn.txn_id;
        let snap = mgr.active_snapshot(&conn);
        assert_eq!(snap.current_txn_id, txn_id);
        assert_eq!(snap.snapshot_id, 1); // max_committed=0 at begin
        mgr.commit(conn).unwrap();
    }

    #[test]
    fn test_uncommitted_row_not_visible_via_snapshot() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        let page_id = storage.alloc_page(PageType::Data).unwrap();
        let mut conn = mgr.begin().unwrap();
        let txn_id = conn.txn_id;

        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        let slot_id = insert_tuple(&mut page, b"secret", txn_id).unwrap();
        storage.write_page(page_id, &page).unwrap();
        mgr.record_insert(&mut conn, 1, b"k", b"secret", page_id, slot_id)
            .unwrap();

        // A committed snapshot should NOT see txn's row.
        let snap = mgr.snapshot();
        let page = storage.read_page(page_id).unwrap();
        let (hdr, _) = read_tuple(&page, slot_id).unwrap().unwrap();
        assert!(
            !hdr.is_visible(&snap),
            "uncommitted row must not be visible"
        );

        // The active snapshot (with current_txn_id) SHOULD see it.
        let active_snap = mgr.active_snapshot(&conn);
        assert!(
            hdr.is_visible(&active_snap),
            "active txn must see its own writes"
        );

        mgr.rollback(conn, &storage).unwrap();
    }

    // ── error cases ───────────────────────────────────────────────────────────

    #[test]
    fn test_double_begin_concurrent() {
        // Phase 40.10: multiple connections can begin concurrently.
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();

        let conn1 = mgr.begin().unwrap();
        let conn2 = mgr.begin().unwrap();
        assert_ne!(conn1.txn_id, conn2.txn_id);
        // Both txns are in the active set.
        assert!(mgr.has_active_txn());
        let _ = mgr.rollback(conn1, &axiomdb_storage::MemoryStorage::new());
        let _ = mgr.rollback(conn2, &axiomdb_storage::MemoryStorage::new());
    }

    #[test]
    fn test_commit_without_begin_error() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        // We can't call commit() without a ConnectionTxn anymore;
        // instead verify that active_txn_id() is None when nothing started.
        assert!(mgr.active_txn_id().is_none());
    }

    #[test]
    fn test_rollback_without_begin_error() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        // No active transaction means active_txn_id() returns None
        assert!(mgr.active_txn_id().is_none());
    }

    // ── open / recovery ───────────────────────────────────────────────────────

    #[test]
    fn test_open_recovers_max_committed() {
        let (_dir, path) = temp_wal();

        {
            let mgr = TxnManager::create(&path).unwrap();
            let c = mgr.begin().unwrap();
            mgr.commit(c).unwrap(); // txn 1
            let c = mgr.begin().unwrap();
            mgr.commit(c).unwrap(); // txn 2
            let _c = mgr.begin().unwrap(); // txn 3 — never committed (crash)
        }

        let mgr = TxnManager::open(&path).unwrap();
        assert_eq!(mgr.max_committed(), 2);
        assert_eq!(mgr.active_txn_id(), None);
    }

    // ── WAL entry order ───────────────────────────────────────────────────────

    #[test]
    fn test_wal_entry_order() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();

        let mut conn = mgr.begin().unwrap();
        let txn_id = conn.txn_id;
        mgr.record_insert(&mut conn, 1, b"k", b"v", 99, 0).unwrap();
        mgr.commit(conn).unwrap();

        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap().entry_type)
            .collect();

        assert_eq!(
            entries,
            vec![EntryType::Begin, EntryType::Insert, EntryType::Commit]
        );
        let _ = txn_id;
    }

    #[test]
    fn test_record_update_in_place_batch_writes_parseable_entries() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();

        let mut conn = mgr.begin().unwrap();
        let txn_id = conn.txn_id;
        let key1 = encode_physical_loc(42, 1);
        let key2 = encode_physical_loc(42, 2);
        let old1 = b"old-row-1".to_vec();
        let new1 = b"new-row-1".to_vec();
        let old2 = b"old-row-2".to_vec();
        let new2 = b"new-row-2".to_vec();

        let batch = vec![
            (
                key1.as_slice(),
                old1.as_slice(),
                new1.as_slice(),
                42_u64,
                1_u16,
            ),
            (
                key2.as_slice(),
                old2.as_slice(),
                new2.as_slice(),
                42_u64,
                2_u16,
            ),
        ];
        mgr.record_update_in_place_batch(&mut conn, 7, &batch)
            .unwrap();
        mgr.commit(conn).unwrap();

        let reader = WalReader::open(&path).unwrap();
        let txn_entries: Vec<_> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap())
            .filter(|e| e.txn_id == txn_id)
            .collect();

        assert_eq!(txn_entries.len(), 4);
        assert_eq!(txn_entries[0].entry_type, EntryType::Begin);
        assert_eq!(txn_entries[1].entry_type, EntryType::UpdateInPlace);
        assert_eq!(txn_entries[2].entry_type, EntryType::UpdateInPlace);
        assert_eq!(txn_entries[3].entry_type, EntryType::Commit);
        assert_eq!(txn_entries[1].table_id, 7);
        assert_eq!(txn_entries[2].table_id, 7);
        assert_eq!(
            decode_physical_loc(&txn_entries[1].old_value),
            Some((42, 1)),
            "old_value must carry the physical location prefix",
        );
        assert_eq!(
            decode_physical_loc(&txn_entries[2].new_value),
            Some((42, 2)),
            "new_value must carry the physical location prefix",
        );
    }

    // ── autocommit ────────────────────────────────────────────────────────────

    #[test]
    fn test_autocommit_commits_on_ok() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        mgr.autocommit(&storage, |mgr, conn| {
            mgr.record_insert(conn, 1, b"k", b"v", 99, 0)?;
            Ok(())
        })
        .unwrap();

        assert_eq!(mgr.max_committed(), 1);
    }

    #[test]
    fn test_autocommit_rollbacks_on_err() {
        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        let result = mgr.autocommit(&storage, |_mgr, _conn| {
            Err::<(), _>(DbError::Other("simulated failure".into()))
        });

        assert!(result.is_err());
        assert_eq!(mgr.max_committed(), 0);
        assert!(mgr.active_txn_id().is_none());
    }

    // ── record_truncate ───────────────────────────────────────────────────────

    #[test]
    fn test_record_truncate_single_wal_entry() {
        use crate::reader::WalReader;
        use axiomdb_storage::{heap_chain::HeapChain, PageType};

        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        let root_page_id = storage.alloc_page(PageType::Data).unwrap();
        let init_page = axiomdb_storage::Page::new(PageType::Data, root_page_id);
        storage.write_page(root_page_id, &init_page).unwrap();

        let conn1 = mgr.begin().unwrap();
        let txn1 = conn1.txn_id;
        for i in 0u8..5 {
            HeapChain::insert(&storage, root_page_id, &[i; 8], txn1, None).unwrap();
        }
        mgr.commit(conn1).unwrap();

        let mut conn2 = mgr.begin().unwrap();
        let txn2 = conn2.txn_id;
        let snap = mgr.active_snapshot(&conn2);
        let raw_rids = HeapChain::scan_rids_visible(&storage, root_page_id, snap).unwrap();
        HeapChain::delete_batch(&storage, root_page_id, &raw_rids, txn2).unwrap();
        mgr.record_truncate(&mut conn2, 1, root_page_id).unwrap();
        mgr.commit(conn2).unwrap();

        let reader = WalReader::open(&path).unwrap();
        let txn2_dml: Vec<_> = reader
            .scan_forward(0)
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|e| e.txn_id == txn2)
            .filter(|e| {
                matches!(
                    e.entry_type,
                    EntryType::Insert
                        | EntryType::Delete
                        | EntryType::Update
                        | EntryType::UpdateInPlace
                        | EntryType::Truncate
                )
            })
            .collect();

        assert_eq!(txn2_dml.len(), 1, "expected exactly 1 WAL DML entry");
        assert_eq!(txn2_dml[0].entry_type, EntryType::Truncate);
        let encoded_root = u64::from_le_bytes(txn2_dml[0].key[..8].try_into().unwrap());
        assert_eq!(encoded_root, root_page_id);
        let _ = txn1;
    }

    #[test]
    fn test_truncate_rollback_restores_rows() {
        use axiomdb_core::TransactionSnapshot;
        use axiomdb_storage::{heap_chain::HeapChain, PageType};

        let (_dir, path) = temp_wal();
        let mgr = TxnManager::create(&path).unwrap();
        let storage = MemoryStorage::new();

        let root_page_id = storage.alloc_page(PageType::Data).unwrap();
        let init_page = axiomdb_storage::Page::new(PageType::Data, root_page_id);
        storage.write_page(root_page_id, &init_page).unwrap();

        let conn1 = mgr.begin().unwrap();
        let txn1 = conn1.txn_id;
        for i in 0u8..5 {
            HeapChain::insert(&storage, root_page_id, &[i; 8], txn1, None).unwrap();
        }
        mgr.commit(conn1).unwrap();

        let snap_after_insert = TransactionSnapshot::committed(mgr.max_committed());
        let before =
            HeapChain::scan_rids_visible(&storage, root_page_id, snap_after_insert).unwrap();
        assert_eq!(before.len(), 5);

        let mut conn2 = mgr.begin().unwrap();
        let txn2 = conn2.txn_id;
        let snap2 = mgr.active_snapshot(&conn2);
        let raw_rids = HeapChain::scan_rids_visible(&storage, root_page_id, snap2).unwrap();
        HeapChain::delete_batch(&storage, root_page_id, &raw_rids, txn2).unwrap();
        mgr.record_truncate(&mut conn2, 1, root_page_id).unwrap();
        mgr.rollback(conn2, &storage).unwrap();

        let snap_after_rollback = TransactionSnapshot::committed(mgr.max_committed());
        let after =
            HeapChain::scan_rids_visible(&storage, root_page_id, snap_after_rollback).unwrap();
        assert_eq!(
            after.len(),
            5,
            "all rows must be visible again after rollback"
        );
    }
}
