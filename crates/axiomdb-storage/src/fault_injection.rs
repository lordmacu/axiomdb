//! Fault-injection storage for crash-recovery (project B / REDO) tests.
//!
//! Models **power-loss durability**: only data made durable by `flush()` survives a
//! [`FaultInjectionStorage::simulate_power_loss`]. Writes since the last flush live
//! in a "current" layer that is reverted to the last durable snapshot on a simulated
//! crash.
//!
//! This is the **worst-case** model — nothing un-`fsync`'d survives — which is the
//! conservative, correct basis for testing that committed data is recoverable from
//! the WAL alone (a real engine must survive even if no un-fsync'd page reached
//! disk). It exists because mmap `MAP_SHARED` dirty pages survive even `SIGKILL`
//! (kernel page cache), so the real storage path cannot reproduce power loss
//! in-process.
//!
//! Pair it with a real WAL (real `fsync` at commit) and fault-inject only the data
//! pages: the WAL is durable at commit, the un-flushed data pages are lost on crash,
//! and recovery must REDO them.

use std::sync::RwLock;

use axiomdb_core::error::DbError;

use crate::{
    engine::StorageEngine,
    freelist::FreeList,
    page::{Page, PageType, PAGE_SIZE},
};

const INITIAL_PAGES: u64 = 64;

/// A complete page-store snapshot (mirrors `MemoryStorage`'s inner layout).
#[derive(Clone)]
struct Layer {
    pages: Vec<Page>,
    allocated: Vec<bool>,
    freelist: FreeList,
}

impl Layer {
    fn new() -> Self {
        let mut pages: Vec<Page> = (0..INITIAL_PAGES as usize)
            .map(|_| Page::new(PageType::Free, 0))
            .collect();
        pages[0] = Page::new(PageType::Meta, 0);
        let mut allocated = vec![false; INITIAL_PAGES as usize];
        allocated[0] = true;
        let freelist = FreeList::new(INITIAL_PAGES, &[0, 1]);
        Layer {
            pages,
            allocated,
            freelist,
        }
    }

    fn ensure_capacity(&mut self, page_id: u64) {
        let idx = page_id as usize;
        if idx >= self.pages.len() {
            self.grow(idx + 1);
        }
    }

    fn grow(&mut self, new_len: usize) {
        if new_len > self.pages.len() {
            self.pages
                .resize_with(new_len, || Page::new(PageType::Free, 0));
            self.allocated.resize(new_len, false);
        }
    }
}

/// `current` is the live state; `durable` is the last `flush()`ed snapshot that a
/// simulated power loss reverts to.
struct State {
    current: Layer,
    durable: Layer,
}

/// In-RAM storage that models power-loss durability for crash-recovery tests.
pub struct FaultInjectionStorage {
    state: RwLock<State>,
    page_locks: crate::page_lock::PageLockTable,
}

impl FaultInjectionStorage {
    /// Empty storage with page 0 (Meta) initialized, already durable.
    pub fn new() -> Self {
        let current = Layer::new();
        let durable = current.clone();
        FaultInjectionStorage {
            state: RwLock::new(State { current, durable }),
            page_locks: crate::page_lock::PageLockTable::new(),
        }
    }

    /// Simulates a power loss: every write since the last [`StorageEngine::flush`]
    /// is discarded; the durable snapshot becomes the live state. The fsync'd WAL
    /// (separate) is unaffected — recovery must REDO the lost committed pages.
    pub fn simulate_power_loss(&self) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.current = state.durable.clone();
    }

    fn write_page_inner(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        page.verify_checksum()?;
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.current.ensure_capacity(page_id);
        let idx = page_id as usize;
        state.current.pages[idx] = page.clone();
        state.current.allocated[idx] = true;
        Ok(())
    }
}

