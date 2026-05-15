use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Mutex, RwLock,
    },
};

use axiomdb_core::error::{classify_io, DbError};
use fs2::FileExt;
use libc;
use memmap2::Mmap;
use tracing::{debug, info, warn};

use crate::{
    dirty::PageDirtyTracker,
    doublewrite::DoublewriteBuffer,
    engine::StorageEngine,
    freelist::FreeList,
    page::{Page, PageType, HEADER_SIZE, PAGE_SIZE},
    page_lock::PageLockTable,
};

// ── Constants ─────────────────────────────────────────────────────────────────

const DB_FILE_MAGIC: u64 = 0x4158494F_4D444201; // "AXIOMDB\1"
const DB_VERSION: u32 = 1;
/// Growth unit: 64 pages = 1 MB.
const GROW_PAGES: u64 = 64;
/// Number of pages to prefetch when the caller passes `count = 0`.
/// 64 × 16 KB = 1 MB — matches one `GROW_PAGES` growth unit.
const PREFETCH_DEFAULT_PAGES: u64 = 64;
/// Log a warning if the deferred-free queue exceeds this many entries.
const DEFERRED_FREE_WARN_THRESHOLD: usize = 4096;

// Fixed offsets for in-place updates without re-parsing the full meta page.
// PageHeader(64) + db_magic(8) + version(4) + _pad(4) = 80
const PAGE_COUNT_OFFSET: usize = HEADER_SIZE + 8 + 4 + 4;
const CLEAN_SHUTDOWN_OFFSET: usize = HEADER_SIZE + crate::meta::CLEAN_SHUTDOWN_BODY_OFFSET;
// Offset of the `checksum` field inside PageHeader.
const CHECKSUM_OFFSET: usize = 12;

// ── DbFileMeta ────────────────────────────────────────────────────────────────

/// File metadata stored in the body of page 0.
/// Occupies exactly `PAGE_SIZE - HEADER_SIZE` bytes.
#[repr(C)]
struct DbFileMeta {
    db_magic: u64,
    version: u32,
    _pad: u32,
    page_count: u64,
    _reserved: [u8; PAGE_SIZE - HEADER_SIZE - 24],
}

const _: () = assert!(
    std::mem::size_of::<DbFileMeta>() == PAGE_SIZE - HEADER_SIZE,
    "DbFileMeta must fill exactly the body of one page"
);

// ── MmapStorage ───────────────────────────────────────────────────────────────

/// mmap-based storage engine with interior mutability.
///
/// File layout:
/// - Page 0: Meta (`DbFileMeta` in body)
/// - Page 1: Free list bitmap (`FreeList` serialized)
/// - Pages 2+: Data, Index, Overflow, etc.
///
/// ## Concurrency model (Phase 40.3)
///
/// All `StorageEngine` methods take `&self`. Mutable state is managed internally:
///
/// | Field | Protection | Contention |
/// |---|---|---|
/// | `mmap` | `RwLock` — write only during grow | Minimal (grow is rare) |
/// | `freelist` | `Mutex` — held only during bitmap scan | ~1µs per alloc/free |
/// | `deferred_frees` | `Mutex` | ~1µs per free |
/// | `freelist_dirty` | `AtomicBool` | Zero contention |
/// | `current_snapshot_id` | `AtomicU64` | Zero contention |
/// | `page_count` | `AtomicU64` | Zero contention |
/// | `dirty` | Existing `PageDirtyTracker` (atomic) | Zero contention |
/// | `dw_buffer` | `Mutex` | Held only during flush |
/// | `page_locks` | `PageLockTable` (64-shard) | Per-page, not global |
/// | `grow_lock` | `Mutex<()>` | Held only during file growth |
///
/// ## Lock ordering (deadlock prevention)
///
/// ```text
/// grow_lock > mmap.write > freelist > deferred_frees > page_lock
/// ```
///
/// No code path acquires a later lock then an earlier lock → no cycles possible.
///
/// The `.db` file is locked with `flock(LOCK_EX)` on open and released on drop.
pub struct MmapStorage {
    /// Read-only memory mapping — read lock for reads, write lock only during grow.
    mmap: RwLock<Mmap>,
    /// File descriptor for `pwrite()` and file length extension.
    /// `pwrite()` is thread-safe at the OS level; no additional lock needed.
    file: File,
    /// In-memory free list. Persisted lazily to page 1 on `flush()`.
    freelist: Mutex<FreeList>,
    /// Set when the freelist was modified and needs to be written on the next flush.
    freelist_dirty: AtomicBool,
    /// Tracks pages written since the last flush. Cleared on `flush()`.
    dirty: PageDirtyTracker,
    /// Pages freed but not yet safe to reuse (MVCC deferred free, Phase 7.4a).
    deferred_frees: Mutex<Vec<(u64, u64)>>,
    /// Snapshot epoch at which the current statement began (used to tag deferred frees).
    current_snapshot_id: AtomicU64,
    /// Doublewrite buffer for torn-page repair (Phase 3.8c).
    dw_buffer: Mutex<DoublewriteBuffer>,
    /// Per-page RwLock table (InnoDB-inspired, 64 shards).
    /// Two transactions writing different pages acquire different locks → parallel.
    page_locks: PageLockTable,
    /// Prevents concurrent file extension (grow). Held only when the freelist
    /// is exhausted and the file must be extended.
    grow_lock: Mutex<()>,
    /// Authoritative page count. `AtomicU64` avoids acquiring the mmap read
    /// lock on every `read_page` call (hot path).
    page_count: AtomicU64,
    /// Phase 40.9: lock-free queue of pages freed by committed transactions.
    /// Drained first by `alloc_page_batch` for fast reuse without touching
    /// the bitmap. Folded back into the bitmap during `release_deferred_frees`.
    recycle_queue: crossbeam_queue::SegQueue<u64>,
    /// Phase 40.9: threads currently blocked (or about to block) on the
    /// freelist `Mutex`. Used by `LocalPageBatch::pop_or_refill` to scale
    /// the refill batch size adaptively under contention.
    extension_waiters: AtomicU32,
    /// Snapshot of the clean-shutdown flag as it was found on disk at open
    /// time, before this process marked the database as running/dirty.
    clean_shutdown_on_open: bool,
}

