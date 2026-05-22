use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
};

use axiomdb_core::error::{classify_io, DbError};
use fs2::FileExt;
use libc;
use memmap2::Mmap;
use tracing::{debug, info, warn};

use crate::{
    buffer_pool::BufferPool,
    dirty::PageDirtyTracker,
    doublewrite::DoublewriteBuffer,
    engine::StorageEngine,
    freelist::FreeList,
    page::{Page, PageType, HEADER_SIZE, PAGE_SIZE},
    page_lock::PageLockTable,
    wal_frame::{FrameLog, FrameRef, WalIndex},
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
// Offset of the `lsn` field inside PageHeader: magic(8)+page_type(1)+flags(1)
// +item_count(2)+checksum(4)+page_id(8) = 24. The checksum (offset 12) covers
// only the body [HEADER_SIZE..], so stamping the lsn here never invalidates it.
const LSN_OFFSET: usize = 24;

/// Page 1 holds the free-list bitmap (page 0 is the meta page).
const FREELIST_PAGE_ID: u64 = 1;

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
    /// Excludes frame appends during a checkpoint's log recycle (subphase 6b). Frame
    /// appends take the read side; `checkpoint_frames` takes the write side so the
    /// recycle (truncate + fresh salt) never races an in-flight append.
    checkpoint_lock: RwLock<()>,
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
    /// User-space clock-sweep page cache. Served on `read_page` hits (no mmap
    /// lock / copy / CRC); invalidated on `write_page` and page reuse.
    buffer_pool: BufferPool,
    /// Page-image redo log (project B, subphase 3). `None` ⇒ write-ahead disabled
    /// ⇒ `read_page`/`write_page` behave exactly as before. Lock-free append, so
    /// it is NOT wrapped in a `Mutex`.
    frame_log: Option<FrameLog>,
    /// Live page → latest-appended-frame index (16-way sharded). Empty until a
    /// frame is appended; consulted by `read_page` only on a buffer-pool miss when
    /// the log is enabled.
    wal_index: WalIndex,
    /// Monotonic LSN stamped into each written page's header (`PageHeader.lsn`).
    /// `0` while redo is disabled; set past the log's highest LSN on enable.
    frame_lsn: AtomicU64,
    /// Frame-only mode (project B subphase 6c): when set, `write_page` does NOT write the
    /// main file (the page lives in the frame log until a checkpoint applies it). `false`
    /// = dual-write (the main file stays authoritative). Set by `enable_frame_only_redo`.
    frame_only: AtomicBool,
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
            checkpoint_lock: RwLock::new(()),
            page_count: AtomicU64::new(GROW_PAGES),
            recycle_queue: crossbeam_queue::SegQueue::new(),
            extension_waiters: AtomicU32::new(0),
            clean_shutdown_on_open: true,
            buffer_pool: BufferPool::new(),
            frame_log: None,
            wal_index: WalIndex::default(),
            frame_lsn: AtomicU64::new(0),
            frame_only: AtomicBool::new(false),
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
            checkpoint_lock: RwLock::new(()),
            page_count: AtomicU64::new(db_page_count),
            recycle_queue: crossbeam_queue::SegQueue::new(),
            extension_waiters: AtomicU32::new(0),
            clean_shutdown_on_open,
            buffer_pool: BufferPool::new(),
            frame_log: None,
            wal_index: WalIndex::default(),
            frame_lsn: AtomicU64::new(0),
            frame_only: AtomicBool::new(false),
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
        // Diagnostic-only timing (zero-cost when not armed: one Relaxed load).
        if crate::io_stats::armed() {
            let t0 = std::time::Instant::now();
            let r = self
                .file
                .write_all_at(data, offset)
                .map_err(|e| classify_io(e, "pwrite"));
            crate::io_stats::record_pwrite(t0.elapsed().as_nanos() as u64, data.len() as u64);
            return r;
        }
        self.file
            .write_all_at(data, offset)
            .map_err(|e| classify_io(e, "pwrite"))
    }

    /// Writes a full page via `pwrite()` at the correct file offset.
    fn pwrite_page(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        let offset = page_id * PAGE_SIZE as u64;
        self.pwrite_bytes(offset, page.as_bytes())
    }

    /// Applies every committed frame to the main file — shared by recovery REDO and the
    /// checkpoint (subphase 6b). For each page's latest committed frame, if
    /// `frame.lsn > page.lsn` the frame's full image is written directly. Grows the file
    /// for a committed frame whose page is beyond EOF (allocated after the last main-file
    /// flush, then lost), and reloads the in-memory freelist if the bitmap page (page 1)
    /// was redone. Returns the number of pages written.
    fn apply_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
        let Some(frame_log) = &self.frame_log else {
            return Ok(0);
        };
        let index = frame_log.build_index(is_committed)?;
        let mut applied = 0usize;
        let mut touched_bitmap = false;
        for frame in index.frames() {
            // Grow-on-redo: extend the main file to cover a committed frame whose page is
            // past EOF before writing it.
            if frame.page_id >= self.page_count.load(Ordering::Acquire) {
                let _grow = self.grow_lock.lock().unwrap_or_else(|e| e.into_inner());
                let cur = self.page_count.load(Ordering::Acquire);
                if frame.page_id >= cur {
                    self.do_grow(frame.page_id + 1 - cur)?;
                }
            }
            let page_lsn = match self.read_page_raw(frame.page_id) {
                Ok(bytes) => u64::from_le_bytes(
                    bytes[LSN_OFFSET..LSN_OFFSET + 8]
                        .try_into()
                        .expect("8 bytes"),
                ),
                Err(DbError::PageNotFound { .. }) => 0,
                Err(e) => return Err(e),
            };
            // Idempotence guard (Postgres pageLSN): apply only a strictly newer frame.
            if frame.lsn > page_lsn {
                let page_bytes = frame_log.read_page_at(frame.offset)?;
                self.pwrite_bytes(frame.page_id * PAGE_SIZE as u64, &page_bytes[..])?;
                self.buffer_pool.invalidate(frame.page_id);
                if frame.page_id == FREELIST_PAGE_ID {
                    touched_bitmap = true;
                }
                applied += 1;
            }
        }
        // A redone freelist bitmap (page 1) makes the in-memory freelist stale.
        if touched_bitmap {
            self.reload_freelist_from_disk()?;
        }
        Ok(applied)
    }

    /// Reloads the in-memory freelist from the (possibly redone) bitmap page (page 1),
    /// so it matches the on-disk bitmap after a REDO/checkpoint applied a bitmap frame.
    fn reload_freelist_from_disk(&self) -> Result<(), DbError> {
        let page_count = self.page_count.load(Ordering::Acquire);
        let bitmap = self.read_page_raw(FREELIST_PAGE_ID)?;
        let page = Page::from_bytes(bitmap)?;
        let fl = FreeList::from_bytes(page.body(), page_count);
        *self.freelist.lock().unwrap_or_else(|e| e.into_inner()) = fl;
        Ok(())
    }

    /// Test-only: shrink the main file (and `page_count`) to `page_count` pages, then
    /// remap — models a page allocated after the last flush that a crash lost, so the
    /// grow-on-redo path can be tested deterministically.
    #[cfg(test)]
    pub(crate) fn truncate_file_for_test(&self, page_count: u64) {
        self.file
            .set_len(page_count * PAGE_SIZE as u64)
            .expect("truncate");
        self.page_count.store(page_count, Ordering::Release);
        // Keep the freelist consistent with the shrunk page_count (else do_grow's
        // `grow(new_total > total)` assert trips when REDO grows the file back).
        *self.freelist.lock().unwrap_or_else(|e| e.into_inner()) =
            FreeList::new(page_count, &[0, 1]);
        let mut g = self.mmap.write().unwrap_or_else(|e| e.into_inner());
        // SAFETY: file just resized; no other writer (test-only, single-threaded).
        *g = unsafe { Mmap::map(&self.file).expect("remap") };
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
        if let Some(frame_log) = &self.frame_log {
            // Write-ahead (subphase 3 = dual-write): stamp the page LSN, write the
            // main file (still authoritative until subphase 4), append a page frame,
            // record it in the live index, and cache the stamped bytes so a
            // read-after-write is a pool hit (no frame pread).
            // Hold the checkpoint read-guard across the append so a checkpoint's log
            // recycle (write-guard) never races it (subphase 6b).
            let _ck = self
                .checkpoint_lock
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let lsn = self.frame_lsn.fetch_add(1, Ordering::Relaxed);
            let mut buf = [0u8; PAGE_SIZE];
            buf.copy_from_slice(page.as_bytes());
            buf[LSN_OFFSET..LSN_OFFSET + 8].copy_from_slice(&lsn.to_le_bytes());
            // Frame-only (subphase 6c): skip the main-file write — the page lives in the
            // frame log until a checkpoint applies it. Dual-write mode still writes main.
            if !self.frame_only.load(Ordering::Relaxed) {
                self.pwrite_bytes(page_id * PAGE_SIZE as u64, &buf)?;
            }
            // Stamp the writing txn (thread-local, set per statement by the executor).
            // 0 = non-transactional/system write (e.g. bootstrap); recovery treats
            // such frames per the committed predicate in subphase 5.
            let txn_id = crate::txn_stamp::get();
            let offset = frame_log.append(page_id, lsn, txn_id, &buf)?;
            self.wal_index.record(FrameRef {
                page_id,
                lsn,
                txn_id,
                offset,
            });
            // The page was just produced from a valid page; only the LSN (offset 24,
            // in the header, outside the checksum body) was stamped, so the body
            // checksum is still valid — skip the redundant re-verification (6d).
            let stamped = Page::from_bytes_unchecked(buf);
            self.buffer_pool.insert(page_id, Arc::new(stamped));
        } else {
            self.pwrite_page(page_id, page)?;
            // Drop any stale cached copy so the next read reloads the new bytes.
            // Public `write_page` holds the page X-latch across this, serializing
            // with S-latch readers' miss-path inserts.
            self.buffer_pool.invalidate(page_id);
        }
        self.dirty.mark(page_id);
        Ok(())
    }

    fn read_page_from_mmap(mmap: &Mmap, page_id: u64) -> Result<crate::page_ref::PageRef, DbError> {
        Ok(crate::page_ref::PageRef::from_arc(
            Self::read_arc_from_mmap(mmap, page_id)?,
        ))
    }

    /// Reads a page from the mmap straight into an `Arc<Page>` — one heap
    /// allocation, one 16 KB copy, then a single CRC32c verify. This is the hot
    /// read path: it avoids the `PageRef → into_page() → Arc::new()` round-trip
    /// (which would cost a second allocation and two extra 16 KB moves).
    fn read_arc_from_mmap(mmap: &Mmap, page_id: u64) -> Result<Arc<Page>, DbError> {
        let offset = page_id as usize * PAGE_SIZE;
        if offset + PAGE_SIZE > mmap.len() {
            return Err(DbError::PageNotFound { page_id });
        }
        let mut boxed = Box::<Page>::new_uninit();
        // SAFETY: `Page` is `repr(C, align(64))` and exactly `PAGE_SIZE` bytes;
        // `[offset, offset + PAGE_SIZE)` is in bounds (checked above), and the
        // box has `PAGE_SIZE` bytes of capacity. After the copy every byte is
        // initialized, so `assume_init` is sound.
        let arc: Arc<Page> = unsafe {
            std::ptr::copy_nonoverlapping(
                mmap.as_ptr().add(offset),
                boxed.as_mut_ptr() as *mut u8,
                PAGE_SIZE,
            );
            Arc::from(boxed.assume_init())
        };
        arc.verify_checksum()?;
        Ok(arc)
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

    /// (hits, misses) of the page buffer pool — for diagnostics and tests.
    pub fn buffer_pool_stats(&self) -> (u64, u64) {
        self.buffer_pool.stats()
    }

    /// Drops cached entries for a batch of (re)allocated page ids so a realloc
    /// never serves stale bytes before the caller writes the page.
    fn invalidate_cached(&self, ids: &[u64]) {
        for &id in ids {
            self.buffer_pool.invalidate(id);
        }
    }
}

