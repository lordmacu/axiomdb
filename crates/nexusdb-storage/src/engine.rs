use nexusdb_core::error::DbError;

use crate::page::{Page, PageType};

/// Unified storage engine interface.
///
/// Implementations: [`MmapStorage`] (disk, mmap) and [`MemoryStorage`] (RAM, tests).
///
/// ## Lifetimes and borrow checker
/// `read_page` returns `&Page` tied to `&self` — while that reference exists,
/// `&mut self` methods cannot be called. This invariant is correct: it prevents
/// modifications while reading, without needing locks.
///
/// ## Usage with trait objects
/// ```rust,ignore
/// fn do_something(engine: &mut dyn StorageEngine) { ... }
/// let engine: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
/// ```
///
/// [`MmapStorage`]: crate::MmapStorage
/// [`MemoryStorage`]: crate::MemoryStorage
pub trait StorageEngine: Send {
    /// Returns a zero-copy reference to page `page_id`.
    /// Verifies the checksum before returning.
    fn read_page(&self, page_id: u64) -> Result<&Page, DbError>;

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
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::page::PageType;

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

        // free + realloc reuses the page_id.
        engine.free_page(id1).unwrap();
        let id_reused = engine.alloc_page(PageType::Data).unwrap();
        assert_eq!(id_reused, id1);

        // double-free: freeing id1 without re-allocating it → error.
        // id1 is USED (we just re-allocated it), so freeing it once
        // is correct; freeing it twice in a row is a double-free.
        engine.free_page(id1).unwrap(); // first free — valid
        assert!(engine.free_page(id1).is_err()); // second — double-free

        // read on non-existent page is an error.
        assert!(engine.read_page(999_999).is_err());

        // flush does not fail.
        engine.flush().unwrap();
    }
}
