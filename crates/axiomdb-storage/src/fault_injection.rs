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

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use axiomdb_core::error::DbError;

use crate::{
    engine::StorageEngine,
    freelist::FreeList,
    page::{Page, PageType, PAGE_SIZE},
    wal_frame::{FrameLog, RecycleMode},
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
    /// Durable page-frame redo log (project B subphase 5). `None` ⇒ redo disabled
    /// ⇒ behaves as a plain in-RAM engine. Set once by [`enable_redo_log`]. A
    /// simulated power loss never touches this file — modelling a real fsync'd WAL.
    ///
    /// [`enable_redo_log`]: FaultInjectionStorage::enable_redo_log
    frame_log: Option<FrameLog>,
    /// Monotonic LSN stamped into each written page (`0` while redo is disabled).
    frame_lsn: AtomicU64,
    /// Excludes frame appends during a checkpoint's log recycle (subphase 6b). Appends
    /// take the read side; `checkpoint_frames` takes the write side.
    checkpoint_lock: RwLock<()>,
}

impl FaultInjectionStorage {
    /// Empty storage with page 0 (Meta) initialized, already durable.
    pub fn new() -> Self {
        let current = Layer::new();
        let durable = current.clone();
        FaultInjectionStorage {
            state: RwLock::new(State { current, durable }),
            page_locks: crate::page_lock::PageLockTable::new(),
            frame_log: None,
            frame_lsn: AtomicU64::new(0),
            checkpoint_lock: RwLock::new(()),
        }
    }

    /// Enables a durable page-frame redo log at `path` (project B subphase 5),
    /// mirroring [`MmapStorage::enable_redo_log`]. Call before any write. The log's
    /// file survives [`simulate_power_loss`] while volatile data pages revert, so
    /// recovery can REDO committed frames the data pages lost.
    ///
    /// [`MmapStorage::enable_redo_log`]: crate::MmapStorage::enable_redo_log
    /// [`simulate_power_loss`]: FaultInjectionStorage::simulate_power_loss
    pub fn enable_redo_log(&mut self, path: &Path) -> Result<(), DbError> {
        self.frame_log = Some(FrameLog::create(path)?);
        self.frame_lsn.store(1, Ordering::Relaxed);
        Ok(())
    }

    /// (test) Whether the durable frame log holds a committed frame for `page_id`
    /// under `is_committed`. Asserts that fsync'd frames survive a simulated crash.
    pub fn frame_log_has_committed(
        &self,
        page_id: u64,
        is_committed: &dyn Fn(u64) -> bool,
    ) -> bool {
        match &self.frame_log {
            Some(fl) => fl
                .build_index(is_committed)
                .map(|idx| idx.latest(page_id).is_some())
                .unwrap_or(false),
            None => false,
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
        if let Some(frame_log) = &self.frame_log {
            // Redo on: stamp the page LSN and append a frame stamped with the current
            // txn (mirrors MmapStorage). The data page stays volatile until flush();
            // the appended frame is made durable by sync_frame_log at the commit.
            // Hold the checkpoint read-guard so a checkpoint's recycle can't race it.
            let _ck = self
                .checkpoint_lock
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let lsn = self.frame_lsn.fetch_add(1, Ordering::Relaxed);
            let mut stamped = page.clone();
            stamped.set_lsn(lsn);
            let txn_id = crate::txn_stamp::get();
            frame_log.append(page_id, lsn, txn_id, stamped.as_bytes())?;
            let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
            state.current.ensure_capacity(page_id);
            let idx = page_id as usize;
            state.current.pages[idx] = stamped;
            state.current.allocated[idx] = true;
        } else {
            let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
            state.current.ensure_capacity(page_id);
            let idx = page_id as usize;
            state.current.pages[idx] = page.clone();
            state.current.allocated[idx] = true;
        }
        Ok(())
    }

    /// Applies every committed frame into BOTH the current and durable layers — shared by
    /// recovery REDO and the checkpoint. The frame is a full page image; absent pages grow
    /// via `ensure_capacity`. Returns the pages applied.
    fn apply_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
        let Some(frame_log) = &self.frame_log else {
            return Ok(0);
        };
        let index = frame_log.build_index(is_committed)?;
        let mut applied = 0usize;
        let mut guard = self.state.write().unwrap_or_else(|e| e.into_inner());
        // One deref to &mut State so `current` and `durable` are disjoint field borrows.
        let st = &mut *guard;
        for frame in index.frames() {
            let idx = frame.page_id as usize;
            let page_lsn = if idx < st.current.pages.len() && st.current.allocated[idx] {
                st.current.pages[idx].lsn()
            } else {
                0 // page lost on crash (allocated after the last flush) ⇒ apply.
            };
            if frame.lsn > page_lsn {
                let page = Page::from_bytes(*frame_log.read_page_at(frame.offset)?)?;
                // Apply into BOTH layers so the restored page is itself durable.
                for layer in [&mut st.current, &mut st.durable] {
                    layer.ensure_capacity(frame.page_id);
                    layer.pages[idx] = page.clone();
                    layer.allocated[idx] = true;
                }
                applied += 1;
            }
        }
        Ok(applied)
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

    fn set_current_txn(&self, txn_id: u64) {
        crate::txn_stamp::set(txn_id);
    }

    fn current_txn(&self) -> u64 {
        crate::txn_stamp::get()
    }

    fn sync_frame_log(&self) -> Result<(), DbError> {
        match &self.frame_log {
            Some(fl) => fl.sync_to_durable(),
            None => Ok(()),
        }
    }

    fn redo_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
        self.apply_committed_frames(is_committed)
    }

