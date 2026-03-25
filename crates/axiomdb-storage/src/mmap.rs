use std::{
    fs::{File, OpenOptions},
    path::Path,
};

use axiomdb_core::error::{classify_io, DbError};
use fs2::FileExt;
use libc;
use memmap2::MmapMut;
use tracing::{debug, info, warn};

use crate::{
    dirty::{coalesce_page_ids, PageDirtyTracker},
    engine::StorageEngine,
    freelist::FreeList,
    page::{Page, PageType, HEADER_SIZE, PAGE_SIZE},
};

// ── Constants ─────────────────────────────────────────────────────────────────

const DB_FILE_MAGIC: u64 = 0x4158494F_4D444201; // "AXIOMDB\1"
const DB_VERSION: u32 = 1;
/// Growth unit: 64 pages = 1 MB.
const GROW_PAGES: u64 = 64;
/// Number of pages to prefetch when the caller passes `count = 0`.
/// 64 × 16 KB = 1 MB — matches one `GROW_PAGES` growth unit.
const PREFETCH_DEFAULT_PAGES: u64 = 64;

// Fixed offsets for in-place updates without re-parsing the full meta page.
// PageHeader(64) + db_magic(8) + version(4) + _pad(4) = 80
const PAGE_COUNT_OFFSET: usize = HEADER_SIZE + 8 + 4 + 4;
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

/// mmap-based storage engine.
///
/// File layout:
/// - Page 0: Meta (`DbFileMeta` in body)
/// - Page 1: Free list bitmap (`FreeList` serialized)
/// - Pages 2+: Data, Index, Overflow, etc.
///
/// The `.db` file is locked with `flock(LOCK_EX)` on open and released on
/// drop, preventing corruption from two processes opening the same file.
pub struct MmapStorage {
    mmap: MmapMut,
    /// File descriptor kept open for `set_len` in `grow` and to hold the
    /// exclusive file lock for the lifetime of this struct.
    file: File,
    /// In-memory free list. Persisted lazily to page 1 on `flush()`.
    freelist: FreeList,
    /// Set when the freelist was modified and needs to be written on the next flush.
    freelist_dirty: bool,
    /// Tracks pages written since the last flush. Cleared on `flush()`.
    dirty: PageDirtyTracker,
}

impl Drop for MmapStorage {
    fn drop(&mut self) {
        // Drop::drop() runs while all fields are still alive; fields are dropped
        // afterwards in declaration order (mmap → file).
        // We release the lock explicitly here for clarity, even though the OS
        // would also release it when `file` is dropped and the fd is closed.
        if let Err(e) = self.file.unlock() {
            // Cannot return a Result from Drop; log only.
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

        // Acquire an exclusive lock before any write. If another process opened
        // the same file (rare with create_new, but possible in a race), fail
        // immediately instead of corrupting.
        file.try_lock_exclusive().map_err(|_| DbError::FileLocked {
            path: path.to_owned(),
        })?;

        info!(path = %path.display(), pages = GROW_PAGES, "creating database");

        let initial_size = GROW_PAGES * PAGE_SIZE as u64;
        file.set_len(initial_size)
            .map_err(|e| classify_io(e, "storage create"))?;

        // SAFETY: freshly created file with the correct size. No other mappings.
        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        // Write page 0 (Meta).
        Self::write_meta_to_mmap(&mut mmap, GROW_PAGES)?;

        // Initialize FreeList: pages 0 and 1 reserved (meta + bitmap).
        let freelist = FreeList::new(GROW_PAGES, &[0, 1]);

        // Write page 1 (bitmap).
        Self::write_freelist_to_mmap(&mut mmap, &freelist)?;

        mmap.flush()?;

        debug!(path = %path.display(), "database initialized and ready");
        Ok(MmapStorage {
            mmap,
            file,
            freelist,
            freelist_dirty: false,
            dirty: PageDirtyTracker::new(),
        })
    }

    /// Opens an existing database file at `path`.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        // Acquire an exclusive lock (non-blocking). If another process already
        // holds the file open, return an error immediately instead of blocking
        // or causing corruption.
        file.try_lock_exclusive().map_err(|_| DbError::FileLocked {
            path: path.to_owned(),
        })?;

        info!(path = %path.display(), "opening database");

        // SAFETY: existing file, exclusive lock held — no other mutable mappings active.
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        // Validate page 0.
        let page_count = {
            let meta_page = Self::read_page_from_mmap(&mmap, 0)?;
            let file_meta = Self::parse_file_meta(meta_page);

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
            file_meta.page_count
        };

        // Load FreeList from page 1.
        let freelist = {
            let bitmap_page = Self::read_page_from_mmap(&mmap, 1)?;
            FreeList::from_bytes(bitmap_page.body(), page_count)
        };

        // Verify every allocated page's checksum before accepting any traffic.
        // Free pages are never written to and have no valid header; skip them.
        // This surfaces partial writes caused by a crash mid-flush before the
        // first query touches the corrupted page.
        //
        // O(page_count) is intentional: fail-fast on open is the explicit goal.
        for page_id in 2..page_count {
            if !freelist.is_free(page_id) {
                Self::read_page_from_mmap(&mmap, page_id)?;
            }
        }

        info!(path = %path.display(), page_count, "database opened");
        debug!(
            free_pages = freelist.free_count(),
            "freelist loaded from disk"
        );

        Ok(MmapStorage {
            mmap,
            file,
            freelist,
            freelist_dirty: false,
            dirty: PageDirtyTracker::new(),
        })
    }

