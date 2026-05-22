use axiomdb_core::error::DbError;

use crate::page::{Page, PageType};
use crate::page_ref::PageRef;

/// Unified storage engine interface.
///
/// Implementations: [`MmapStorage`] (disk, mmap) and [`MemoryStorage`] (RAM, tests).
///
/// ## Interior mutability (Phase 40.3)
///
/// All mutating methods (`write_page`, `alloc_page`, `free_page`, `flush`,
/// `set_current_snapshot`) now take `&self` instead of `&mut self`. Mutable
/// state is managed internally with per-page `RwLock`s, a sharded
/// [`PageLockTable`], and `Mutex`/`AtomicU64` fields.
///
/// This mirrors the design of both InnoDB (per-page `block_lock`, no `&mut`
/// on the buffer pool) and PostgreSQL (per-buffer atomic `state` field with
/// no `&mut` on the `BufferDescriptor` array).
///
/// Two transactions writing **different** pages proceed in full parallelism.
/// Two transactions writing the **same** page are serialized by the page lock.
///
/// ## Owned page references (Phase 7.4a)
///
/// `read_page` returns an owned [`PageRef`] (heap-allocated copy) instead of
/// `&Page`. This is safe for concurrent access: the caller owns the data and
/// it remains valid even if the storage backend remaps or grows the file.
///
/// The copy cost (~0.5µs from L2/L3 cache) is comparable to PostgreSQL's
/// buffer pool copy. All production databases (PostgreSQL, InnoDB, DuckDB,
/// SQLite) copy page data rather than returning raw pointers into mmap.
///
/// ## Usage with trait objects
/// ```rust,ignore
/// fn do_something(engine: &dyn StorageEngine) { ... }
/// let engine: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
/// ```
///
/// [`MmapStorage`]: crate::MmapStorage
/// [`MemoryStorage`]: crate::MemoryStorage
/// [`PageLockTable`]: crate::page_lock::PageLockTable
pub trait StorageEngine: Send + Sync {
    /// Returns an owned copy of page `page_id`.
    /// Verifies the checksum before returning.
    fn read_page(&self, page_id: u64) -> Result<PageRef, DbError>;

    /// Returns the raw bytes of page `page_id` without checksum validation.
    /// Use only for backup/export paths where all pages must be captured
    /// regardless of checksum validity.
    fn read_page_raw(&self, page_id: u64) -> Result<[u8; crate::PAGE_SIZE], DbError>;

    /// Writes `page` to `page_id`. The page must have a valid checksum.
    ///
    /// General entry point for callers that do not already hold the page's
    /// exclusive latch. Implementations serialize concurrent writers on the
    /// same page internally.
    fn write_page(&self, page_id: u64, page: &Page) -> Result<(), DbError>;

    /// Writes `page` to `page_id` assuming the caller already holds the page's
    /// exclusive latch from [`page_lock_table`](Self::page_lock_table).
    ///
    /// Read-modify-write paths such as [`crate::HeapChain`] must use this
    /// method after taking the latch themselves; calling [`write_page`] there
    /// would re-acquire the same lock and can self-deadlock on backends that
    /// enforce per-page locking internally.
    fn write_page_under_page_lock(&self, page_id: u64, page: &Page) -> Result<(), DbError>;

    /// Allocates a new page of the given type. Grows the storage if necessary.
    ///
    /// Thread-safe: allocation acquires a `Mutex<FreeList>` held only during
    /// the bitmap scan (~microseconds). File growth acquires a separate
    /// grow lock to prevent concurrent resizes.
    fn alloc_page(&self, page_type: PageType) -> Result<u64, DbError>;

    /// Returns `page_id` to the free page pool.
    /// Returns an error on double-free or invalid page_id.
    fn free_page(&self, page_id: u64) -> Result<(), DbError>;

    /// Syncs to durable storage. No-op in MemoryStorage.
    fn flush(&self) -> Result<(), DbError>;

    /// Current capacity (total pages in storage).
    fn page_count(&self) -> u64;

