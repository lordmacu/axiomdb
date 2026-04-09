use std::sync::RwLock;

use axiomdb_core::error::DbError;

use crate::{
    engine::StorageEngine,
    freelist::FreeList,
    page::{Page, PageType, PAGE_SIZE},
};

/// Inner state, protected by a single `RwLock`.
///
/// Using one lock keeps `MemoryStorage` simple — it is used exclusively for
/// unit tests and benchmarks where single-threaded access dominates.
struct MemoryStorageInner {
    /// Flat page array. Slot `i` holds page `i`'s bytes directly — no Box, no Option.
    pages: Vec<Page>,
    /// `true` if slot `i` has been allocated and not yet freed.
    allocated: Vec<bool>,
    freelist: FreeList,
}

/// In-RAM storage engine — no I/O, ideal for unit tests and benchmarks.
///
/// ## Interior mutability (Phase 40.3)
///
/// All `StorageEngine` methods now take `&self`. State is protected by a single
/// `RwLock<MemoryStorageInner>`. For the single-threaded test use case the lock
/// adds minimal overhead (no contention → uncontended fast path).
///
/// ## Performance design
///
/// The inner `pages: Vec<Page>` is a flat array:
/// - `alloc_page` → zero-init one slot in place (no heap alloc)
/// - `write_page` → one 16 KB copy directly into the slot
/// - `read_page` → direct array index, no pointer indirection
pub struct MemoryStorage {
    inner: RwLock<MemoryStorageInner>,
    page_locks: crate::page_lock::PageLockTable,
}

impl MemoryStorage {
    /// Creates an empty storage with page 0 (Meta) initialized.
    pub fn new() -> Self {
        const INITIAL_PAGES: u64 = 64;
        let mut pages: Vec<Page> = (0..INITIAL_PAGES as usize)
            .map(|_| Page::new(PageType::Free, 0))
            .collect();
        pages[0] = Page::new(PageType::Meta, 0);

        let mut allocated = vec![false; INITIAL_PAGES as usize];
        allocated[0] = true;

        let freelist = FreeList::new(INITIAL_PAGES, &[0, 1]);
        MemoryStorage {
            inner: RwLock::new(MemoryStorageInner {
                pages,
                allocated,
                freelist,
            }),
            page_locks: crate::page_lock::PageLockTable::new(),
        }
    }
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageEngine for MemoryStorage {
    fn read_page(&self, page_id: u64) -> Result<crate::page_ref::PageRef, DbError> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let idx = page_id as usize;
        if idx >= inner.pages.len() || !inner.allocated[idx] {
            return Err(DbError::PageNotFound { page_id });
        }
        debug_assert!(
            inner.pages[idx].verify_checksum().is_ok(),
            "checksum mismatch in MemoryStorage — logic bug in write path"
        );
        let mut bytes = [0u8; PAGE_SIZE];
        bytes.copy_from_slice(inner.pages[idx].as_bytes());
        Ok(crate::page_ref::PageRef::from_bytes(bytes))
    }

    fn write_page(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        page.verify_checksum()?;
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let idx = page_id as usize;
        if idx >= inner.pages.len() {
            return Err(DbError::PageNotFound { page_id });
        }
        // Direct 16 KB copy into the flat array slot — no heap allocation.
        // SAFETY: both src and dst are valid Page slots. src (page) is passed
        // by the caller and does not alias inner.pages[idx] because write_page
        // takes &Page from outside, not &self.pages[idx].
        unsafe {
            std::ptr::copy_nonoverlapping(
                page as *const Page,
                &mut inner.pages[idx] as *mut Page,
                1,
            );
        }
        inner.allocated[idx] = true;
        Ok(())
    }

    fn alloc_page(&self, page_type: PageType) -> Result<u64, DbError> {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(page_id) = inner.freelist.alloc() {
            ensure_capacity(&mut inner, page_id);
            let idx = page_id as usize;
            inner.pages[idx] = Page::new(page_type, page_id);
            inner.allocated[idx] = true;
            return Ok(page_id);
        }
        let new_total = inner.freelist.total_pages() + 64;
        inner.freelist.grow(new_total);
        grow_arrays(&mut inner, new_total as usize);
        let page_id = inner.freelist.alloc().ok_or(DbError::Other(
            "freelist empty after grow — internal invariant violated".into(),
        ))?;
        let idx = page_id as usize;
        inner.pages[idx] = Page::new(page_type, page_id);
        inner.allocated[idx] = true;
        Ok(page_id)
    }

    fn free_page(&self, page_id: u64) -> Result<(), DbError> {
        if page_id == 0 || page_id == 1 {
            return Err(DbError::Other(format!(
                "cannot free reserved page {page_id}"
            )));
        }
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let idx = page_id as usize;
        if idx < inner.allocated.len() {
            inner.allocated[idx] = false;
        }
        inner.freelist.free(page_id)
    }

    fn flush(&self) -> Result<(), DbError> {
        Ok(())
    }

    fn page_count(&self) -> u64 {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .freelist
            .total_pages()
    }

    fn page_lock_table(&self) -> &crate::page_lock::PageLockTable {
        &self.page_locks
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn ensure_capacity(inner: &mut MemoryStorageInner, page_id: u64) {
    let idx = page_id as usize;
    if idx >= inner.pages.len() {
        grow_arrays(inner, idx + 1);
    }
}

fn grow_arrays(inner: &mut MemoryStorageInner, new_len: usize) {
    if new_len > inner.pages.len() {
        inner
            .pages
            .resize_with(new_len, || Page::new(PageType::Free, 0));
        inner.allocated.resize(new_len, false);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::run_storage_engine_suite;

    #[test]
    fn test_storage_engine_suite() {
        let storage = MemoryStorage::new();
        run_storage_engine_suite(&storage);
    }

    #[test]
    fn test_new_has_meta_page() {
        let storage = MemoryStorage::new();
        let page = storage.read_page(0).unwrap();
        assert_eq!(page.header().page_type, PageType::Meta as u8);
    }

    #[test]
    fn test_alloc_starts_from_2() {
        let storage = MemoryStorage::new();
        assert_eq!(storage.alloc_page(PageType::Data).unwrap(), 2);
        assert_eq!(storage.alloc_page(PageType::Data).unwrap(), 3);
    }

    #[test]
    fn test_free_and_realloc() {
        let storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.free_page(id).unwrap();
        assert_eq!(storage.alloc_page(PageType::Data).unwrap(), id);
    }

    #[test]
    fn test_double_free_is_error() {
        let storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.free_page(id).unwrap();
        assert!(storage.free_page(id).is_err());
    }

    #[test]
    fn test_free_reserved_is_error() {
        let storage = MemoryStorage::new();
        assert!(storage.free_page(0).is_err());
        assert!(storage.free_page(1).is_err());
    }

    #[test]
    fn test_alloc_grows_automatically() {
        let storage = MemoryStorage::new();
        let initial = storage.page_count();
        for _ in 0..(initial - 2) {
            storage.alloc_page(PageType::Data).unwrap();
        }
        let id = storage.alloc_page(PageType::Data).unwrap();
        assert!(id >= initial);
    }

    #[test]
    fn test_read_write_roundtrip() {
        let storage = MemoryStorage::new();
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
        let storage = MemoryStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        let mut page = Page::new(PageType::Data, id);
        page.body_mut()[0] = 0xFF; // without update_checksum
        assert!(storage.write_page(id, &page).is_err());
    }

    #[test]
    fn test_box_dyn_storage_engine() {
        let engine: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
        let id = engine.alloc_page(PageType::Data).unwrap();
        assert!(id >= 2);
        engine.flush().unwrap();
    }
}
