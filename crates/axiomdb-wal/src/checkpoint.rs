//! WAL Checkpoint — flush dirty pages then record a durable Checkpoint LSN.
//!
//! ## Ordering invariant (MUST NOT be violated)
//!
//! ```text
//! 1. storage.flush()          ← all pages land on disk (msync)
//! 2. wal.append(Checkpoint)   ← record in WAL that pages are safe
//! 3. wal.commit()             ← fsync — Checkpoint entry is durable
//! 4. write checkpoint_lsn → meta page (page 0)
//! 5. storage.flush()          ← meta page is durable
//! ```
//!
//! If the process crashes between steps 1 and 3, the pages are on disk but
//! the WAL has no durable Checkpoint entry. Crash recovery (3.8) will replay
//! from the previous checkpoint LSN — correct and safe.
//!
//! If the process crashes between steps 3 and 5, the Checkpoint WAL entry
//! exists but `last_checkpoint_lsn()` returns the old value. Recovery (3.8)
//! scans the WAL backward as a fallback — correct.

use axiomdb_core::error::DbError;
use axiomdb_storage::{read_checkpoint_lsn, write_checkpoint_lsn, StorageEngine};

use crate::{
    entry::{EntryType, WalEntry},
    writer::WalWriter,
};

/// Stateless checkpoint executor.
///
/// All persistent state lives in the storage (meta page) and the WAL file.
/// Being stateless avoids synchronisation issues and simplifies the API.
pub struct Checkpointer;

impl Checkpointer {
    /// Executes a full checkpoint and returns the checkpoint LSN.
    ///
    /// The five-step sequence is documented in the module header. The caller
    /// is responsible for not calling this while a transaction is actively
    /// writing — checkpointing mid-transaction produces a consistent checkpoint
    /// only up to the last committed state, which is safe but wastes work.
    ///
    /// # Errors
    /// Any I/O error from storage flush, WAL append, or WAL commit is
    /// propagated immediately. On error, the checkpoint is not recorded in the
    /// meta page, so the database remains consistent with the previous checkpoint.
    pub fn checkpoint(
        storage: &mut dyn StorageEngine,
        wal: &mut WalWriter,
    ) -> Result<u64, DbError> {
        // Step 1: flush all pages to disk BEFORE writing the Checkpoint WAL entry.
        // This is the critical ordering constraint — see module doc.
        storage.flush()?;

        // Step 2: write Checkpoint WAL entry (no payload, txn_id = 0).
        let mut entry = WalEntry::new(0, 0, EntryType::Checkpoint, 0, vec![], vec![], vec![]);
        let checkpoint_lsn = wal.append(&mut entry)?;

        // Step 3: fsync — Checkpoint entry is now durable.
        wal.commit()?;

        // Step 4: record the checkpoint LSN in the meta page.
        write_checkpoint_lsn(storage, checkpoint_lsn)?;

        // Step 5: flush meta page to disk.
        storage.flush()?;

        Ok(checkpoint_lsn)
    }

    /// Returns the LSN of the last successful checkpoint.
    ///
    /// Returns `0` if the database has never been checkpointed.
    /// Crash recovery (3.8) uses this as the starting LSN for WAL replay.
    pub fn last_checkpoint_lsn(storage: &dyn StorageEngine) -> Result<u64, DbError> {
        read_checkpoint_lsn(storage)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_storage::{MemoryStorage, MmapStorage, PageType};

    use crate::{reader::WalReader, EntryType as ET, TxnManager};

    fn temp_wal() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        (dir, path)
    }

    // ── MemoryStorage tests (fast, no I/O) ────────────────────────────────────

    #[test]
    fn test_fresh_db_last_checkpoint_lsn_is_zero() {
        let (_dir, path) = temp_wal();
        let storage = MemoryStorage::new();
        assert_eq!(Checkpointer::last_checkpoint_lsn(&storage).unwrap(), 0);
    }

    #[test]
    fn test_checkpoint_stores_lsn_in_meta_page() {
        let (_dir, path) = temp_wal();
        let mut storage = MemoryStorage::new();
        let mut wal = WalWriter::create(&path).unwrap();

        let lsn = Checkpointer::checkpoint(&mut storage, &mut wal).unwrap();
        assert!(lsn > 0);
        assert_eq!(Checkpointer::last_checkpoint_lsn(&storage).unwrap(), lsn);
    }