impl Drop for MmapStorage {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            if let Err(e) = self.write_clean_shutdown_flag(true).and_then(|_| {
                self.file
                    .sync_all()
                    .map_err(|e| classify_io(e, "storage close fsync"))
            }) {
                warn!(error = %e, "failed to mark clean shutdown on close");
            }
        }
        if let Err(e) = self.file.unlock() {
            warn!(error = %e, "failed to release file lock on close");
        } else {
            debug!("file lock released");
        }
    }
}

impl MmapStorage {
    /// Creates a new database file at `path`. Fails if the file already exists.
    pub fn create(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;

        file.try_lock_exclusive().map_err(|_| DbError::FileLocked {
            path: path.to_owned(),
        })?;

        info!(path = %path.display(), pages = GROW_PAGES, "creating database");

        let initial_size = GROW_PAGES * PAGE_SIZE as u64;
        file.set_len(initial_size)
            .map_err(|e| classify_io(e, "storage create"))?;

        // Write page 0 (Meta) via pwrite.
        {
            let meta_page = Self::build_meta_page(GROW_PAGES);
            use std::os::unix::fs::FileExt;
            file.write_all_at(meta_page.as_bytes(), 0)
                .map_err(|e| classify_io(e, "create meta pwrite"))?;
        }

        // Initialize FreeList: pages 0 and 1 reserved (meta + bitmap).
        let freelist = FreeList::new(GROW_PAGES, &[0, 1]);

        // Write page 1 (bitmap) via pwrite.
        {
            let mut bitmap_page = Page::new(PageType::Free, 1);
            freelist.to_bytes(bitmap_page.body_mut());
            bitmap_page.update_checksum();
            use std::os::unix::fs::FileExt;
            file.write_all_at(bitmap_page.as_bytes(), PAGE_SIZE as u64)
                .map_err(|e| classify_io(e, "create freelist pwrite"))?;
        }

        file.sync_all()
            .map_err(|e| classify_io(e, "create fsync"))?;

        // SAFETY: file fully written and synced. No mutable aliases.
        let mmap = unsafe { Mmap::map(&file)? };

        debug!(path = %path.display(), "database initialized and ready");
        let dw_buffer = DoublewriteBuffer::for_db(path);
        Ok(MmapStorage {
            mmap: RwLock::new(mmap),
            file,
            freelist: Mutex::new(freelist),
            freelist_dirty: AtomicBool::new(false),
            dirty: PageDirtyTracker::new(),
            deferred_frees: Mutex::new(Vec::new()),
            current_snapshot_id: AtomicU64::new(0),
            dw_buffer: Mutex::new(dw_buffer),
            page_locks: PageLockTable::new(),
            grow_lock: Mutex::new(()),
            page_count: AtomicU64::new(GROW_PAGES),
            recycle_queue: crossbeam_queue::SegQueue::new(),
            extension_waiters: AtomicU32::new(0),
            clean_shutdown_on_open: true,
        })
    }