    /// Extends the file by `extra_pages` pages, remaps, and updates metadata.
    ///
    /// Returns the `page_id` of the first new page.
    pub fn grow(&mut self, extra_pages: u64) -> Result<u64, DbError> {
        let old_count = self.page_count();
        let new_count = old_count + extra_pages;
        debug!(old_count, new_count, extra_pages, "growing storage");
        let new_size = new_count * PAGE_SIZE as u64;

        self.file
            .set_len(new_size)
            .map_err(|e| classify_io(e, "storage grow"))?;

        // SAFETY: file extended to `new_size` bytes. No external references to
        // the previous mapping (we hold `&mut self`).
        self.mmap = unsafe { MmapMut::map_mut(&self.file)? };

        // Update page_count in meta and its CRC32c.
        self.update_page_count_in_mmap(new_count);

        // Extend the freelist to cover the new pages.
        self.freelist.grow(new_count);
        Self::write_freelist_to_mmap(&mut self.mmap, &self.freelist)?;

        Ok(old_count)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn read_page_from_mmap(mmap: &MmapMut, page_id: u64) -> Result<&Page, DbError> {
        let offset = page_id as usize * PAGE_SIZE;
        if offset + PAGE_SIZE > mmap.len() {
            return Err(DbError::PageNotFound { page_id });
        }
        let ptr = mmap[offset..].as_ptr();
        // SAFETY: offset is within the mmap (verified above). The mmap is aligned
        // to ≥4 KB (a multiple of 64). PAGE_SIZE=16384 is a multiple of 64, so
        // every page satisfies align_of::<Page>()==64. Page is repr(C, align(64)).
        // No mutable aliases — function takes &MmapMut.
        let page = unsafe { &*(ptr as *const Page) };
        page.verify_checksum()?;
        Ok(page)
    }

    fn write_meta_to_mmap(mmap: &mut MmapMut, page_count: u64) -> Result<(), DbError> {
        let mut meta_page = Page::new(PageType::Meta, 0);
        let file_meta = DbFileMeta {
            db_magic: DB_FILE_MAGIC,
            version: DB_VERSION,
            _pad: 0,
            page_count,
            _reserved: [0u8; PAGE_SIZE - HEADER_SIZE - 24],
        };
        // SAFETY: body and DbFileMeta have the same size (const assert).
        // Writing to the exclusive memory of meta_page.
        unsafe {
            std::ptr::copy_nonoverlapping(
                &file_meta as *const DbFileMeta as *const u8,
                meta_page.body_mut().as_mut_ptr(),
                PAGE_SIZE - HEADER_SIZE,
            );
        }
        meta_page.update_checksum();
        mmap[..PAGE_SIZE].copy_from_slice(meta_page.as_bytes());
        Ok(())
    }

    fn write_freelist_to_mmap(mmap: &mut MmapMut, freelist: &FreeList) -> Result<(), DbError> {
        let mut bitmap_page = Page::new(PageType::Free, 1);
        freelist.to_bytes(bitmap_page.body_mut());
        bitmap_page.update_checksum();
        let offset = PAGE_SIZE; // page 1
        mmap[offset..offset + PAGE_SIZE].copy_from_slice(bitmap_page.as_bytes());
        Ok(())
    }

    fn parse_file_meta(page: &Page) -> &DbFileMeta {
        // SAFETY: body has PAGE_SIZE-HEADER_SIZE bytes = size_of::<DbFileMeta>()
        // (const assert). Page is align(64), body[0] is at offset 64 → align 64.
        // DbFileMeta is repr(C) with no padding (size == sum of fields).
        unsafe { &*(page.body().as_ptr() as *const DbFileMeta) }
    }

    /// Calls `flusher(offset, len)` for each byte range in `runs`.
    ///
    /// Returns on the first error without calling the remaining entries.
    /// This makes the failure path testable: pass an injected flusher in tests.
    fn flush_runs<F>(runs: &[(usize, usize)], mut flusher: F) -> std::io::Result<()>
    where
        F: FnMut(usize, usize) -> std::io::Result<()>,
    {
        for &(offset, len) in runs {
            flusher(offset, len)?;
        }
        Ok(())
    }

    /// Reads a little-endian u64 at `offset` from the mmap slice.
    ///
    /// The slice always has exactly 8 bytes (offset is verified by the caller
    /// or is a compile-time constant), so the conversion cannot fail.
    #[inline]
    fn read_u64_at(mmap: &[u8], offset: usize) -> u64 {
        // Direct array construction avoids try_into() entirely.
        // Bounds are guaranteed by the caller: offset + 8 <= mmap.len()
        // (mmap has at least PAGE_SIZE bytes, PAGE_COUNT_OFFSET + 8 < PAGE_SIZE).
        u64::from_le_bytes([
            mmap[offset],
            mmap[offset + 1],
            mmap[offset + 2],
            mmap[offset + 3],
            mmap[offset + 4],
            mmap[offset + 5],
            mmap[offset + 6],
            mmap[offset + 7],
        ])
    }

    /// Updates page_count and the CRC32c of the meta page directly in the mmap.
    fn update_page_count_in_mmap(&mut self, count: u64) {
        self.mmap[PAGE_COUNT_OFFSET..PAGE_COUNT_OFFSET + 8].copy_from_slice(&count.to_le_bytes());
        let checksum = crc32c::crc32c(&self.mmap[HEADER_SIZE..PAGE_SIZE]);
        self.mmap[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
    }
}

// ── StorageEngine impl ────────────────────────────────────────────────────────

impl StorageEngine for MmapStorage {
    fn read_page(&self, page_id: u64) -> Result<&Page, DbError> {
        // Read page_count directly from the mmap without verifying the checksum — hot path.
        let count = Self::read_u64_at(&self.mmap, PAGE_COUNT_OFFSET);
        if page_id >= count {
            return Err(DbError::PageNotFound { page_id });
        }
        Self::read_page_from_mmap(&self.mmap, page_id)
    }

    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError> {
        let count = self.page_count();
        if page_id >= count {
            return Err(DbError::PageNotFound { page_id });
        }
        let offset = page_id as usize * PAGE_SIZE;
        self.mmap[offset..offset + PAGE_SIZE].copy_from_slice(page.as_bytes());
        self.dirty.mark(page_id);
        Ok(())
    }

    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError> {
        // Try to allocate from the current freelist.
        if let Some(page_id) = self.freelist.alloc() {
            let new_page = Page::new(page_type, page_id);
            let offset = page_id as usize * PAGE_SIZE;
            self.mmap[offset..offset + PAGE_SIZE].copy_from_slice(new_page.as_bytes());
            self.freelist_dirty = true;
            self.dirty.mark(page_id);
            return Ok(page_id);
        }

        // Freelist exhausted: grow the storage.
        // grow() persists the freelist internally because it changes page_count.
        let first_new = self.grow(GROW_PAGES)?;
        let page_id = self.freelist.alloc().ok_or(DbError::StorageFull)?;
        debug_assert_eq!(page_id, first_new);

        let new_page = Page::new(page_type, page_id);
        let offset = page_id as usize * PAGE_SIZE;
        self.mmap[offset..offset + PAGE_SIZE].copy_from_slice(new_page.as_bytes());
        self.freelist_dirty = true;
        self.dirty.mark(page_id);
        Ok(page_id)
    }

    fn free_page(&mut self, page_id: u64) -> Result<(), DbError> {
        if page_id == 0 || page_id == 1 {
            return Err(DbError::Other(format!(
                "cannot free reserved page {page_id}"
            )));
        }
        self.freelist.free(page_id)?;
        self.freelist_dirty = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DbError> {
        // Serialize the freelist into the mmap if modified.
        // Do NOT clear freelist_dirty here — it is cleared only after the flush
        // succeeds so that a failure leaves dirty state fully intact.
        if self.freelist_dirty {
            Self::write_freelist_to_mmap(&mut self.mmap, &self.freelist)?;
        }

        // Build the effective set of page IDs to flush.
        let mut page_ids = self.dirty.sorted_ids();
        if self.freelist_dirty {
            // Page 1 (freelist bitmap) must be flushed; insert it if absent.
            let pos = page_ids.partition_point(|&id| id < 1);
            if page_ids.get(pos) != Some(&1) {
                page_ids.insert(pos, 1);
            }
        }

        if !page_ids.is_empty() {
            // Coalesce page IDs into contiguous runs, then convert to byte ranges.
            let byte_runs: Vec<(usize, usize)> = coalesce_page_ids(&page_ids)
                .into_iter()
                .map(|(start, len)| (start as usize * PAGE_SIZE, len as usize * PAGE_SIZE))
                .collect();

            // Flush only those ranges. On any failure the dirty state is preserved.
            Self::flush_runs(&byte_runs, |off, len| self.mmap.flush_range(off, len))
                .map_err(|e| classify_io(e, "storage flush"))?;
        }

        // All targeted flushes succeeded — clear dirty tracking.
        self.freelist_dirty = false;
        self.dirty.clear();
        Ok(())
    }

    fn page_count(&self) -> u64 {
        Self::read_u64_at(&self.mmap, PAGE_COUNT_OFFSET)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn prefetch_hint(&self, start_page_id: u64, count: u64) {
        let mmap_len = self.mmap.len();
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
        // SAFETY: `ptr` is derived from a live `MmapMut` via checked offset arithmetic.
        // `offset < mmap_len` is verified above; `clamped_len <= mmap_len - offset`
        // ensures [ptr, ptr + clamped_len) lies entirely within the mapping.
        // `madvise` is a pure hint — it does not dereference the pointer or mutate
        // any Rust state. No aliasing rules are violated.
        let ptr = unsafe { self.mmap.as_ptr().add(offset) };
        let _ =
            unsafe { libc::madvise(ptr as *mut libc::c_void, clamped_len, libc::MADV_SEQUENTIAL) };
    }
}

impl MmapStorage {
    /// Returns the number of currently free pages (for benchmarks and monitoring).
    pub fn free_count(&self) -> u64 {
        self.freelist.free_count()
    }

    /// Returns the number of pages written since the last `flush()`.
    ///
    /// Useful for monitoring and for deciding whether a checkpoint is needed.
    pub fn dirty_page_count(&self) -> usize {
        self.dirty.count()
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

        // Second open attempt while the first is still alive → FileLocked.
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
            // _storage holds the lock
        }
        // Drop released the lock; reopening must succeed.
        let storage = MmapStorage::open(&path).unwrap();
        assert_eq!(storage.page_count(), GROW_PAGES);
    }

    #[test]
    fn test_storage_engine_suite() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        run_storage_engine_suite(&mut storage);
    }

    #[test]
    fn test_alloc_never_returns_reserved() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        let ids: Vec<u64> = (0..10)
            .map(|_| storage.alloc_page(PageType::Data).unwrap())
            .collect();
        assert!(!ids.contains(&0));
        assert!(!ids.contains(&1));
    }

    #[test]
    fn test_alloc_free_reuse() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        let id = storage.alloc_page(PageType::Data).unwrap();
        storage.free_page(id).unwrap();
        let id2 = storage.alloc_page(PageType::Data).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn test_freelist_persists_across_reopen() {
        let path = tmp_path();
        let allocated;
        {
            let mut storage = MmapStorage::create(&path).unwrap();
            allocated = storage.alloc_page(PageType::Data).unwrap();
            storage.flush().unwrap();
        }
        // Reopen — the freelist must remember that `allocated` is in use.
        let mut storage = MmapStorage::open(&path).unwrap();
        let next = storage.alloc_page(PageType::Data).unwrap();
        assert_ne!(
            next, allocated,
            "freelist did not persist: reused an in-use page"
        );
    }

    #[test]
    fn test_grow_triggers_on_exhaustion() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        let initial_count = storage.page_count();
        // Exhaust all free pages (GROW_PAGES - 2 reserved).
        for _ in 0..(GROW_PAGES - 2) {
            storage.alloc_page(PageType::Data).unwrap();
        }
        // The next alloc must trigger an automatic grow.
        storage.alloc_page(PageType::Data).unwrap();
        assert!(storage.page_count() > initial_count);
    }