    /// Hint to the storage backend that pages starting at `start_page_id` will be
    /// read sequentially. The backend may prefetch `count` pages ahead
    /// (0 = use the backend-defined default). Implementations that do not support
    /// prefetch provide a default no-op.
    fn prefetch_hint(&self, start_page_id: u64, count: u64) {
        let _ = (start_page_id, count);
    }

    /// Tells the storage the current transaction's snapshot_id so that
    /// `free_page` can tag deferred frees with the epoch at which they became
    /// unreachable. Implementations that do not defer frees may ignore this.
    fn set_current_snapshot(&self, _snapshot_id: u64) {}

    /// Tells the storage the current transaction's id so `write_page` can stamp it
    /// into page frames (project B REDO recovery). `MmapStorage` keeps it in a
    /// **thread-local** (correct under multi-writer: each statement runs
    /// synchronously on one thread); the executor sets it per statement and resets
    /// it to 0 at statement end. `0` = non-transactional / system write. Default no-op.
    fn set_current_txn(&self, _txn_id: u64) {}

    /// Reads the current transaction id (thread-local in `MmapStorage`). The
    /// executor uses it to save/restore the stamp around nested sub-transactions
    /// (a `nextval`/cron sub-txn must stamp its own id, then restore the parent's).
    /// Default `0`.
    fn current_txn(&self) -> u64 {
        0
    }

    /// Makes the page-frame redo log durable (fsync) at the commit boundary, before
    /// the logical WAL `Commit` record (write-ahead: data before the commit record).
    /// Default no-op (backend without a redo log).
    fn sync_frame_log(&self) -> Result<(), DbError> {
        Ok(())
    }