    /// Opens an existing database file at `path`.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        file.try_lock_exclusive().map_err(|_| DbError::FileLocked {
            path: path.to_owned(),
        })?;

        info!(path = %path.display(), "opening database");

        // ── Doublewrite recovery (Phase 3.8c) ───────────────────────────
        let dw_buffer = DoublewriteBuffer::for_db(path);
        if dw_buffer.exists() {
            match dw_buffer.recover(&file) {
                Ok(n) if n > 0 => {
                    info!(repaired = n, "doublewrite recovery completed");
                }
                Ok(_) => {
                    debug!("doublewrite file found but no repairs needed");
                }
                Err(e) => {
                    warn!(error = %e, "doublewrite recovery failed, continuing");
                }
            }
            if let Err(e) = dw_buffer.cleanup() {
                warn!(error = %e, "failed to remove doublewrite file after recovery");
            }
        }

        // SAFETY: existing file, exclusive lock held. Read-only mapping.
        let mmap = unsafe { Mmap::map(&file)? };

        // Validate page 0.
        let (db_page_count, clean_shutdown_on_open) = {
            let meta_page = Self::read_page_from_mmap(&mmap, 0)?;
            let file_meta = Self::parse_file_meta(&meta_page);

            if file_meta.db_magic != DB_FILE_MAGIC {
                return Err(DbError::Other(format!(
                    "invalid file: expected db_magic {:#018x}, got {:#018x}",
                    DB_FILE_MAGIC, file_meta.db_magic
                )));
            }
            if file_meta.version != DB_VERSION {
                return Err(DbError::Other(format!(
                    "unsupported file version: {}",
                    file_meta.version
                )));
            }
            (
                file_meta.page_count,
                meta_page.body()[crate::meta::CLEAN_SHUTDOWN_BODY_OFFSET] != 0,
            )
        };

        // Load FreeList from page 1.
        let freelist = {
            let bitmap_page = Self::read_page_from_mmap(&mmap, 1)?;
            FreeList::from_bytes(bitmap_page.body(), db_page_count)
        };

        // Verify every allocated page's checksum before accepting any traffic.
        for page_id in 2..db_page_count {
            if !freelist.is_free(page_id) {
                Self::read_page_from_mmap(&mmap, page_id)?;
            }
        }

        info!(path = %path.display(), page_count = db_page_count, "database opened");
        debug!(
            free_pages = freelist.free_count(),
            "freelist loaded from disk"
        );

        let storage = MmapStorage {
            mmap: RwLock::new(mmap),
            file,
            freelist: Mutex::new(freelist),
            freelist_dirty: AtomicBool::new(false),
            dirty: PageDirtyTracker::new(),
            deferred_frees: Mutex::new(Vec::new()),
            current_snapshot_id: AtomicU64::new(0),
            dw_buffer: Mutex::new(dw_buffer),
            page_locks: PageLockTable::new(),
            grow_lock: Mutex::new(()),
            page_count: AtomicU64::new(db_page_count),
            recycle_queue: crossbeam_queue::SegQueue::new(),
            extension_waiters: AtomicU32::new(0),
            clean_shutdown_on_open,
        };
        storage.write_clean_shutdown_flag(false)?;
        storage
            .file
            .sync_all()
            .map_err(|e| classify_io(e, "storage open fsync"))?;
        Ok(storage)
    }

    /// Releases deferred-free pages back to the freelist.
    ///
    /// Pages freed at snapshot ≤ `oldest_active_snapshot` are returned to the
    /// freelist immediately. Pass `u64::MAX` to release all pages (safe when
    /// no readers are active, e.g. during `flush()`).
    pub fn release_deferred_frees(&self, oldest_active_snapshot: u64) -> Result<(), DbError> {
        let mut deferred = self
            .deferred_frees
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let deferred_empty = deferred.is_empty();
        let mut freelist = self.freelist.lock().unwrap_or_else(|e| e.into_inner());

        // Step 1: release snapshot-safe deferred frees.
        if !deferred_empty {
            let mut retained = Vec::new();
            for (page_id, freed_at) in deferred.drain(..) {
                if freed_at <= oldest_active_snapshot {
                    freelist.free(page_id)?;
                    self.freelist_dirty.store(true, Ordering::Relaxed);
                } else {
                    retained.push((page_id, freed_at));
                }
            }
            *deferred = retained;
        }

        // Step 2 (Phase 40.9): fold recycled pages back into the bitmap.
        // The recycle_queue is lock-free so draining it under the freelist
        // mutex does NOT create a lock-ordering issue with alloc_page_batch
        // (which pops from the queue OUTSIDE the mutex, then takes the
        // mutex only for the bitmap path).
        let mut recycled = 0usize;
        while let Some(page_id) = self.recycle_queue.pop() {
            // Best-effort: skip pages already free (commit + rollback race
            // can push the same ID to both the recycle queue and the bitmap
            // if the executor code calls recycle_page AND free_page_batch on
            // the same set).
            if freelist.is_free(page_id) {
                continue;
            }
            if let Err(e) = freelist.free(page_id) {
                debug!(page_id, error = %e, "recycle_queue fold-back: skip");
                continue;
            }
            recycled += 1;
        }
        if recycled > 0 {
            self.freelist_dirty.store(true, Ordering::Relaxed);
        }

        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Extends the file and remaps. Does NOT touch the freelist bitmap.
    ///
    /// Caller must hold `grow_lock`. Freelist Mutex must NOT be held.
    /// Returns the old page count (first new page ID).
    fn do_grow(&self, extra_pages: u64) -> Result<u64, DbError> {
        let old_count = self.page_count.load(Ordering::Acquire);
        let new_count = old_count + extra_pages;
        debug!(old_count, new_count, extra_pages, "growing storage");
        let new_size = new_count * PAGE_SIZE as u64;

        self.file
            .set_len(new_size)
            .map_err(|e| classify_io(e, "storage grow"))?;

        // Remap: brief exclusive lock. Readers that called read_page while
        // the mmap write lock is held will wait here until the remap completes.
        // SAFETY: file extended to `new_size`. No other writer (grow_lock held).
        {
            let mut mmap_guard = self.mmap.write().unwrap_or_else(|e| e.into_inner());
            *mmap_guard = unsafe { Mmap::map(&self.file)? };
        }

        // Update page_count in meta page and atomically stored count.
        self.update_page_count_in_meta(new_count)?;
        self.page_count.store(new_count, Ordering::Release);

        // Grow the freelist bitmap to cover new pages.
        {
            let mut fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
            fl.grow(new_count);
        }
        self.pwrite_freelist()?;

        Ok(old_count)
    }

    /// Writes raw bytes at `offset` in the file via `pwrite()`.
    fn pwrite_bytes(&self, offset: u64, data: &[u8]) -> Result<(), DbError> {
        use std::os::unix::fs::FileExt;
        self.file
            .write_all_at(data, offset)
            .map_err(|e| classify_io(e, "pwrite"))
    }

    /// Writes a full page via `pwrite()` at the correct file offset.
    fn pwrite_page(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        let offset = page_id * PAGE_SIZE as u64;
        self.pwrite_bytes(offset, page.as_bytes())
    }

    fn write_page_inner(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        if page_id >= self.page_count.load(Ordering::Acquire) {
            return Err(DbError::PageNotFound { page_id });
        }
        #[cfg(debug_assertions)]
        {
            let body = page.body();
            let hdr_page_type = page.as_bytes()[8];
            if hdr_page_type == 2 && !body.is_empty() && body[0] == 0 {
                let num_keys = u16::from_le_bytes([body[2], body[3]]) as usize;
                let max_n = num_keys.min(223);
                for i in 0..max_n {
                    let len = body[8 + i];
                    assert!(
                        len as usize <= 64,
                        "write_page({page_id}): corrupt internal node key_lens[{i}]={len}>64, num_keys={num_keys}"
                    );
                }
            }
        }
        self.pwrite_page(page_id, page)?;
        self.dirty.mark(page_id);
        Ok(())
    }

    fn read_page_from_mmap(mmap: &Mmap, page_id: u64) -> Result<crate::page_ref::PageRef, DbError> {
        let offset = page_id as usize * PAGE_SIZE;
        if offset + PAGE_SIZE > mmap.len() {
            return Err(DbError::PageNotFound { page_id });
        }
        let mut bytes = [0u8; PAGE_SIZE];
        bytes.copy_from_slice(&mmap[offset..offset + PAGE_SIZE]);
        let page_ref = crate::page_ref::PageRef::from_bytes(bytes);
        page_ref.verify_checksum()?;
        Ok(page_ref)
    }

    /// Copies raw page bytes from the mmap (no CRC check). Acquires read lock.
    fn copy_raw_page_from_mmap(&self, page_id: u64) -> Result<[u8; PAGE_SIZE], DbError> {
        let mmap = self.mmap.read().unwrap_or_else(|e| e.into_inner());
        let offset = page_id as usize * PAGE_SIZE;
        if offset + PAGE_SIZE > mmap.len() {
            return Err(DbError::PageNotFound { page_id });
        }
        let mut bytes = [0u8; PAGE_SIZE];
        bytes.copy_from_slice(&mmap[offset..offset + PAGE_SIZE]);
        Ok(bytes)
    }

    /// Builds a meta page (page 0) with the given page count.
    fn build_meta_page(page_count: u64) -> Page {
        let mut meta_page = Page::new(PageType::Meta, 0);
        let file_meta = DbFileMeta {
            db_magic: DB_FILE_MAGIC,
            version: DB_VERSION,
            _pad: 0,
            page_count,
            _reserved: [0u8; PAGE_SIZE - HEADER_SIZE - 24],
        };
        // SAFETY: body and DbFileMeta have the same size (const assert).
        unsafe {
            std::ptr::copy_nonoverlapping(
                &file_meta as *const DbFileMeta as *const u8,
                meta_page.body_mut().as_mut_ptr(),
                PAGE_SIZE - HEADER_SIZE,
            );
        }
        meta_page.update_checksum();
        meta_page
    }

    /// Builds the freelist bitmap page from the locked freelist.
    fn build_freelist_page_from(fl: &FreeList) -> Page {
        let mut page = Page::new(PageType::Free, 1);
        fl.to_bytes(page.body_mut());
        page.update_checksum();
        page
    }

    /// Writes the in-memory freelist to page 1 via `pwrite()`.
    fn pwrite_freelist(&self) -> Result<(), DbError> {
        let fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
        let page = Self::build_freelist_page_from(&fl);
        drop(fl); // release before pwrite (no lock needed for I/O)
        self.pwrite_page(1, &page)
    }

    fn parse_file_meta(page: &Page) -> &DbFileMeta {
        // SAFETY: body has PAGE_SIZE-HEADER_SIZE bytes = size_of::<DbFileMeta>()
        // (const assert). DbFileMeta is repr(C) with no padding.
        unsafe { &*(page.body().as_ptr() as *const DbFileMeta) }
    }

    /// Reads the current meta page from mmap, updates page_count, and pwrites.
    fn update_page_count_in_meta(&self, count: u64) -> Result<(), DbError> {
        // Read current bytes from mmap (read lock briefly).
        let mut bytes = {
            let mmap = self.mmap.read().unwrap_or_else(|e| e.into_inner());
            let mut b = [0u8; PAGE_SIZE];
            b.copy_from_slice(&mmap[0..PAGE_SIZE]);
            b
        };
        // Patch page_count and recompute checksum.
        bytes[PAGE_COUNT_OFFSET..PAGE_COUNT_OFFSET + 8].copy_from_slice(&count.to_le_bytes());
        let checksum = crc32c::crc32c(&bytes[HEADER_SIZE..PAGE_SIZE]);
        bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
        self.pwrite_bytes(0, &bytes)
    }

    fn write_clean_shutdown_flag(&self, clean: bool) -> Result<(), DbError> {
        let mut bytes = {
            let mmap = self.mmap.read().unwrap_or_else(|e| e.into_inner());
            let mut b = [0u8; PAGE_SIZE];
            b.copy_from_slice(&mmap[0..PAGE_SIZE]);
            b
        };
        bytes[CLEAN_SHUTDOWN_OFFSET] = u8::from(clean);
        let checksum = crc32c::crc32c(&bytes[HEADER_SIZE..PAGE_SIZE]);
        bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
        self.pwrite_bytes(0, &bytes)
    }

    /// Returns the clean-shutdown flag as it was found on disk when this
    /// process opened the database file.
    pub fn clean_shutdown_on_open(&self) -> bool {
        self.clean_shutdown_on_open
    }
}