impl Default for FaultInjectionStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageEngine for FaultInjectionStorage {
    fn read_page(&self, page_id: u64) -> Result<crate::page_ref::PageRef, DbError> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        let idx = page_id as usize;
        if idx >= state.current.pages.len() || !state.current.allocated[idx] {
            return Err(DbError::PageNotFound { page_id });
        }
        let mut bytes = [0u8; PAGE_SIZE];
        bytes.copy_from_slice(state.current.pages[idx].as_bytes());
        Ok(crate::page_ref::PageRef::from_bytes(bytes))
    }

    fn read_page_raw(&self, page_id: u64) -> Result<[u8; PAGE_SIZE], DbError> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        let idx = page_id as usize;
        if idx >= state.current.pages.len() || !state.current.allocated[idx] {
            return Err(DbError::PageNotFound { page_id });
        }
        let mut bytes = [0u8; PAGE_SIZE];
        bytes.copy_from_slice(state.current.pages[idx].as_bytes());
        Ok(bytes)
    }

    fn write_page(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        let _page_guard = self.page_locks.write(page_id);
        self.write_page_inner(page_id, page)
    }

    fn write_page_under_page_lock(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        self.write_page_inner(page_id, page)
    }

    fn alloc_page(&self, page_type: PageType) -> Result<u64, DbError> {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        let cur = &mut state.current;
        if let Some(page_id) = cur.freelist.alloc() {
            cur.ensure_capacity(page_id);
            let idx = page_id as usize;
            cur.pages[idx] = Page::new(page_type, page_id);
            cur.allocated[idx] = true;
            return Ok(page_id);
        }
        let new_total = cur.freelist.total_pages() + 64;
        cur.freelist.grow(new_total);
        cur.grow(new_total as usize);
        let page_id = cur.freelist.alloc().ok_or(DbError::Other(
            "freelist empty after grow — internal invariant violated".into(),
        ))?;
        let idx = page_id as usize;
        cur.pages[idx] = Page::new(page_type, page_id);
        cur.allocated[idx] = true;
        Ok(page_id)
    }

    fn free_page(&self, page_id: u64) -> Result<(), DbError> {
        if page_id == 0 || page_id == 1 {
            return Err(DbError::Other(format!(
                "cannot free reserved page {page_id}"
            )));
        }
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        let idx = page_id as usize;
        if idx < state.current.allocated.len() {
            state.current.allocated[idx] = false;
        }
        state.current.freelist.free(page_id)
    }

    /// fsync model: the live state becomes durable (survives a later power loss).
    fn flush(&self) -> Result<(), DbError> {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.durable = state.current.clone();
        Ok(())
    }

    fn page_count(&self) -> u64 {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .freelist
            .total_pages()
    }

    fn page_lock_table(&self) -> &crate::page_lock::PageLockTable {
        &self.page_locks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::run_storage_engine_suite;

    fn data_page(id: u64, marker: u8) -> Page {
        let mut p = Page::new(PageType::Data, id);
        p.body_mut()[0] = marker;
        p.update_checksum();
        p
    }

    #[test]
    fn satisfies_storage_engine_contract_pre_crash() {
        // Before any crash it behaves like a normal in-memory engine.
        let storage = FaultInjectionStorage::new();
        run_storage_engine_suite(&storage);
    }

    #[test]
    fn write_is_visible_before_flush() {
        let storage = FaultInjectionStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.write_page(id, &data_page(id, 0xAA)).unwrap();
        assert_eq!(storage.read_page(id).unwrap().body()[0], 0xAA);
    }

    #[test]
    fn flushed_write_survives_power_loss() {
        let storage = FaultInjectionStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.write_page(id, &data_page(id, 0xAA)).unwrap();
        storage.flush().unwrap();
        storage.simulate_power_loss();
        assert_eq!(
            storage.read_page(id).unwrap().body()[0],
            0xAA,
            "a flushed write must survive a power loss"
        );
    }

    #[test]
    fn unflushed_write_is_lost_on_power_loss() {
        let storage = FaultInjectionStorage::new();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.write_page(id, &data_page(id, 0xAA)).unwrap();
        storage.flush().unwrap(); // 0xAA is now durable

        // A second write WITHOUT flush — the un-fsync'd change.
        storage.write_page(id, &data_page(id, 0xBB)).unwrap();
        assert_eq!(storage.read_page(id).unwrap().body()[0], 0xBB);

        storage.simulate_power_loss();
        assert_eq!(
            storage.read_page(id).unwrap().body()[0],
            0xAA,
            "an un-flushed write must be lost; the prior durable state survives"
        );
    }

    #[test]
    fn page_allocated_after_last_flush_is_gone_on_power_loss() {
        let storage = FaultInjectionStorage::new();
        storage.flush().unwrap(); // durable baseline (only meta page)
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.write_page(id, &data_page(id, 0x7F)).unwrap();
        // No flush after allocating `id`.
        storage.simulate_power_loss();
        assert!(
            matches!(storage.read_page(id), Err(DbError::PageNotFound { .. })),
            "a page allocated after the last flush must not exist post-crash"
        );
    }

    #[test]
    fn flush_then_more_writes_then_crash_reverts_to_flush_point() {
        let storage = FaultInjectionStorage::new();
        let a = storage.alloc_page(PageType::Data).unwrap();
        storage.write_page(a, &data_page(a, 1)).unwrap();
        storage.flush().unwrap();

        let b = storage.alloc_page(PageType::Data).unwrap();
        storage.write_page(b, &data_page(b, 2)).unwrap();
        storage.simulate_power_loss();

        assert_eq!(storage.read_page(a).unwrap().body()[0], 1, "a survives");
        assert!(
            matches!(storage.read_page(b), Err(DbError::PageNotFound { .. })),
            "b (post-flush alloc) is gone"
        );
    }
}