    /// REDO: apply every committed frame to its page so committed data survives a
    /// crash (project B subphase 5). For each page with a committed frame (latest
    /// wins), if `frame.lsn > page.lsn` the frame's bytes are written to the page
    /// **directly** — no new frame is appended. `is_committed(txn_id)` selects
    /// committed frames; recovery passes the logical WAL's committed-txn set.
    /// Returns the number of pages redone. Called once, on open, after the UNDO pass.
    /// Default: no-op (backend without a redo log).
    fn redo_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
        let _ = is_committed;
        Ok(0)
    }

    /// CHECKPOINT: apply every committed frame to the main file (pageLSN-guarded, growing
    /// the file for a page beyond EOF), fsync the main file, then recycle the frame log
    /// (project B subphase 6b). `is_committed(txn_id)` selects committed frames. Returns
    /// the number of pages written to the main file. Default: no-op (no redo log).
    fn checkpoint_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
        let _ = is_committed;
        Ok(0)
    }

    /// Returns the number of pages currently waiting in the deferred-free queue.
    /// Useful for diagnostics and tests. Default: 0.
    fn deferred_free_count(&self) -> usize {
        0
    }

    /// Returns a reference to the per-page lock table for heap chain operations.
    ///
    /// HeapChain read-modify-write methods acquire X-latches from this table
    /// before mutating page contents and then persist through
    /// `write_page_under_page_lock`. Readers don't latch — MVCC visibility
    /// handles concurrent reads (Phase 40.7).
    fn page_lock_table(&self) -> &crate::page_lock::PageLockTable;

    // ── Batch allocation (Phase 40.9) ───────────────────────────────────

    /// Allocates `n` page IDs of the given type in one batch operation.
    ///
    /// Returns **uninitialized** page IDs (the caller is responsible for
    /// `Page::new(ty, pid) + write_page(pid, &p)` at first use). The IDs
    /// are marked USED in the on-disk bitmap immediately.
    ///
    /// The default implementation falls back to `n` individual `alloc_page`
    /// calls. `MmapStorage` overrides this with a single `Mutex<FreeList>`
    /// acquisition per batch (amortized lock cost: ~2µs / 64 = 31 ns each).
    ///
    /// ## Recycle priority
    ///
    /// When `MmapStorage` overrides this method, committed-but-not-flushed
    /// pages from the `recycle_queue` are drained first (lock-free), then
    /// the bitmap is consulted, and finally the file is grown if needed.
    fn alloc_page_batch(&self, n: usize, page_type: PageType) -> Result<Vec<u64>, DbError> {
        let mut ids = Vec::with_capacity(n);
        for _ in 0..n {
            ids.push(self.alloc_page(page_type)?);
        }
        Ok(ids)
    }

    /// Returns `ids` to the free page pool in one batch operation.
    ///
    /// The default implementation falls back to `ids.len()` individual
    /// `free_page` calls. `MmapStorage` overrides this with a single
    /// `Mutex<FreeList>` acquisition per batch.
    fn free_page_batch(&self, ids: &[u64]) -> Result<(), DbError> {
        for &id in ids {
            self.free_page(id)?;
        }
        Ok(())
    }

    /// Number of threads currently waiting on the global freelist mutex.
    ///
    /// Used by `LocalPageBatch::pop_or_refill` for adaptive batch sizing:
    /// `refill_size × max(1, extension_waiters)`, capped at `MAX_BATCH_SIZE`.
    /// Default: 0 (no contention tracking for MemoryStorage).
    fn extension_waiters(&self) -> u32 {
        0
    }

    /// Pushes a page ID into the lock-free recycle queue for fast reuse by
    /// future `alloc_page_batch` calls.
    ///
    /// Called on COMMIT: the transaction's `LocalPageBatch.freed` pages are
    /// pushed here so other transactions can pick them up without touching
    /// the global bitmap. The queue is drained back into the bitmap during
    /// `flush()` / `release_deferred_frees`.
    ///
    /// Default: delegates to `free_page`. Overridden by `MmapStorage`.
    fn recycle_page(&self, page_id: u64) -> Result<(), DbError> {
        self.free_page(page_id)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::page::PageType;

    #[test]
    fn test_prefetch_hint_noop_on_memory_storage() {
        use crate::MemoryStorage;
        let storage = MemoryStorage::new();
        storage.prefetch_hint(0, 0);
        storage.prefetch_hint(0, 64);
        storage.prefetch_hint(9_999_999, 64);
        storage.prefetch_hint(u64::MAX, u64::MAX);
    }

    #[test]
    fn redo_is_noop_without_a_frame_log() {
        use crate::MemoryStorage;
        let storage = MemoryStorage::new();
        assert_eq!(storage.redo_committed_frames(&|_| true).unwrap(), 0);
    }

    /// Generic test suite for any StorageEngine implementation.
    /// Call from the implementation-specific tests.
    pub fn run_storage_engine_suite(engine: &dyn StorageEngine) {
        // alloc returns unique page_ids.
        let id1 = engine.alloc_page(PageType::Data).unwrap();
        let id2 = engine.alloc_page(PageType::Data).unwrap();
        let id3 = engine.alloc_page(PageType::Index).unwrap();
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);

        // write + read roundtrip.
        let mut page = Page::new(PageType::Data, id1);
        page.body_mut()[0] = 0xAB;
        page.body_mut()[1] = 0xCD;
        page.update_checksum();
        engine.write_page(id1, &page).unwrap();
        let read = engine.read_page(id1).unwrap();
        assert_eq!(read.body()[0], 0xAB);
        assert_eq!(read.body()[1], 0xCD);

        // free + flush (releases deferred frees) + realloc reuses the page_id.
        engine.free_page(id1).unwrap();
        engine.flush().unwrap(); // release deferred frees to freelist
        let id_reused = engine.alloc_page(PageType::Data).unwrap();
        assert_eq!(id_reused, id1);

        // double-free: freeing id1 without re-allocating it → error.
        // id1 is USED (we just re-allocated it), so freeing it once
        // is correct; freeing it twice in a row is a double-free.
        engine.free_page(id1).unwrap(); // first free — deferred
        engine.flush().unwrap(); // release to freelist
        assert!(engine.free_page(id1).is_err()); // second — double-free

        // read on non-existent page is an error.
        assert!(engine.read_page(999_999).is_err());

        // flush does not fail.
        engine.flush().unwrap();
    }
}