// ── StorageEngine impl ────────────────────────────────────────────────────────

impl StorageEngine for MmapStorage {
    fn read_page(&self, page_id: u64) -> Result<crate::page_ref::PageRef, DbError> {
        // Hot path: page_count is an AtomicU64 — no lock needed.
        if page_id >= self.page_count.load(Ordering::Acquire) {
            return Err(DbError::PageNotFound { page_id });
        }
        // Buffer-pool hit: share the cached page — no mmap lock, no 16 KB copy,
        // no CRC re-check (the page was verified when it was first loaded).
        if let Some(page) = self.buffer_pool.get(page_id) {
            return Ok(crate::page_ref::PageRef::from_arc(page));
        }
        // Write-ahead miss path (project B, SQLite walFindFrame ordering): if the
        // log is enabled and this page has a frame, its latest bytes live in the
        // frame log — read them there, verify CRC, and cache. An empty index or a
        // disabled log short-circuits to the mmap path below (= today's behavior),
        // so warm reads never pay the index lookup.
        if let Some(frame_log) = &self.frame_log {
            if let Some(frame) = self.wal_index.latest(page_id) {
                // Verify the frame is still for this page under the current salt: a
                // concurrent checkpoint may have recycled the log, leaving a stale
                // wal-index offset. On a mismatch the checkpoint already applied this page
                // to the main file (apply precedes recycle), so fall through to mmap.
                if let Some(bytes) = frame_log.read_page_if_for(frame.offset, page_id)? {
                    let arc = Arc::new(Page::from_bytes(*bytes)?);
                    let arc = self.buffer_pool.insert(page_id, arc);
                    return Ok(crate::page_ref::PageRef::from_arc(arc));
                }
            }
        }
        // Cold miss: take the mmap read lock, read straight into an Arc<Page>
        // (one alloc, one copy), verify its CRC32c once, then cache it.
        let mmap = self.mmap.read().unwrap_or_else(|e| e.into_inner());
        let arc = Self::read_arc_from_mmap(&mmap, page_id)?;
        drop(mmap);
        let arc = self.buffer_pool.insert(page_id, arc);
        Ok(crate::page_ref::PageRef::from_arc(arc))
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
        // Reused id: drop any stale cached entry from before this page was freed.
        self.buffer_pool.invalidate(page_id);
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

    fn set_current_txn(&self, txn_id: u64) {
        crate::txn_stamp::set(txn_id);
    }

    fn current_txn(&self) -> u64 {
        crate::txn_stamp::get()
    }

    fn sync_frame_log(&self) -> Result<(), DbError> {
        if let Some(fl) = &self.frame_log {
            // Gap-free durability: wait for the contiguous written prefix to cover every
            // frame reserved before this commit, then fsync (subphase 6a).
            fl.sync_to_durable()?;
        }
        Ok(())
    }

    fn frame_log_active(&self) -> bool {
        self.frame_log.is_some()
    }

    fn frame_log_durable_len(&self) -> u64 {
        // Gap-free written prefix (bytes); resets to FILE_HDR_SIZE on recycle.
        self.frame_log
            .as_ref()
            .map_or(0, |fl| fl.contiguous_written_offset())
    }

    fn redo_committed_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
        // Recovery REDO shares the apply with the checkpoint (grow-on-redo + freelist
        // reload). On open, this materializes committed frames into the main file.
        self.apply_committed_frames(is_committed)
    }

