//! WAL rotation — size-based threshold for automatic checkpoint + WAL truncation.
//!
//! [`WalRotator`] wraps a `max_wal_size` threshold and delegates rotation to
//! [`TxnManager::check_and_rotate`]. It is purely a convenience struct; all the
//! actual work is done by the checkpoint and writer machinery.

use std::path::Path;

use axiomdb_core::error::DbError;
use axiomdb_storage::StorageEngine;

use crate::txn::TxnManager;

/// Triggers WAL rotation when the WAL file exceeds a configurable size threshold.
///
/// # Example
///
/// ```rust,ignore
/// let rotator = WalRotator::new(64 * 1024 * 1024); // 64 MB
///
/// // After each commit, check if rotation is needed:
/// if rotator.check_and_rotate(&mut txn_mgr, &mut storage, &wal_path)? {
///     // WAL was rotated — next LSN continues from checkpoint_lsn + 1
/// }
/// ```
#[derive(Debug, Clone)]
pub struct WalRotator {
    /// Maximum WAL file size in bytes before auto-rotation.
    pub max_wal_size: u64,
}

impl WalRotator {
    /// Default maximum WAL size: 64 MB.
    pub const DEFAULT_MAX_WAL_SIZE: u64 = 64 * 1024 * 1024;

    /// Creates a new `WalRotator` with the given size threshold.
    pub fn new(max_wal_size: u64) -> Self {
        Self { max_wal_size }
    }

    /// Creates a `WalRotator` with the default 64 MB threshold.
    pub fn default_size() -> Self {
        Self::new(Self::DEFAULT_MAX_WAL_SIZE)
    }

    /// Rotates the WAL if its current size exceeds `max_wal_size`.
    ///
    /// Returns `true` if rotation occurred, `false` otherwise.
    pub fn check_and_rotate(
        &self,
        mgr: &mut TxnManager,
        storage: &mut dyn StorageEngine,
        wal_path: &Path,
    ) -> Result<bool, DbError> {
        mgr.check_and_rotate(storage, wal_path, self.max_wal_size)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_storage::{MemoryStorage, MmapStorage};

    use crate::{reader::WalReader, EntryType, TxnManager, WalWriter};

    fn temp_setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let wal_path = dir.path().join("test.wal");
        let db_path = dir.path().join("test.db");
        (dir, wal_path, db_path)
    }

    // ── Basic rotation ────────────────────────────────────────────────────────

    #[test]
    fn test_rotate_lsn_continues_monotonically() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        // Write 3 entries (LSNs 1, 2, 3).
        mgr.begin().unwrap();
        mgr.record_insert(1, b"k", b"v", 99, 0).unwrap();
        mgr.commit().unwrap();
        // LSN 1=Begin, 2=Insert, 3=Commit → current_lsn = 3

        let checkpoint_lsn = mgr.rotate_wal(&mut storage, &wal_path).unwrap();
        assert_eq!(checkpoint_lsn, 4); // Checkpoint entry = LSN 4