    fn checkpoint_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
        let Some(frame_log) = &self.frame_log else {
            return Ok(0);
        };
        // Exclusive guard (model A): drains in-flight appends, blocks new ones until the
        // recycle completes. The apply already wrote both layers, so the data is durable
        // before the log is reset.
        let _ckpt = self
            .checkpoint_lock
            .write()
            .unwrap_or_else(|e| e.into_inner());
        // `txn_id == 0` (system write) counts as committed for apply + recycle.
        let committed = |t: u64| t == 0 || is_committed(t);
        // 6d: fsync the frame log BEFORE applying to main (walCheckpoint order). Under NORMAL
        // the per-commit frame fsync is deferred to here.
        frame_log.sync_to_durable()?;
        let applied = self.apply_committed_frames(&committed)?;
        // In-flight-safe: only recycle when every frame is committed.
        if frame_log.scan()?.iter().all(|f| committed(f.txn_id)) {
            frame_log.recycle(RecycleMode::Reuse)?;
        }
        Ok(applied)
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

    #[test]
    fn frame_log_survives_power_loss_while_data_page_reverts() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = FaultInjectionStorage::new();
        storage.enable_redo_log(&dir.path().join("fi.wf")).unwrap();

        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.write_page(id, &data_page(id, 0xAA)).unwrap(); // baseline, txn 0
        storage.flush().unwrap();

        storage.set_current_txn(5);
        storage.write_page(id, &data_page(id, 0xBB)).unwrap(); // committed-row write
        storage.sync_frame_log().unwrap();
        storage.set_current_txn(0);

