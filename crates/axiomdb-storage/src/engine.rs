use axiomdb_core::error::DbError;

use crate::page::{Page, PageType};
use crate::page_ref::PageRef;

/// Unified storage engine interface.
///
/// Implementations: [`MmapStorage`] (disk, mmap) and [`MemoryStorage`] (RAM, tests).
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
/// fn do_something(engine: &mut dyn StorageEngine) { ... }
/// let engine: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
/// ```
///
/// [`MmapStorage`]: crate::MmapStorage
/// [`MemoryStorage`]: crate::MemoryStorage
pub trait StorageEngine: Send + Sync {
    /// Returns an owned copy of page `page_id`.
    /// Verifies the checksum before returning.
    fn read_page(&self, page_id: u64) -> Result<PageRef, DbError>;

    /// Writes `page` to `page_id`. The page must have a valid checksum.
    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError>;

    /// Allocates a new page of the given type. Grows the storage if necessary.
    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError>;

    /// Returns `page_id` to the free page pool.
    /// Returns an error on double-free or invalid page_id.
    fn free_page(&mut self, page_id: u64) -> Result<(), DbError>;

    /// Syncs to durable storage. No-op in MemoryStorage.
    fn flush(&mut self) -> Result<(), DbError>;

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
    fn set_current_snapshot(&mut self, _snapshot_id: u64) {}

    /// Returns the number of pages currently waiting in the deferred-free queue.
    /// Useful for diagnostics and tests. Default: 0.
    fn deferred_free_count(&self) -> usize {
        0
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

    /// Generic test suite for any StorageEngine implementation.
    /// Call from the implementation-specific tests.
    pub fn run_storage_engine_suite(engine: &mut dyn StorageEngine) {
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