// ── StorageEngine impl ────────────────────────────────────────────────────────

impl StorageEngine for MmapStorage {
    fn read_page(&self, page_id: u64) -> Result<crate::page_ref::PageRef, DbError> {
        // Hot path: page_count is an AtomicU64 — no lock needed.
        if page_id >= self.page_count.load(Ordering::Acquire) {
            return Err(DbError::PageNotFound { page_id });
        }
        // Acquire mmap read lock (shared — multiple concurrent reads proceed in parallel).
        let mmap = self.mmap.read().unwrap_or_else(|e| e.into_inner());
        Self::read_page_from_mmap(&mmap, page_id)
    }

    fn read_page_raw(&self, page_id: u64) -> Result<[u8; crate::PAGE_SIZE], DbError> {
        if page_id >= self.page_count.load(Ordering::Acquire) {
            return Err(DbError::PageNotFound { page_id });
        }
        self.copy_raw_page_from_mmap(page_id)
    }

    fn write_page(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        // Acquire per-page exclusive lock.
        // Two threads writing DIFFERENT pages: different locks → full parallelism.
        // Two threads writing SAME page: same lock → serialized → correctness.
        let _page_guard = self.page_locks.write(page_id);
        self.write_page_inner(page_id, page)
    }

    fn write_page_under_page_lock(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        self.write_page_inner(page_id, page)
    }