    fn checkpoint_frames(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<usize, DbError> {
        let Some(frame_log) = &self.frame_log else {
            return Ok(0);
        };
        // Exclusive guard (model A): drains in-flight frame appends and blocks new ones
        // until the recycle completes, so the truncate never races an append.
        let _ckpt = self
            .checkpoint_lock
            .write()
            .unwrap_or_else(|e| e.into_inner());
        // `txn_id == 0` is a non-transactional/system write — always durable, never an
        // in-flight user txn — so it counts as committed for both apply and recycle.
        let committed = |t: u64| t == 0 || is_committed(t);
        // 6d: make the frame log durable BEFORE mutating the main file (SQLite walCheckpoint
        // order: fsync WAL → copy to db). Under NORMAL the per-commit frame fsync is deferred
        // to here, so this is where the committed frames actually become durable. If a crash
        // hits mid-apply, recovery REDOes from these (now-durable) frames — without this fsync
        // a power loss could lose un-synced frames that a partially-applied main still needs.
        frame_log.sync_to_durable()?;
        // Ordering invariant: fsync frame log → apply committed frames → fsync main → recycle.
        let applied = self.apply_committed_frames(&committed)?;
        self.flush()?; // applied pages are durable in the main file before the recycle
                       // In-flight-safe: only recycle when EVERY frame is committed (else an uncommitted
                       // txn's frames live only in the log and must survive once writes are frame-only).
                       // On a full recycle, clear the live wal_index — its offsets are now invalid and
                       // the pages are in the (just-fsync'd) main file, so reads fall through to it.
        if frame_log.scan()?.iter().all(|f| committed(f.txn_id)) {
            frame_log.recycle()?;
            self.wal_index.clear();
        }
        Ok(applied)
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
            self.invalidate_cached(&out);
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
            self.invalidate_cached(&out);
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
        self.invalidate_cached(&out);
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
    /// Enables the page-image redo log (project B, subphase 3). Opens (or creates)
    /// the frame log next to the db file (`<db>.wf`), advances the LSN counter past
    /// the highest LSN already logged, and routes future writes through it.
    ///
    /// Opt-in in subphase 3 (default OFF ⇒ behavior is byte-for-byte unchanged).
    /// Call right after `create`/`open`, before the storage is shared.
    pub fn enable_redo_log(&mut self, db_path: &Path) -> Result<(), DbError> {
        let wf_path = db_path.with_extension("wf");
        let log = if wf_path.exists() {
            FrameLog::open(&wf_path)?
        } else {
            FrameLog::create(&wf_path)?
        };
        let max_lsn = log.scan()?.iter().map(|f| f.lsn).max().unwrap_or(0);
        self.frame_lsn.store(max_lsn + 1, Ordering::Relaxed);
        self.frame_log = Some(log);
        Ok(())
    }

    /// Enables the redo log in **frame-only** mode (project B subphase 6c): `write_page`
    /// stops writing the main file (a commit is durable via the frame fsync; the main file
    /// is updated only by a checkpoint). The production durability switch — gated by the
    /// `DbConfig` redo mode.
    pub fn enable_frame_only_redo(&mut self, db_path: &Path) -> Result<(), DbError> {
        self.enable_redo_log(db_path)?;
        self.frame_only.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Whether the page-image redo log is active.
    pub fn redo_enabled(&self) -> bool {
        self.frame_log.is_some()
    }

    /// (diag/test) number of pages currently in the live wal-index.
    pub fn wal_index_len(&self) -> usize {
        self.wal_index.len()
    }

    /// (diag/test) current value of the frame LSN counter (next LSN to assign).
    pub fn current_frame_lsn(&self) -> u64 {
        self.frame_lsn.load(Ordering::Acquire)
    }

    /// (diag/test) whether the live wal-index has a frame for `page_id`.
    pub fn frame_index_contains(&self, page_id: u64) -> bool {
        self.wal_index.latest(page_id).is_some()
    }

    /// (diag/test) txn_id stamped on the live frame for `page_id`, if any.
    pub fn frame_txn_id(&self, page_id: u64) -> Option<u64> {
        self.wal_index.latest(page_id).map(|f| f.txn_id)
    }

    /// (diag/test) drop a page's cached copy, forcing the next read down the
    /// miss path (frame log if enabled, else mmap).
    pub fn evict_from_pool(&self, page_id: u64) {
        self.buffer_pool.invalidate(page_id);
    }

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
    fn buffer_pool_second_read_is_hit() {
        let s = MmapStorage::create(&tmp_path()).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        let _ = s.read_page(pid).unwrap(); // cold miss → populates the pool
        let (h0, _) = s.buffer_pool_stats();
        let _ = s.read_page(pid).unwrap(); // warm hit
        let (h1, _) = s.buffer_pool_stats();
        assert_eq!(h1, h0 + 1, "second read of the same page must hit the pool");
    }

    #[test]
    fn buffer_pool_write_then_read_sees_fresh_bytes() {
        let s = MmapStorage::create(&tmp_path()).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        let _ = s.read_page(pid).unwrap(); // cache the freshly-alloc'd contents
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0xAB;
        p.update_checksum();
        s.write_page(pid, &p).unwrap(); // must invalidate the cached copy
        let got = s.read_page(pid).unwrap();
        assert_eq!(
            got.body()[0],
            0xAB,
            "read after write must see the new bytes"
        );
    }

    #[test]
    fn buffer_pool_free_then_realloc_does_not_serve_stale() {
        let s = MmapStorage::create(&tmp_path()).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0xCC; // distinctive marker
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        let _ = s.read_page(pid).unwrap(); // cache the marked page
        s.free_page(pid).unwrap();
        s.release_deferred_frees(u64::MAX).unwrap();
        let pid2 = s.alloc_page(PageType::Data).unwrap(); // fresh page (body zeroed)
        let got = s.read_page(pid2).unwrap();
        if pid2 == pid {
            assert_eq!(got.body()[0], 0x00, "must not serve the stale cached page");
        }
        assert_eq!(got.header().page_id, pid2);
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

    #[test]
    fn enable_redo_log_is_additive() {
        let path = tmp_path();
        let mut s = MmapStorage::create(&path).unwrap();
        assert!(!s.redo_enabled(), "redo off by default");
        s.enable_redo_log(&path).unwrap();
        assert!(s.redo_enabled());
        assert_eq!(
            s.wal_index_len(),
            0,
            "index empty until a frame is appended"
        );
        assert_eq!(s.current_frame_lsn(), 1, "fresh log ⇒ first LSN is 1");
        // Empty index ⇒ reads fall through to mmap exactly as before.
        let pid = s.alloc_page(PageType::Data).unwrap();
        let got = s.read_page(pid).unwrap();
        assert_eq!(got.header().page_id, pid);
    }

    #[test]
    fn write_appends_frame_and_records_index_when_enabled() {
        let path = tmp_path();
        let mut s = MmapStorage::create(&path).unwrap();
        s.enable_redo_log(&path).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0x7C;
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        assert!(
            s.frame_index_contains(pid),
            "page recorded in the live index"
        );
        assert!(s.current_frame_lsn() >= 2, "lsn advanced past the write");
        assert_eq!(s.read_page(pid).unwrap().body()[0], 0x7C);
    }

    #[test]
    fn lsn_stamp_does_not_break_page_checksum() {
        let path = tmp_path();
        let mut s = MmapStorage::create(&path).unwrap();
        s.enable_redo_log(&path).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0x5A;
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        // Read raw from the MAIN file and parse: the stamped lsn must be non-zero
        // and the body checksum must still verify (lsn lives outside the body).
        let raw = s.read_page_raw(pid).unwrap();
        let parsed = Page::from_bytes(raw).expect("checksum valid after lsn stamp");
        assert_ne!(parsed.header().lsn, 0, "PageHeader.lsn was stamped");
    }

    #[test]
    fn write_disabled_is_byte_identical() {
        let path = tmp_path();
        let s = MmapStorage::create(&path).unwrap(); // redo OFF
        let pid = s.alloc_page(PageType::Data).unwrap();
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0x33;
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        assert_eq!(s.wal_index_len(), 0, "no frames recorded when disabled");
        assert_eq!(s.read_page(pid).unwrap().body()[0], 0x33);
        let raw = s.read_page_raw(pid).unwrap();
        assert_eq!(
            Page::from_bytes(raw).unwrap().header().lsn,
            0,
            "lsn stays 0 when redo is off"
        );
    }

    #[test]
    fn read_after_write_served_from_frame_not_main() {
        use std::os::unix::fs::FileExt;
        let path = tmp_path();
        let mut s = MmapStorage::create(&path).unwrap();
        s.enable_redo_log(&path).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0xD7;
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        s.evict_from_pool(pid); // force the miss path

        // Corrupt the MAIN file's copy of this page: if the read still returns the
        // correct bytes they came from the frame log (the mmap copy is now garbage
        // and would fail its checksum).
        {
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.write_all_at(&[0xFFu8; 64], pid * PAGE_SIZE as u64)
                .unwrap();
        }
        let got = s.read_page(pid).unwrap();
        assert_eq!(
            got.body()[0],
            0xD7,
            "served from the frame log, not the corrupt main file"
        );
    }

    #[test]
    fn pool_hit_returns_without_index() {
        let path = tmp_path();
        let mut s = MmapStorage::create(&path).unwrap();
        s.enable_redo_log(&path).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0x42;
        p.update_checksum();
        s.write_page(pid, &p).unwrap(); // inserts the stamped page into the pool
        let (hits_before, _) = s.buffer_pool_stats();
        let got = s.read_page(pid).unwrap(); // pool hit — no index lookup
        assert_eq!(got.body()[0], 0x42);
        let (hits_after, _) = s.buffer_pool_stats();
        assert_eq!(hits_after, hits_before + 1, "served from the pool (hit)");
    }

    #[test]
    fn enabled_unwritten_page_reads_from_mmap() {
        let path = tmp_path();
        let mut s = MmapStorage::create(&path).unwrap();
        s.enable_redo_log(&path).unwrap();
        // alloc_page pwrites a zeroed page but appends no frame in subphase 3.
        let pid = s.alloc_page(PageType::Data).unwrap();
        assert!(!s.frame_index_contains(pid), "alloc appends no frame");
        let got = s.read_page(pid).unwrap(); // index miss → mmap path
        assert_eq!(got.header().page_id, pid);
    }

    #[test]
    fn write_stamps_current_txn_into_frame() {
        let path = tmp_path();
        let mut s = MmapStorage::create(&path).unwrap();
        s.enable_redo_log(&path).unwrap();
        s.set_current_txn(42);
        let pid = s.alloc_page(PageType::Data).unwrap();
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0x09;
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        assert_eq!(
            s.frame_txn_id(pid),
            Some(42),
            "frame stamped with current txn"
        );
        s.sync_frame_log().unwrap(); // fsync the frame log (commit-boundary primitive)
        s.set_current_txn(0); // reset for this thread
    }

    #[test]
    fn redo_restores_a_page_whose_main_file_write_was_lost() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("redo.db");
        let row_byte = 0xC7u8;

        // Session A (redo ON): write ROW under committed txn 5 → frame ROW@lsnN durable.
        // flush() persists page_count/freelist into the meta page so B/C see the page.
        let page_id = {
            let mut s = MmapStorage::create(&db).unwrap();
            s.enable_redo_log(&db).unwrap();
            let page_id = s.alloc_page(PageType::Data).unwrap();
            s.set_current_txn(5);
            let mut p = Page::new(PageType::Data, page_id);
            p.body_mut()[0] = row_byte;
            p.update_checksum();
            s.write_page(page_id, &p).unwrap();
            s.sync_frame_log().unwrap(); // frame durable in <db>.wf
            s.set_current_txn(0);
            s.flush().unwrap();
            page_id
        };

        // Session B (redo OFF): clobber the main-file page with an empty lsn-0 image,
        // modelling the lost (un-fsync'd) main-file write. The .wf is untouched.
        {
            let s = MmapStorage::open(&db).unwrap();
            s.write_page(page_id, &Page::new(PageType::Data, page_id))
                .unwrap();
            s.flush().unwrap();
            assert_eq!(
                s.read_page(page_id).unwrap().body()[0],
                0,
                "precondition: the main file lost the row"
            );
        }

        // Session C (redo ON): REDO restores the row from the frame, idempotently.
        {
            let mut s = MmapStorage::open(&db).unwrap();
            s.enable_redo_log(&db).unwrap();
            assert_eq!(
                s.redo_committed_frames(&|t| t == 5).unwrap(),
                1,
                "one committed page redone"
            );
            assert_eq!(
                s.read_page(page_id).unwrap().body()[0],
                row_byte,
                "committed row restored by REDO"
            );
            assert_eq!(
                s.redo_committed_frames(&|t| t == 5).unwrap(),
                0,
                "idempotent re-run is a no-op (pageLSN guard)"
            );
        }
    }

    #[test]
    fn redo_grows_the_file_for_a_committed_frame_beyond_eof() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("grow.db");
        let row = 0xE9u8;

        let mut s = MmapStorage::create(&db).unwrap();
        s.enable_redo_log(&db).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        s.set_current_txn(5);
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = row;
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        s.sync_frame_log().unwrap();
        s.set_current_txn(0);

        // Model a lost post-flush allocation: shrink the main file below `pid`.
        s.truncate_file_for_test(pid);
        assert!(
            matches!(s.read_page(pid), Err(DbError::PageNotFound { .. })),
            "precondition: the allocated page is beyond EOF after the shrink"
        );

        // REDO must grow the file back and restore the committed page.
        assert_eq!(
            s.redo_committed_frames(&|t| t == 5).unwrap(),
            1,
            "one page redone"
        );
        assert_eq!(
            s.read_page(pid).unwrap().body()[0],
            row,
            "grow-on-redo restored the committed page beyond the old EOF"
        );
    }

    #[test]
    fn checkpoint_restores_stale_main_then_recycles_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ckpt.db");
        let row = 0x5Cu8;

        // Session A (redo on): write ROW under committed txn 5 → frame durable; flush meta.
        let page_id = {
            let mut s = MmapStorage::create(&db).unwrap();
            s.enable_redo_log(&db).unwrap();
            let pid = s.alloc_page(PageType::Data).unwrap();
            s.set_current_txn(5);
            let mut p = Page::new(PageType::Data, pid);
            p.body_mut()[0] = row;
            p.update_checksum();
            s.write_page(pid, &p).unwrap();
            s.sync_frame_log().unwrap();
            s.set_current_txn(0);
            s.flush().unwrap();
            pid
        };

        // Session B (redo off): clobber the main page with an empty lsn-0 image.
        {
            let s = MmapStorage::open(&db).unwrap();
            s.write_page(page_id, &Page::new(PageType::Data, page_id))
                .unwrap();
            s.flush().unwrap();
            assert_eq!(
                s.read_page(page_id).unwrap().body()[0],
                0,
                "main lost the row"
            );
        }

        // Session C (redo on): checkpoint applies the committed frame, then recycles.
        {
            let mut s = MmapStorage::open(&db).unwrap();
            s.enable_redo_log(&db).unwrap();
            assert_eq!(
                s.checkpoint_frames(&|t| t == 5).unwrap(),
                1,
                "one committed page checkpointed to the main file"
            );
            assert_eq!(
                s.read_page(page_id).unwrap().body()[0],
                row,
                "page restored from its frame"
            );
            // The log was recycled (fresh salt, empty): a second checkpoint applies nothing.
            assert_eq!(
                s.checkpoint_frames(&|t| t == 5).unwrap(),
                0,
                "log recycled — nothing left to apply"
            );
        }
    }

    #[test]
    fn read_falls_back_to_main_when_frame_was_recycled() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fb.db");
        let mut s = MmapStorage::create(&db).unwrap();
        s.enable_redo_log(&db).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        s.set_current_txn(5);
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0x9A;
        p.update_checksum();
        s.write_page(pid, &p).unwrap(); // dual-write: main + frame + wal_index
        s.sync_frame_log().unwrap();
        s.set_current_txn(0);
        // A checkpoint recycles the frame log (all committed); a still-live wal-index
        // offset is now stale. The next read must verify it, fail, and fall back to the
        // main file (which the checkpoint made current).
        s.checkpoint_frames(&|t| t == 5).unwrap();
        s.evict_from_pool(pid); // force the miss path
        assert_eq!(
            s.read_page(pid).unwrap().body()[0],
            0x9A,
            "read falls back to the main file when the frame was recycled"
        );
    }