    #[test]
    fn test_multiple_checkpoints_monotonically_increasing() {
        let (_dir, path) = temp_wal();
        let mut storage = MemoryStorage::new();
        let mut wal = WalWriter::create(&path).unwrap();

        let lsn1 = Checkpointer::checkpoint(&mut storage, &mut wal).unwrap();
        let lsn2 = Checkpointer::checkpoint(&mut storage, &mut wal).unwrap();
        let lsn3 = Checkpointer::checkpoint(&mut storage, &mut wal).unwrap();

        assert!(lsn1 < lsn2 && lsn2 < lsn3);
        assert_eq!(Checkpointer::last_checkpoint_lsn(&storage).unwrap(), lsn3);
    }

    #[test]
    fn test_checkpoint_entry_readable_via_wal_reader() {
        let (_dir, path) = temp_wal();
        let mut storage = MemoryStorage::new();
        let mut wal = WalWriter::create(&path).unwrap();

        let ckpt_lsn = Checkpointer::checkpoint(&mut storage, &mut wal).unwrap();

        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_type, ET::Checkpoint);
        assert_eq!(entries[0].lsn, ckpt_lsn);
    }

    #[test]
    fn test_checkpoint_with_memory_storage_succeeds() {
        // MemoryStorage::flush() is a no-op — checkpoint must still complete.
        let (_dir, path) = temp_wal();
        let mut storage = MemoryStorage::new();
        let mut wal = WalWriter::create(&path).unwrap();

        let result = Checkpointer::checkpoint(&mut storage, &mut wal);
        assert!(result.is_ok());
    }

    #[test]
    fn test_checkpoint_after_txn_commit() {
        // begin → record_insert → commit → checkpoint.
        // Verifies the Checkpoint entry appears after the Commit entry in the WAL.
        let (_dir, path) = temp_wal();
        let mut storage = MemoryStorage::new();
        let mut mgr = TxnManager::create(&path).unwrap();

        mgr.begin().unwrap();
        mgr.record_insert(1, b"key", b"value", 99, 0).unwrap();
        mgr.commit().unwrap();

        let ckpt_lsn = Checkpointer::checkpoint(&mut storage, mgr.wal_mut()).unwrap();

        let reader = WalReader::open(&path).unwrap();
        let types: Vec<_> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap().entry_type)
            .collect();

        // WAL: [Begin, Insert, Commit, Checkpoint]
        assert_eq!(
            types,
            vec![ET::Begin, ET::Insert, ET::Commit, ET::Checkpoint]
        );
        // Checkpoint LSN must be greater than all previous LSNs.
        assert_eq!(ckpt_lsn, 4); // LSN 1=Begin, 2=Insert, 3=Commit, 4=Checkpoint
    }

    #[test]
    fn test_checkpoint_empty_wal() {
        // WAL has only the file header, no entries yet.
        let (_dir, path) = temp_wal();
        let mut storage = MemoryStorage::new();
        let mut wal = WalWriter::create(&path).unwrap();

        assert_eq!(wal.current_lsn(), 0); // no entries yet
        let lsn = Checkpointer::checkpoint(&mut storage, &mut wal).unwrap();
        assert_eq!(lsn, 1); // first entry gets LSN 1
    }

    // ── MmapStorage tests (real I/O, persistence) ─────────────────────────────

    #[test]
    fn test_checkpoint_lsn_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let wal_path = dir.path().join("test.wal");

        // Session 1: checkpoint.
        let checkpoint_lsn = {
            let mut storage = MmapStorage::create(&db_path).unwrap();
            let mut wal = WalWriter::create(&wal_path).unwrap();
            Checkpointer::checkpoint(&mut storage, &mut wal).unwrap()
        };

        // Session 2: reopen — checkpoint_lsn must persist.
        let storage2 = MmapStorage::open(&db_path).unwrap();
        let recovered = Checkpointer::last_checkpoint_lsn(&storage2).unwrap();
        assert_eq!(recovered, checkpoint_lsn);
    }

    #[test]
    fn test_checkpoint_lsn_advances_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let wal_path = dir.path().join("test.wal");

        // Session 1: two checkpoints.
        let lsn2 = {
            let mut storage = MmapStorage::create(&db_path).unwrap();
            let mut wal = WalWriter::create(&wal_path).unwrap();
            Checkpointer::checkpoint(&mut storage, &mut wal).unwrap(); // lsn=1
            Checkpointer::checkpoint(&mut storage, &mut wal).unwrap() // lsn=2
        };

        // Session 2: last checkpoint LSN = 2.
        let storage2 = MmapStorage::open(&db_path).unwrap();
        assert_eq!(Checkpointer::last_checkpoint_lsn(&storage2).unwrap(), lsn2);
    }
}