    fn alloc_page(&self, page_type: PageType) -> Result<u64, DbError> {
        // Fast path: try to allocate from the freelist (mutex held briefly).
        let maybe_id = {
            let mut fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
            fl.alloc()
        };

        let page_id = if let Some(id) = maybe_id {
            id
        } else {
            // Freelist exhausted — must grow.
            // Acquire grow_lock to prevent two threads from growing simultaneously.
            // The freelist mutex is NOT held here (released above).
            let _grow_guard = self.grow_lock.lock().unwrap_or_else(|e| e.into_inner());

            // Re-check under grow_lock: another thread may have grown between the
            // freelist check and this grow_lock acquisition.
            let maybe_retry = {
                let mut fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
                fl.alloc()
            };

            if let Some(id) = maybe_retry {
                id
            } else {
                // Grow the file and extend the freelist bitmap.
                self.do_grow(GROW_PAGES)?;
                let mut fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
                fl.alloc().ok_or(DbError::StorageFull)?
            }
        };

        self.freelist_dirty.store(true, Ordering::Relaxed);

        // Initialize the new page. Acquire the per-page write lock to prevent
        // any concurrent reader from observing an uninitialized page.
        let _page_guard = self.page_locks.write(page_id);
        let new_page = Page::new(page_type, page_id);
        self.pwrite_page(page_id, &new_page)?;
        self.dirty.mark(page_id);
        Ok(page_id)
    }