        storage.simulate_power_loss();
        // The volatile data page reverted to the durable baseline …
        assert_eq!(storage.read_page(id).unwrap().body()[0], 0xAA);
        // … but the fsync'd frame for txn 5 is still in the (separate) frame log.
        assert!(storage.frame_log_has_committed(id, &|t| t == 5));
    }

    #[test]
    fn concurrent_commits_are_all_durable() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let mut storage = FaultInjectionStorage::new();
        storage.enable_redo_log(&dir.path().join("cc.wf")).unwrap();
        let storage = Arc::new(storage);

        let threads = 8u64;
        let mut handles = Vec::new();
        for t in 0..threads {
            let s = Arc::clone(&storage);
            handles.push(std::thread::spawn(move || {
                let id = s.alloc_page(PageType::Data).unwrap();
                s.set_current_txn(t + 1);
                s.write_page(id, &data_page(id, (t & 0xFF) as u8)).unwrap();
                s.sync_frame_log().unwrap(); // commit boundary: returns only when durable
                (t + 1, id)
            }));
        }
        let committed: Vec<(u64, u64)> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Every committed frame survives in the gap-free durable prefix: a swallowed gap
        // would make `scan` (inside build_index) stop early and drop some page.
        for (txn, id) in committed {
            assert!(
                storage.frame_log_has_committed(id, &|t| t == txn),
                "committed frame for txn {txn} (page {id}) must be in the durable prefix"
            );
        }
    }

    #[test]
    fn redo_restores_committed_row_after_power_loss_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = FaultInjectionStorage::new();
        storage.enable_redo_log(&dir.path().join("fi.wf")).unwrap();

        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.write_page(id, &data_page(id, 0xAA)).unwrap(); // baseline txn 0
        storage.flush().unwrap();

        storage.set_current_txn(5);
        storage.write_page(id, &data_page(id, 0xBB)).unwrap(); // committed row
        storage.sync_frame_log().unwrap();
        storage.set_current_txn(0);
        storage.simulate_power_loss();
        assert_eq!(
            storage.read_page(id).unwrap().body()[0],
            0xAA,
            "the committed row was lost on the crash"
        );

        // REDO restores it.
        assert_eq!(storage.redo_committed_frames(&|t| t == 5).unwrap(), 1);
        assert_eq!(
            storage.read_page(id).unwrap().body()[0],
            0xBB,
            "committed row restored by REDO"
        );
        // Crash *during* recovery: a second pass is a no-op (pageLSN guard).
        assert_eq!(storage.redo_committed_frames(&|t| t == 5).unwrap(), 0);
        // An uncommitted txn's frame is never applied.
        assert_eq!(storage.redo_committed_frames(&|_| false).unwrap(), 0);
    }

    #[test]
    fn checkpoint_applies_committed_then_recycles() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = FaultInjectionStorage::new();
        storage.enable_redo_log(&dir.path().join("ck.wf")).unwrap();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.write_page(id, &data_page(id, 0xAA)).unwrap(); // baseline txn 0
        storage.flush().unwrap();
        storage.set_current_txn(5);
        storage.write_page(id, &data_page(id, 0xBB)).unwrap(); // committed row
        storage.sync_frame_log().unwrap();
        storage.set_current_txn(0);
        storage.simulate_power_loss();
        assert_eq!(
            storage.read_page(id).unwrap().body()[0],
            0xAA,
            "row lost on crash"
        );

        assert_eq!(storage.checkpoint_frames(&|t| t == 5).unwrap(), 1);
        assert_eq!(
            storage.read_page(id).unwrap().body()[0],
            0xBB,
            "checkpoint applied the committed frame"
        );
        // Log recycled → the committed frame is no longer in the log.
        assert!(
            !storage.frame_log_has_committed(id, &|t| t == 5),
            "checkpoint recycled the log"
        );
        assert_eq!(
            storage.checkpoint_frames(&|t| t == 5).unwrap(),
            0,
            "nothing left to apply after the recycle"
        );
    }

    #[test]
    fn checkpoint_excludes_concurrent_writers_safely() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let mut storage = FaultInjectionStorage::new();
        storage.enable_redo_log(&dir.path().join("cck.wf")).unwrap();
        let storage = Arc::new(storage);

        let mut handles = Vec::new();
        // A checkpoint thread racing the writers must never panic/deadlock or lose data.
        {
            let s = Arc::clone(&storage);
            handles.push(std::thread::spawn(move || {
                for _ in 0..10 {
                    s.checkpoint_frames(&|_| true).unwrap();
                }
                (0u64, u64::MAX, 0u8) // sentinel for the checkpoint thread
            }));
        }
        for t in 1..=8u64 {
            let s = Arc::clone(&storage);
            handles.push(std::thread::spawn(move || {
                let id = s.alloc_page(PageType::Data).unwrap();
                s.set_current_txn(t);
                let marker = (t & 0xFF) as u8;
                s.write_page(id, &data_page(id, marker)).unwrap();
                s.sync_frame_log().unwrap();
                (t, id, marker)
            }));
        }
        let results: Vec<(u64, u64, u8)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // Every committed page's data survives in the live state — a concurrent recycle
        // must not lose it (the data is in the layers; the log is just a redo source).
        for (t, id, marker) in results {
            if id == u64::MAX {
                continue; // the checkpoint thread's sentinel
            }
            assert_eq!(
                storage.read_page(id).unwrap().body()[0],
                marker,
                "committed page {id} (txn {t}) lost under a concurrent checkpoint"
            );
        }
    }
}