    #[test]
    fn maybe_checkpoint_respects_threshold_and_force() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("mc.db");
        let mut s = MmapStorage::create(&db).unwrap();

        // Redo off: durable_len is 0 and maybe_checkpoint is always a no-op.
        assert_eq!(s.frame_log_durable_len(), 0);
        assert_eq!(s.maybe_checkpoint_frames(&|_| true, 0, true).unwrap(), 0);

        s.enable_redo_log(&db).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        s.set_current_txn(5);
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0x7;
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        s.sync_frame_log().unwrap();
        s.set_current_txn(0);

        let len = s.frame_log_durable_len();
        assert!(len > 0, "a written + synced frame advances durable_len");

        // Under a huge threshold, no force → skip (log untouched).
        assert_eq!(
            s.maybe_checkpoint_frames(&|t| t == 5, u64::MAX, false).unwrap(),
            0
        );
        assert_eq!(s.frame_log_durable_len(), len, "log untouched under threshold");

        // Over threshold (1 byte ≪ len) → checkpoint runs + recycles. In dual-write
        // the apply COUNT is 0 (main is already current via the equal pageLSN), but
        // the log is recycled (all frames committed) → durable_len drops. The
        // recycle (bounding the log) is the trigger's actual job, not the count.
        s.maybe_checkpoint_frames(&|t| t == 5, 1, false).unwrap();
        let after = s.frame_log_durable_len();
        assert!(after < len, "over threshold → log recycled ({after} < {len})");