    fn free_page(&self, page_id: u64) -> Result<(), DbError> {
        if page_id == 0 || page_id == 1 {
            return Err(DbError::Other(format!(
                "cannot free reserved page {page_id}"
            )));
        }
        let mut deferred = self
            .deferred_frees
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Check for double-free in the deferred queue.
        if deferred.iter().any(|(pid, _)| *pid == page_id) {
            return Err(DbError::Other(format!(
                "double free: page {page_id} already in deferred free queue"
            )));
        }
        // Check for double-free in the freelist.
        {
            let fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
            if fl.is_free(page_id) {
                return Err(DbError::Other(format!(
                    "double free: page {page_id} already free"
                )));
            }
        }
        deferred.push((page_id, self.current_snapshot_id.load(Ordering::Acquire)));
        if deferred.len() > DEFERRED_FREE_WARN_THRESHOLD {
            warn!(
                count = deferred.len(),
                "deferred free queue exceeds warning threshold — possible snapshot leak"
            );
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), DbError> {
        // Step 1: release deferred-free pages back to the freelist.
        // Under the current RwLock(Database) architecture, writers hold exclusive
        // access during flush, so no readers are active → u64::MAX releases all.
        self.release_deferred_frees(u64::MAX)?;

        let is_freelist_dirty = self.freelist_dirty.load(Ordering::Acquire);
        let has_changes = !self.dirty.is_empty() || is_freelist_dirty;

        if has_changes {
            let mut dw_pages: Vec<(u64, Vec<u8>)> = Vec::new();

            // Page 0 (meta) — may have been modified by grow().
            let meta_bytes = self.copy_raw_page_from_mmap(0)?;
            dw_pages.push((0, meta_bytes.to_vec()));

            // Page 1 (freelist) — build from in-memory state so the DW has the
            // freelist that WILL be written, not the stale on-disk version.
            let freelist_page = {
                let fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
                Self::build_freelist_page_from(&fl)
            };
            dw_pages.push((1, freelist_page.as_bytes().to_vec()));

            // Dirty data/index/overflow pages.
            for page_id in self.dirty.sorted_ids() {
                if page_id <= 1 {
                    continue; // already included above
                }
                let bytes = self.copy_raw_page_from_mmap(page_id)?;
                dw_pages.push((page_id, bytes.to_vec()));
            }

            // Write DW file and fsync — committed copy on disk.
            let dw_refs: Vec<(u64, &[u8])> =
                dw_pages.iter().map(|(id, b)| (*id, b.as_slice())).collect();
            let dw_guard = self.dw_buffer.lock().unwrap_or_else(|e| e.into_inner());
            dw_guard.write_and_sync(&dw_refs)?;
        }

        // Step 4: serialize the freelist via pwrite if modified.
        if is_freelist_dirty {
            self.pwrite_freelist()?;
        }

        // Step 5: fsync the main file — all pwrite() data becomes durable.
        self.file
            .sync_all()
            .map_err(|e| classify_io(e, "storage fsync"))?;

        // Step 6: remove the DW file (non-fatal on failure).
        if has_changes {
            let dw_guard = self.dw_buffer.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = dw_guard.cleanup() {
                warn!(error = %e, "failed to remove doublewrite file");
            }
        }

        // Step 7: clear tracking.
        self.freelist_dirty.store(false, Ordering::Release);
        self.dirty.clear();
        Ok(())
    }

    fn page_count(&self) -> u64 {
        self.page_count.load(Ordering::Acquire)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn prefetch_hint(&self, start_page_id: u64, count: u64) {
        let mmap = self.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap_len = mmap.len();
        let Some(offset) = (start_page_id as usize).checked_mul(PAGE_SIZE) else {
            return;
        };
        if offset >= mmap_len {
            return;
        }
        let effective_count = if count == 0 {
            PREFETCH_DEFAULT_PAGES
        } else {
            count
        };
        let requested_len = (effective_count as usize).saturating_mul(PAGE_SIZE);
        let clamped_len = requested_len.min(mmap_len - offset);
        // SAFETY: `ptr` is derived from a live `Mmap` guard (read lock held).
        // `offset < mmap_len` verified; `clamped_len <= mmap_len - offset`.
        // `madvise` is a pure hint — does not dereference Rust memory.
        let ptr = unsafe { mmap.as_ptr().add(offset) };
        let _ =
            unsafe { libc::madvise(ptr as *mut libc::c_void, clamped_len, libc::MADV_SEQUENTIAL) };
    }

    fn set_current_snapshot(&self, snapshot_id: u64) {
        self.current_snapshot_id
            .store(snapshot_id, Ordering::Release);
    }

    fn deferred_free_count(&self) -> usize {
        self.deferred_frees
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    fn page_lock_table(&self) -> &PageLockTable {
        &self.page_locks
    }

    // ── Batch allocation (Phase 40.9) ───────────────────────────────────

    fn alloc_page_batch(&self, n: usize, _page_type: PageType) -> Result<Vec<u64>, DbError> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(n);

        // Phase 1: drain recycle_queue (lock-free).
        while out.len() < n {
            match self.recycle_queue.pop() {
                Some(id) => out.push(id),
                None => break,
            }
        }
        if out.len() == n {
            self.freelist_dirty.store(true, Ordering::Relaxed);
            return Ok(out);
        }

        // Phase 2: acquire freelist mutex once and pull the rest.
        self.extension_waiters.fetch_add(1, Ordering::Relaxed);
        {
            let mut fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
            let remaining = n - out.len();
            out.extend(fl.alloc_batch_sequential(remaining));
        }
        self.extension_waiters.fetch_sub(1, Ordering::Relaxed);

        if out.len() == n {
            self.freelist_dirty.store(true, Ordering::Relaxed);
            return Ok(out);
        }

        // Phase 3: still short — grow the file and retry.
        {
            let _grow_guard = self.grow_lock.lock().unwrap_or_else(|e| e.into_inner());

            // Re-check under grow_lock: another thread may have grown in between.
            {
                let mut fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
                let still_needed = n - out.len();
                let extra = fl.alloc_batch_sequential(still_needed);
                out.extend(extra);
            }
            if out.len() < n {
                let still_needed = (n - out.len()) as u64;
                let grow_amount = still_needed.max(GROW_PAGES);
                self.do_grow(grow_amount)?;
                let mut fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
                out.extend(fl.alloc_batch_sequential(n - out.len()));
            }
        }

        if out.len() < n {
            return Err(DbError::StorageFull);
        }
        self.freelist_dirty.store(true, Ordering::Relaxed);
        Ok(out)
    }

    fn free_page_batch(&self, ids: &[u64]) -> Result<(), DbError> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut fl = self.freelist.lock().unwrap_or_else(|e| e.into_inner());
        fl.free_batch(ids)?;
        self.freelist_dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn extension_waiters(&self) -> u32 {
        self.extension_waiters.load(Ordering::Relaxed)
    }

    fn recycle_page(&self, page_id: u64) -> Result<(), DbError> {
        self.recycle_queue.push(page_id);
        Ok(())
    }
}

// ── Public MmapStorage helpers (non-trait) ────────────────────────────────────

impl MmapStorage {
    /// Returns the number of currently free pages.
    pub fn free_count(&self) -> u64 {
        self.freelist
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .free_count()
    }

    /// Returns the number of pages written since the last `flush()`.
    pub fn dirty_page_count(&self) -> usize {
        self.dirty.count()
    }

    /// Explicit grow by `extra_pages` (used in benchmarks).
    ///
    /// Acquires grow_lock and extends the file + freelist.
    pub fn grow(&self, extra_pages: u64) -> Result<u64, DbError> {
        let _grow_guard = self.grow_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.do_grow(extra_pages)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::run_storage_engine_suite;

    fn tmp_path() -> std::path::PathBuf {
        tempfile::NamedTempFile::new()
            .unwrap()
            .into_temp_path()
            .to_path_buf()
    }

    #[test]
    fn test_create_and_open() {
        let path = tmp_path();
        {
            let storage = MmapStorage::create(&path).unwrap();
            assert_eq!(storage.page_count(), GROW_PAGES);
        }
        let storage = MmapStorage::open(&path).unwrap();
        assert_eq!(storage.page_count(), GROW_PAGES);
    }

    #[test]
    fn test_file_lock_prevents_double_open() {
        let path = tmp_path();
        let _storage1 = MmapStorage::create(&path).unwrap();
        let result = MmapStorage::open(&path);
        assert!(
            matches!(result, Err(DbError::FileLocked { .. })),
            "expected FileLocked, got a different result"
        );
    }

    #[test]
    fn test_lock_released_after_drop() {
        let path = tmp_path();
        {
            let _storage = MmapStorage::create(&path).unwrap();
        }
        let storage = MmapStorage::open(&path).unwrap();
        assert_eq!(storage.page_count(), GROW_PAGES);
    }

    #[test]
    fn test_storage_engine_suite() {
        let path = tmp_path();
        let storage = MmapStorage::create(&path).unwrap();
        run_storage_engine_suite(&storage);
    }

    #[test]
    fn test_alloc_never_returns_reserved() {
        let path = tmp_path();
        let storage = MmapStorage::create(&path).unwrap();
        let ids: Vec<u64> = (0..10)
            .map(|_| storage.alloc_page(PageType::Data).unwrap())
            .collect();
        assert!(!ids.contains(&0));
        assert!(!ids.contains(&1));
    }

    #[test]
    fn test_alloc_free_reuse() {
        let path = tmp_path();
        let storage = MmapStorage::create(&path).unwrap();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.free_page(id).unwrap();
        storage.flush().unwrap();
        let id2 = storage.alloc_page(PageType::Data).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn test_freelist_persists_across_reopen() {
        let path = tmp_path();
        let allocated;
        {
            let storage = MmapStorage::create(&path).unwrap();
            allocated = storage.alloc_page(PageType::Data).unwrap();
            storage.flush().unwrap();
        }
        let storage = MmapStorage::open(&path).unwrap();
        let next = storage.alloc_page(PageType::Data).unwrap();
        assert_ne!(
            next, allocated,
            "freelist did not persist: reused an in-use page"
        );
    }

    #[test]
    fn test_grow_triggers_on_exhaustion() {
        let path = tmp_path();
        let storage = MmapStorage::create(&path).unwrap();
        let initial_count = storage.page_count();
        for _ in 0..(GROW_PAGES - 2) {
            storage.alloc_page(PageType::Data).unwrap();
        }
        storage.alloc_page(PageType::Data).unwrap();
        assert!(storage.page_count() > initial_count);
    }

    #[test]
    fn test_read_write_roundtrip() {
        let path = tmp_path();
        let storage = MmapStorage::create(&path).unwrap();
        let id = storage.alloc_page(PageType::Data).unwrap();

        let mut page = Page::new(PageType::Data, id);
        page.body_mut()[0] = 0xBE;
        page.body_mut()[1] = 0xEF;
        page.update_checksum();
        storage.write_page(id, &page).unwrap();
        let read = storage.read_page(id).unwrap();
        assert_eq!(read.body()[0], 0xBE);
        assert_eq!(read.body()[1], 0xEF);
    }

    #[test]
    fn test_concurrent_writes_different_pages() {
        // 4 threads × 25 writes to different pages — must all succeed.
        use std::sync::Arc;
        let path = tmp_path();
        let storage = Arc::new(MmapStorage::create(&path).unwrap());
        // Pre-allocate enough pages.
        let page_ids: Vec<u64> = (0..100)
            .map(|_| storage.alloc_page(PageType::Data).unwrap())
            .collect();
        let page_ids = Arc::new(page_ids);
        let mut handles = Vec::new();
        for t in 0..4usize {
            let storage = Arc::clone(&storage);
            let page_ids = Arc::clone(&page_ids);
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    let pid = page_ids[t * 25 + i];
                    let mut p = Page::new(PageType::Data, pid);
                    p.body_mut()[0] = (t * 25 + i) as u8;
                    p.update_checksum();
                    storage.write_page(pid, &p).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_concurrent_alloc_no_duplicates() {
        // 4 threads allocating pages concurrently must not get the same page_id.
        use std::sync::{Arc, Mutex};
        let path = tmp_path();
        let storage = Arc::new(MmapStorage::create(&path).unwrap());
        let all_ids: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let storage = Arc::clone(&storage);
            let all_ids = Arc::clone(&all_ids);
            handles.push(std::thread::spawn(move || {
                let mut local = Vec::new();
                for _ in 0..10 {
                    local.push(storage.alloc_page(PageType::Data).unwrap());
                }
                all_ids.lock().unwrap().extend(local);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let ids = all_ids.lock().unwrap();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate page IDs allocated");
    }
}