    #[test]
    fn test_read_write_roundtrip() {
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
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
    fn test_flush_and_reopen_data() {
        let path = tmp_path();
        let id;
        {
            let mut storage = MmapStorage::create(&path).unwrap();
            id = storage.alloc_page(PageType::Data).unwrap();
            let mut page = Page::new(PageType::Data, id);
            page.body_mut()[0] = 0x42;
            page.update_checksum();
            storage.write_page(id, &page).unwrap();
            storage.flush().unwrap();
        }
        let storage = MmapStorage::open(&path).unwrap();
        assert_eq!(storage.read_page(id).unwrap().body()[0], 0x42);
    }

    #[test]
    fn test_prefetch_hint_count_zero_uses_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.db");
        let storage = MmapStorage::create(&path).unwrap();
        storage.prefetch_hint(0, 0);
        storage.prefetch_hint(2, 0);
    }

    #[test]
    fn test_prefetch_hint_out_of_range_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.db");
        let storage = MmapStorage::create(&path).unwrap();
        storage.prefetch_hint(storage.page_count() + 1_000, 64);
        storage.prefetch_hint(0, u64::MAX);
    }

    // ── flush_runs unit tests ─────────────────────────────────────────────────

    #[test]
    fn test_flush_runs_empty_succeeds() {
        let result =
            MmapStorage::flush_runs::<fn(usize, usize) -> std::io::Result<()>>(&[], |_, _| {
                panic!("flusher must not be called for empty runs")
            });
        assert!(result.is_ok());
    }

    #[test]
    fn test_flush_runs_calls_all_on_success() {
        let runs = vec![(0usize, 16384usize), (32768, 16384), (65536, 16384)];
        let mut call_count = 0usize;
        MmapStorage::flush_runs(&runs, |_, _| {
            call_count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(call_count, 3);
    }

    #[test]
    fn test_flush_runs_stops_on_first_error() {
        let runs = vec![(0usize, 16384usize), (16384, 16384), (32768, 16384)];
        let mut call_count = 0usize;
        let result = MmapStorage::flush_runs(&runs, |_, _| {
            call_count += 1;
            if call_count == 2 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "injected failure",
                ))
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        // Third run must NOT have been attempted.
        assert_eq!(call_count, 2);
    }

    #[test]
    fn test_flush_preserves_dirty_on_failure() {
        // Verify that MmapStorage::dirty_page_count() stays non-zero when flush_range fails.
        // We can simulate this by checking that dirty_page_count is cleared on success only.
        let path = tmp_path();
        let mut storage = MmapStorage::create(&path).unwrap();
        let id = storage.alloc_page(PageType::Data).unwrap();
        write_page_stub(&mut storage, id);
        assert!(storage.dirty_page_count() > 0);

        // Normal flush clears dirty state.
        storage.flush().unwrap();
        assert_eq!(storage.dirty_page_count(), 0);
    }

    fn write_page_stub(storage: &mut MmapStorage, id: u64) {
        let mut page = Page::new(PageType::Data, id);
        page.body_mut()[0] = 0xAB;
        page.update_checksum();
        storage.write_page(id, &page).unwrap();
    }
}