        // force=true checkpoints/recycles even under a huge threshold (the
        // clean-shutdown path). Write a fresh frame, then force.
        s.set_current_txn(6);
        let mut p2 = Page::new(PageType::Data, pid);
        p2.body_mut()[0] = 0x8;
        p2.update_checksum();
        s.write_page(pid, &p2).unwrap();
        s.sync_frame_log().unwrap();
        s.set_current_txn(0);
        let grew = s.frame_log_durable_len();
        assert!(grew > after, "a new frame grows the log again");
        s.maybe_checkpoint_frames(&|t| t == 6, u64::MAX, true).unwrap();
        assert!(
            s.frame_log_durable_len() < grew,
            "force → log recycled even under threshold"
        );
    }

    #[test]
    fn checkpoint_with_inflight_frame_does_not_recycle() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("inflight.db");
        let mut s = MmapStorage::create(&db).unwrap();
        s.enable_redo_log(&db).unwrap();
        let p_committed = s.alloc_page(PageType::Data).unwrap();
        let p_inflight = s.alloc_page(PageType::Data).unwrap();

        s.set_current_txn(5);
        let mut a = Page::new(PageType::Data, p_committed);
        a.body_mut()[0] = 0x11;
        a.update_checksum();
        s.write_page(p_committed, &a).unwrap();

        s.set_current_txn(9); // in-flight: never committed
        let mut b = Page::new(PageType::Data, p_inflight);
        b.body_mut()[0] = 0x22;
        b.update_checksum();
        s.write_page(p_inflight, &b).unwrap();
        s.set_current_txn(0);

        // Only txn 5 committed → the txn-9 frame is in-flight → NO recycle.
        s.checkpoint_frames(&|t| t == 5).unwrap();
        assert!(
            s.frame_index_contains(p_inflight),
            "an in-flight frame must survive the checkpoint (no recycle)"
        );

        // Now both committed → full recycle + the live wal_index is cleared.
        s.checkpoint_frames(&|t| t == 5 || t == 9).unwrap();
        assert!(
            !s.frame_index_contains(p_inflight),
            "all committed → recycled + wal_index cleared"
        );
    }

    #[test]
    fn frame_only_write_skips_the_main_file() {
        use crate::page::HEADER_SIZE;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fo.db");
        let mut s = MmapStorage::create(&db).unwrap();
        s.enable_frame_only_redo(&db).unwrap();
        let pid = s.alloc_page(PageType::Data).unwrap();
        s.set_current_txn(5);
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = 0x77;
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        s.sync_frame_log().unwrap();
        s.set_current_txn(0);
        // Frame-only: the main file was NOT written — a raw read shows the zeroed page.
        assert_eq!(
            s.read_page_raw(pid).unwrap()[HEADER_SIZE],
            0,
            "a frame-only write must not touch the main file"
        );
        // But read_page returns the row, served from the frame via the wal_index.
        s.evict_from_pool(pid);
        assert_eq!(
            s.read_page(pid).unwrap().body()[0],
            0x77,
            "served from the frame"
        );
        // A checkpoint applies it to the main file.
        s.checkpoint_frames(&|t| t == 5).unwrap();
        assert_eq!(
            s.read_page_raw(pid).unwrap()[HEADER_SIZE],
            0x77,
            "checkpoint wrote the page to the main file"
        );
    }
}