        // Next entry after rotation must be LSN 5, not LSN 1.
        mgr.begin().unwrap();
        let lsn = mgr.current_lsn();
        // current_lsn after begin (LSN 5 is the Begin entry).
        assert!(
            lsn >= 5,
            "LSN must continue from checkpoint_lsn + 1, got {lsn}"
        );
    }

    #[test]
    fn test_rotated_wal_file_is_header_only() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        mgr.begin().unwrap();
        mgr.commit().unwrap();
        mgr.rotate_wal(&mut storage, &wal_path).unwrap();

        // After rotation, the logical WAL contains only the header, but the
        // physical file may still have reserved tail capacity for the durable
        // fast path.
        let size = std::fs::metadata(&wal_path).unwrap().len();
        assert!(size >= crate::WAL_HEADER_SIZE as u64);

        let reader = WalReader::open(&wal_path).unwrap();
        let entries: Vec<_> = reader.scan_forward(0).unwrap().collect();
        assert!(
            entries.is_empty(),
            "rotated WAL must expose no entries beyond the header"
        );
    }

    #[test]
    fn test_multiple_rotations_lsn_monotonic() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        let mut prev_lsn = 0u64;
        for _ in 0..3 {
            mgr.begin().unwrap();
            mgr.commit().unwrap();
            let ckpt = mgr.rotate_wal(&mut storage, &wal_path).unwrap();
            assert!(
                ckpt > prev_lsn,
                "checkpoint_lsn must be strictly increasing"
            );
            prev_lsn = ckpt;
        }
    }

    #[test]
    fn test_rotate_with_active_txn_returns_error() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        mgr.begin().unwrap();
        let err = mgr.rotate_wal(&mut storage, &wal_path).unwrap_err();
        assert!(matches!(err, DbError::TransactionAlreadyActive { .. }));
    }

    #[test]
    fn test_check_and_rotate_below_threshold_no_rotation() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        // Threshold = 1 GB — WAL will never exceed it in this test.
        let rotated = mgr
            .check_and_rotate(&mut storage, &wal_path, 1024 * 1024 * 1024)
            .unwrap();
        assert!(!rotated);
    }

    #[test]
    fn test_check_and_rotate_above_threshold_triggers_rotation() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        mgr.begin().unwrap();
        mgr.commit().unwrap();

        // Threshold = 0 — any WAL size triggers rotation.
        let rotated = mgr.check_and_rotate(&mut storage, &wal_path, 0).unwrap();
        assert!(rotated);
    }

    #[test]
    fn test_wal_rotator_convenience_wrapper() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        mgr.begin().unwrap();
        mgr.commit().unwrap();

        let rotator = WalRotator::new(0); // always triggers
        let rotated = rotator
            .check_and_rotate(&mut mgr, &mut storage, &wal_path)
            .unwrap();
        assert!(rotated);
    }

    #[test]
    fn test_rotation_updates_checkpoint_lsn_in_meta_page() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        mgr.begin().unwrap();
        mgr.commit().unwrap();
        let ckpt_lsn = mgr.rotate_wal(&mut storage, &wal_path).unwrap();

        let stored = axiomdb_storage::read_checkpoint_lsn(&storage).unwrap();
        assert_eq!(stored, ckpt_lsn);
    }

    #[test]
    fn test_entries_after_rotation_have_correct_lsns() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        // First session: 2 entries (LSN 1=Begin, 2=Commit) + rotate (LSN 3=Checkpoint).
        mgr.begin().unwrap();
        mgr.commit().unwrap();
        let ckpt = mgr.rotate_wal(&mut storage, &wal_path).unwrap();
        assert_eq!(ckpt, 3);

        // After rotation: new begin must be LSN 4.
        mgr.begin().unwrap();
        let begin_lsn = mgr.current_lsn();
        assert_eq!(begin_lsn, 4);
        mgr.commit().unwrap();
        let commit_lsn = mgr.current_lsn();
        assert_eq!(commit_lsn, 5);
    }

    // ── MmapStorage persistence ───────────────────────────────────────────────

    #[test]
    fn test_reopen_after_rotation_correct_next_lsn() {
        let (_dir, wal_path, db_path) = temp_setup();

        let ckpt_lsn = {
            let mut storage = MmapStorage::create(&db_path).unwrap();
            let mut mgr = TxnManager::create(&wal_path).unwrap();
            mgr.begin().unwrap();
            mgr.commit().unwrap();
            mgr.rotate_wal(&mut storage, &wal_path).unwrap()
        };

        // Reopen: WalWriter::open reads start_lsn from header.
        // next_lsn should be ckpt_lsn + 1, so current_lsn = ckpt_lsn.
        let w = WalWriter::open(&wal_path).unwrap();
        assert_eq!(w.current_lsn(), ckpt_lsn);
    }

    #[test]
    fn test_wal_entries_readable_after_rotation() {
        let (_dir, wal_path, _db) = temp_setup();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&wal_path).unwrap();

        // Write, rotate, write again.
        mgr.begin().unwrap();
        mgr.commit().unwrap();
        mgr.rotate_wal(&mut storage, &wal_path).unwrap(); // WAL now empty

        mgr.begin().unwrap();
        mgr.commit().unwrap();

        // Only the post-rotation entries should be in the WAL (Begin + Commit).
        let reader = WalReader::open(&wal_path).unwrap();
        let entries: Vec<_> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry_type, EntryType::Begin);
        assert_eq!(entries[1].entry_type, EntryType::Commit);
        // LSNs must be > 3 (the checkpoint LSN after first rotation).
        assert!(entries[0].lsn > 3);
    }
}
