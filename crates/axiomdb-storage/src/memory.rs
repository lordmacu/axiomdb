use axiomdb_core::error::DbError;

use crate::{
    engine::StorageEngine,
    freelist::FreeList,
    page::{Page, PageType},
};

/// In-RAM storage engine — no I/O, ideal for unit tests.
///
/// Pages are stored in a `Vec<Option<Box<Page>>>` indexed directly by `page_id`,
/// providing O(1) access with no hashing overhead.
/// Integrates `FreeList` for alloc/free with page reuse.
pub struct MemoryStorage {
    /// Slot `i` holds `Some(page)` if page `i` is allocated, `None` otherwise.
    pages: Vec<Option<Box<Page>>>,
    freelist: FreeList,
}

impl MemoryStorage {
    /// Creates an empty storage with page 0 (Meta) initialized.
    pub fn new() -> Self {
        const INITIAL_PAGES: u64 = 64;
        let mut pages: Vec<Option<Box<Page>>> = (0..INITIAL_PAGES as usize).map(|_| None).collect();
        pages[0] = Some(Box::new(Page::new(PageType::Meta, 0)));
        // Pages 0 and 1 reserved (meta + bitmap slot, consistent with MmapStorage).
        let freelist = FreeList::new(INITIAL_PAGES, &[0, 1]);
        MemoryStorage { pages, freelist }
    }
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageEngine for MemoryStorage {
    fn read_page(&self, page_id: u64) -> Result<&Page, DbError> {
        let page = self
            .pages
            .get(page_id as usize)
            .and_then(|slot| slot.as_deref())
            .ok_or(DbError::PageNotFound { page_id })?;
        // In MemoryStorage, pages never experience disk corruption.
        // Checksum is validated on write_page; re-verifying on every read
        // is redundant and accounts for ~300ns overhead per lookup level.
        // In debug builds we still verify to catch logic bugs early.
        debug_assert!(
            page.verify_checksum().is_ok(),
            "checksum mismatch in MemoryStorage — logic bug in write path"
        );
        Ok(page)
    }

    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError> {
        let owned = Page::from_bytes(*page.as_bytes())?;
        let idx = page_id as usize;
        if idx >= self.pages.len() {
            return Err(DbError::PageNotFound { page_id });
        }
        self.pages[idx] = Some(Box::new(owned));
        Ok(())
    }

    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError> {
        if let Some(page_id) = self.freelist.alloc() {
            let idx = page_id as usize;
            if idx >= self.pages.len() {
                self.pages.resize_with(idx + 1, || None);
            }
            self.pages[idx] = Some(Box::new(Page::new(page_type, page_id)));
            return Ok(page_id);
        }
        // Grow by 64 pages and retry.
        let old_total = self.freelist.total_pages();
        let new_total = old_total + 64;
        self.freelist.grow(new_total);
        self.pages.resize_with(new_total as usize, || None);
        let page_id = self.freelist.alloc().ok_or(DbError::Other(
            "freelist empty after grow — internal invariant violated".into(),
        ))?;
        let idx = page_id as usize;
        self.pages[idx] = Some(Box::new(Page::new(page_type, page_id)));
        Ok(page_id)
    }

    fn free_page(&mut self, page_id: u64) -> Result<(), DbError> {
        if page_id == 0 || page_id == 1 {
            return Err(DbError::Other(format!(
                "cannot free reserved page {page_id}"
            )));
        }
        self.freelist.free(page_id)
    }

    fn flush(&mut self) -> Result<(), DbError> {
        Ok(())
    }

    fn page_count(&self) -> u64 {
        self.freelist.total_pages()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::run_storage_engine_suite;

    #[test]
    fn test_storage_engine_suite() {
        let mut storage = MemoryStorage::new();
        run_storage_engine_suite(&mut storage);
    }

    #[test]
    fn test_new_has_meta_page() {
        let storage = MemoryStorage::new();
        let page = storage.read_page(0).unwrap();
        assert_eq!(page.header().page_type, PageType::Meta as u8);
    }

    #[test]
    fn test_alloc_starts_from_2() {
        let mut storage = MemoryStorage::new();
        assert_eq!(storage.alloc_page(PageType::Data).unwrap(), 2);
        assert_eq!(storage.alloc_page(PageType::Data).unwrap(), 3);
    }

    #[test]
    fn test_free_and_realloc() {
        let mut storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.free_page(id).unwrap();
        assert_eq!(storage.alloc_page(PageType::Data).unwrap(), id);
    }

    #[test]
    fn test_double_free_is_error() {
        let mut storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.free_page(id).unwrap();
        assert!(storage.free_page(id).is_err());
    }

    #[test]
    fn test_free_reserved_is_error() {
        let mut storage = MemoryStorage::new();
        assert!(storage.free_page(0).is_err());
        assert!(storage.free_page(1).is_err());
    }

    #[test]
    fn test_alloc_grows_automatically() {
        let mut storage = MemoryStorage::new();
        let initial = storage.page_count();
        for _ in 0..(initial - 2) {
            storage.alloc_page(PageType::Data).unwrap();
        }
        let id = storage.alloc_page(PageType::Data).unwrap();
        assert!(id >= initial);
    }

    #[test]
    fn test_read_write_roundtrip() {
        let mut storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        let mut page = Page::new(PageType::Data, id);
        page.body_mut()[0] = 0xDE;
        page.body_mut()[1] = 0xAD;
        page.update_checksum();
        storage.write_page(id, &page).unwrap();
        let read = storage.read_page(id).unwrap();
        assert_eq!(read.body()[0], 0xDE);
        assert_eq!(read.body()[1], 0xAD);
    }

    #[test]
    fn test_write_invalid_checksum_rejected() {
        let mut storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        let mut page = Page::new(PageType::Data, id);
        page.body_mut()[0] = 0xFF; // without update_checksum
        assert!(storage.write_page(id, &page).is_err());
    }

    #[test]
    fn test_box_dyn_storage_engine() {
        // Verify that Box<dyn StorageEngine> compiles and works.
        let mut engine: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
        let id = engine.alloc_page(PageType::Data).unwrap();
        assert!(id >= 2);
        engine.flush().unwrap();
    }
}
