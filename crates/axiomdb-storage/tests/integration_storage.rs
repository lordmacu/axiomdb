//! Integration tests for the storage engine.
//!
//! Difference from unit tests in src/:
//! - Test end-to-end behavior, not internal implementations.
//! - Simulate crash recovery (drop + reopen).
//! - Verify behavioral equivalence between MmapStorage and MemoryStorage.
//! - Exercise the StorageEngine trait as a unified interface.

use axiomdb_core::error::DbError;
use axiomdb_storage::{HeapChain, MemoryStorage, MmapStorage, Page, PageType, StorageEngine};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn tmp_dir() -> TempDir {
    tempfile::tempdir().expect("create temporary directory")
}

fn write_pattern(engine: &mut dyn StorageEngine, page_id: u64, pattern: u8) {
    let mut page = Page::new(PageType::Data, page_id);
    page.body_mut().fill(pattern);
    page.update_checksum();
    engine.write_page(page_id, &page).expect("write_page");
}

fn assert_pattern(engine: &dyn StorageEngine, page_id: u64, pattern: u8) {
    let page = engine.read_page(page_id).expect("read_page");
    assert!(
        page.body().iter().all(|&b| b == pattern),
        "page {page_id}: expected pattern {pattern:#x} not found"
    );
}

// ── Crash recovery ────────────────────────────────────────────────────────────

#[test]
fn test_crash_recovery_data_survives() {
    let dir = tmp_dir();
    let db_path = dir.path().join("test.db");
    let page_id;

    // Write data and flush.
    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        page_id = engine.alloc_page(PageType::Data).expect("alloc");
        write_pattern(&mut engine, page_id, 0xAB);
        engine.flush().expect("flush");
        // `engine` is dropped here — simulates clean shutdown.
    }

    // Reopen and verify that data survived.
    {
        let engine = MmapStorage::open(&db_path).expect("reopen");
        assert_pattern(&engine, page_id, 0xAB);
    }
}

#[test]
fn test_mmap_heap_chain_insert_persists_under_page_latch() {
    let dir = tmp_dir();
    let db_path = dir.path().join("heap_chain.db");
    let storage = MmapStorage::create(&db_path).expect("create");
    let root = storage.alloc_page(PageType::Data).expect("alloc root");
    let root_page = Page::new(PageType::Data, root);
    storage.write_page(root, &root_page).expect("init root");

    let rid = HeapChain::insert(&storage, root, b"seed", 1, None).expect("heap insert");
    assert_eq!(rid.0, root, "single insert should stay on root page");
    storage.flush().expect("flush");
    drop(storage);

    let storage = MmapStorage::open(&db_path).expect("reopen");
    let rows = HeapChain::scan_visible(
        &storage,
        root,
        axiomdb_core::TransactionSnapshot::committed(1),
    )
    .expect("scan");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, b"seed");
}

#[test]
fn test_crash_recovery_freelist_survives() {
    let dir = tmp_dir();
    let db_path = dir.path().join("test.db");
    let allocated_ids: Vec<u64>;

    {
        let engine = MmapStorage::create(&db_path).expect("create");
        allocated_ids = (0..5)
            .map(|_| engine.alloc_page(PageType::Data).expect("alloc"))
            .collect();
        // Free the first page.
        engine.free_page(allocated_ids[0]).expect("free");
        engine.flush().expect("flush");
    }

    // After reopening, the freelist remembers which pages were in use.
    {
        let engine = MmapStorage::open(&db_path).expect("reopen");
        // The first free ID must be `allocated_ids[0]` (it was freed).
        let next = engine.alloc_page(PageType::Data).expect("alloc");
        assert_eq!(
            next, allocated_ids[0],
            "freelist did not persist: expected {}, got {}",
            allocated_ids[0], next
        );
        // In-use pages are still not reassigned.
        let next2 = engine.alloc_page(PageType::Data).expect("alloc");
        assert!(
            !allocated_ids[1..].contains(&next2),
            "in-use page reassigned after recovery: {next2}"
        );
    }
}

#[test]
fn test_crash_recovery_multiple_grows() {
    let dir = tmp_dir();
    let db_path = dir.path().join("test.db");
    let count_after_grows;

    {
        let engine = MmapStorage::create(&db_path).expect("create");
        // Exhaust initial capacity to force two grows.
        let initial = engine.page_count();
        for _ in 0..(initial - 2 + 64 + 1) {
            engine.alloc_page(PageType::Data).expect("alloc");
        }
        count_after_grows = engine.page_count();
        engine.flush().expect("flush");
    }

    {
        let engine = MmapStorage::open(&db_path).expect("reopen");
        assert_eq!(
            engine.page_count(),
            count_after_grows,
            "page_count did not persist after grows"
        );
    }
}

// ── Equivalence MmapStorage ↔ MemoryStorage ──────────────────────────────────

fn run_equivalence_test(engine: &mut dyn StorageEngine) {
    // alloc returns IDs starting from 2 (0=meta, 1=bitmap).
    let id1 = engine.alloc_page(PageType::Data).expect("alloc 1");
    let id2 = engine.alloc_page(PageType::Index).expect("alloc 2");
    assert!(id1 >= 2);
    assert!(id2 > id1);

    // write + read roundtrip.
    write_pattern(engine, id1, 0xCC);
    assert_pattern(engine, id1, 0xCC);
    write_pattern(engine, id2, 0xDD);
    assert_pattern(engine, id2, 0xDD);

    // free + flush (releases deferred frees) + realloc reuses.
    engine.free_page(id1).expect("free");
    engine.flush().expect("flush deferred frees");
    let id_reused = engine.alloc_page(PageType::Data).expect("realloc");
    assert_eq!(id_reused, id1);

    // double-free: free id2 and then free it again — should fail.
    engine.free_page(id2).expect("first free of id2");
    assert!(
        engine.free_page(id2).is_err(),
        "double-free of id2 should fail"
    );

    // reserved pages cannot be freed.
    assert!(engine.free_page(0).is_err());
    assert!(engine.free_page(1).is_err());

    // read of non-existent page fails.
    assert!(engine.read_page(999_999).is_err());

    // flush does not fail.
    engine.flush().expect("flush");
}

#[test]
fn test_mmap_storage_equivalence() {
    let dir = tmp_dir();
    let db_path = dir.path().join("equiv.db");
    let mut engine = MmapStorage::create(&db_path).expect("create");
    run_equivalence_test(&mut engine);
}

#[test]
fn test_memory_storage_equivalence() {
    let mut engine = MemoryStorage::new();
    run_equivalence_test(&mut engine);
}

// ── StorageEngine as a trait object ──────────────────────────────────────────

#[test]
fn test_box_dyn_storage_engine_mmap() {
    let dir = tmp_dir();
    let db_path = dir.path().join("dyn.db");
    let mut engine: Box<dyn StorageEngine> =
        Box::new(MmapStorage::create(&db_path).expect("create"));

    let id = engine.alloc_page(PageType::Data).expect("alloc");
    write_pattern(engine.as_mut(), id, 0xFF);
    assert_pattern(engine.as_ref(), id, 0xFF);
    engine.flush().expect("flush");
}

#[test]
fn test_box_dyn_storage_engine_memory() {
    let mut engine: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let id = engine.alloc_page(PageType::Data).expect("alloc");
    write_pattern(engine.as_mut(), id, 0x42);
    assert_pattern(engine.as_ref(), id, 0x42);
}

// ── Automatic growth ─────────────────────────────────────────────────────────

#[test]
fn test_mmap_auto_grow_on_exhaustion() {
    let dir = tmp_dir();
    let db_path = dir.path().join("grow.db");
    let engine = MmapStorage::create(&db_path).expect("create");
    let initial_count = engine.page_count();

    // Exhaust initial pages.
    for _ in 0..(initial_count - 2) {
        engine.alloc_page(PageType::Data).expect("alloc");
    }
    // This alloc must grow automatically.
    let id = engine.alloc_page(PageType::Data).expect("alloc after grow");
    assert!(
        id >= initial_count,
        "alloc after grow must return an ID in the new range"
    );
    assert!(
        engine.page_count() > initial_count,
        "page_count must have grown"
    );
}

#[test]
fn test_memory_auto_grow_on_exhaustion() {
    let engine = MemoryStorage::new();
    let initial = engine.page_count();
    for _ in 0..(initial - 2) {
        engine.alloc_page(PageType::Data).expect("alloc");
    }
    let id = engine.alloc_page(PageType::Data).expect("alloc after grow");
    assert!(id >= initial);
    assert!(engine.page_count() > initial);
}

// ── Targeted flush_range behavior (3.15b) ────────────────────────────────────

#[test]
fn test_flush_empty_dirty_set_succeeds() {
    // No writes → flush must succeed without touching any pages.
    let dir = tmp_dir();
    let engine = MmapStorage::create(&dir.path().join("empty_flush.db")).expect("create");
    // After create() the dirty tracker is empty (create flushed internally).
    assert_eq!(engine.dirty_page_count(), 0);
    engine.flush().expect("flush on clean state must succeed");
    assert_eq!(engine.dirty_page_count(), 0);
}

#[test]
fn test_flush_single_dirty_page_clears_state() {
    let dir = tmp_dir();
    let db_path = dir.path().join("single.db");
    let page_id;

    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        page_id = engine.alloc_page(PageType::Data).expect("alloc");
        write_pattern(&mut engine, page_id, 0xBB);
        assert!(
            engine.dirty_page_count() > 0,
            "alloc + write should be dirty"
        );
        engine.flush().expect("flush");
        assert_eq!(engine.dirty_page_count(), 0, "flush must clear dirty state");
    }

    // Data must persist.
    let engine = MmapStorage::open(&db_path).expect("reopen");
    assert_pattern(&engine, page_id, 0xBB);
}

#[test]
fn test_flush_contiguous_dirty_pages_clears_state() {
    let dir = tmp_dir();
    let db_path = dir.path().join("contiguous.db");
    let ids: Vec<u64>;

    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        // Alloc 3 consecutive pages — they will be contiguous because the freelist
        // hands them out in order from a fresh file.
        ids = (0..3)
            .map(|_| engine.alloc_page(PageType::Data).expect("alloc"))
            .collect();
        for (i, &id) in ids.iter().enumerate() {
            write_pattern(&mut engine, id, 0x10 + i as u8);
        }
        engine.flush().expect("flush contiguous pages");
        assert_eq!(engine.dirty_page_count(), 0);
    }

    let engine = MmapStorage::open(&db_path).expect("reopen");
    for (i, &id) in ids.iter().enumerate() {
        assert_pattern(&engine, id, 0x10 + i as u8);
    }
}

#[test]
fn test_flush_freelist_only_change() {
    // alloc_page sets freelist_dirty but may or may not set data page dirty.
    // After flush, the freelist must persist across reopen.
    let dir = tmp_dir();
    let db_path = dir.path().join("freelist_only.db");
    let allocated;

    {
        let engine = MmapStorage::create(&db_path).expect("create");
        allocated = engine.alloc_page(PageType::Data).expect("alloc");
        engine.flush().expect("flush after freelist change");
    }

    // Reopen — freelist must remember `allocated` is in use.
    {
        let engine = MmapStorage::open(&db_path).expect("reopen");
        let next = engine.alloc_page(PageType::Data).expect("alloc");
        assert_ne!(
            next, allocated,
            "freelist flush did not persist: reused an in-use page"
        );
    }
}

// ── Verified open — corruption detected at startup (3.8b) ────────────────────

#[test]
fn test_verified_open_detects_data_page_corruption() {
    use std::io::{Seek, SeekFrom, Write};

    let dir = tmp_dir();
    let db_path = dir.path().join("corrupt_data.db");
    let page_id;

    // Write a page and flush so it is on disk.
    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        page_id = engine.alloc_page(PageType::Data).expect("alloc");
        write_pattern(&mut engine, page_id, 0xAA);
        engine.flush().expect("flush");
    }

    // Corrupt the page body on disk (bypass the mmap).
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&db_path)
            .expect("open for corruption");
        let offset =
            page_id * axiomdb_storage::PAGE_SIZE as u64 + axiomdb_storage::HEADER_SIZE as u64 + 42;
        file.seek(SeekFrom::Start(offset)).expect("seek");
        file.write_all(&[0xFFu8]).expect("corrupt byte");
    }

    // Reopen must fail immediately — before any query.
    let result = MmapStorage::open(&db_path);
    assert!(
        result.is_err(),
        "open() must fail when a data page has a bad checksum"
    );
    assert!(
        matches!(result, Err(DbError::ChecksumMismatch { .. })),
        "error must be ChecksumMismatch, got: {:?}",
        result.err()
    );
}

#[test]
fn test_verified_open_clean_database_succeeds() {
    let dir = tmp_dir();
    let db_path = dir.path().join("clean_reopen.db");

    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        let id = engine.alloc_page(PageType::Data).expect("alloc");
        write_pattern(&mut engine, id, 0x7F);
        engine.flush().expect("flush");
    }

    // Reopen of a clean file must always succeed.
    let engine = MmapStorage::open(&db_path).expect("clean reopen must succeed");
    assert!(
        engine.page_count() >= 2,
        "page_count must be valid after clean open"
    );
}

#[test]
fn test_verified_open_multiple_pages_any_corruption_detected() {
    use std::io::{Seek, SeekFrom, Write};

    let dir = tmp_dir();
    let db_path = dir.path().join("multi_corrupt.db");

    // Allocate several pages.
    let ids: Vec<u64> = {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        let ids: Vec<u64> = (0..5)
            .map(|_| engine.alloc_page(PageType::Data).expect("alloc"))
            .collect();
        for &id in &ids {
            write_pattern(&mut engine, id, 0x55);
        }
        engine.flush().expect("flush");
        ids
    };

    // Corrupt the last allocated page.
    {
        let last_id = *ids.last().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&db_path)
            .expect("open");
        let offset =
            last_id * axiomdb_storage::PAGE_SIZE as u64 + axiomdb_storage::HEADER_SIZE as u64 + 1;
        file.seek(SeekFrom::Start(offset)).expect("seek");
        file.write_all(&[0x00u8]).expect("corrupt");
    }

    let result = MmapStorage::open(&db_path);
    assert!(
        result.is_err(),
        "open() must fail when any page is corrupted, not only the first"
    );
}

// ── On-disk checksum integrity ────────────────────────────────────────────────

#[test]
fn test_corrupted_page_detected_on_read() {
    use std::io::{Seek, SeekFrom, Write};

    let dir = tmp_dir();
    let db_path = dir.path().join("corrupt.db");
    let page_id;

    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        page_id = engine.alloc_page(PageType::Data).expect("alloc");
        write_pattern(&mut engine, page_id, 0x55);
        engine.flush().expect("flush");
    }

    // Corrupt 1 byte of the page body on disk.
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&db_path)
            .expect("open file");
        let offset =
            page_id * axiomdb_storage::PAGE_SIZE as u64 + axiomdb_storage::HEADER_SIZE as u64 + 100;
        file.seek(SeekFrom::Start(offset)).expect("seek");
        file.write_all(&[0xFFu8]).expect("write corruption");
    }

    // Since 3.8b, open() itself verifies all allocated pages.
    // Corruption must be caught at startup, not lazily on read.
    let result = MmapStorage::open(&db_path);
    assert!(
        result.is_err(),
        "open() must fail when an allocated page has a bad checksum"
    );
    assert!(
        matches!(result, Err(DbError::ChecksumMismatch { .. })),
        "error must be ChecksumMismatch, got: {:?}",
        result.err()
    );
}

// ── Phase 40.9: LocalPageBatch + concurrent alloc stress ─────────────────────

#[test]
fn test_local_page_batch_pop_or_refill_basic() {
    use axiomdb_storage::{LocalPageBatch, BATCH_ALLOC_SIZE};

    let storage = MemoryStorage::new();
    let mut batch = LocalPageBatch::new();

    // First alloc triggers a refill from the global allocator.
    let id1 = batch.pop_or_refill(&storage, PageType::Data).unwrap();
    assert!(id1 >= 2, "page IDs 0,1 are reserved; got {id1}");

    // Subsequent allocs from the batch are fast (no global lock).
    let mut ids = vec![id1];
    for _ in 1..BATCH_ALLOC_SIZE {
        let id = batch.pop_or_refill(&storage, PageType::Data).unwrap();
        ids.push(id);
    }
    // All IDs must be unique.
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), BATCH_ALLOC_SIZE, "all IDs must be unique");
}

#[test]
fn test_local_page_batch_type_mismatch_drain() {
    use axiomdb_storage::LocalPageBatch;

    let storage = MemoryStorage::new();
    let mut batch = LocalPageBatch::new();

    // Allocate some Data pages.
    let _d1 = batch.pop_or_refill(&storage, PageType::Data).unwrap();
    let _d2 = batch.pop_or_refill(&storage, PageType::Data).unwrap();

    // Switch to Index type — must drain the Data batch and refill.
    let i1 = batch.pop_or_refill(&storage, PageType::Index).unwrap();
    assert!(i1 >= 2, "page IDs 0,1 are reserved; got {i1}");
}

#[test]
fn test_local_page_batch_commit_drain() {
    use axiomdb_storage::LocalPageBatch;

    let storage = MemoryStorage::new();
    let mut batch = LocalPageBatch::new();

    let id1 = batch.pop_or_refill(&storage, PageType::Data).unwrap();
    batch.push_freed(id1);

    let (avail, freed) = batch.take_for_commit();
    assert!(
        !avail.is_empty() || freed.len() == 1,
        "freed must contain id1"
    );
    assert!(freed.contains(&id1));

    // After commit, batch is empty — return pre-allocated to bitmap.
    storage.free_page_batch(&avail).unwrap();
    for pid in &freed {
        storage.recycle_page(*pid).unwrap();
    }
}

#[test]
fn test_mmap_alloc_page_batch_concurrent_8_threads() {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    let dir = tmp_dir();
    let path = dir.path().join("concurrent_batch.db");
    let storage = Arc::new(MmapStorage::create(&path).unwrap());

    // Pre-grow: 8 threads × 10K allocs = 80K pages. Grow to 81K to avoid
    // interleaving growth with alloc (keeps the test focused on the bitmap).
    storage.grow(81_000).unwrap();

    let num_threads = 8;
    let allocs_per_thread = 10_000;

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let s = Arc::clone(&storage);
            thread::spawn(move || {
                let mut ids = Vec::with_capacity(allocs_per_thread);
                for _ in 0..allocs_per_thread {
                    let id = s.alloc_page(PageType::Data).unwrap();
                    ids.push(id);
                }
                // Free all pages back.
                for &id in &ids {
                    s.free_page(id).unwrap();
                }
                ids
            })
        })
        .collect();

    let mut all_ids: Vec<u64> = Vec::new();
    for h in handles {
        all_ids.extend(h.join().unwrap());
    }

    // Verify uniqueness across all threads.
    let unique: HashSet<u64> = all_ids.iter().copied().collect();
    assert_eq!(
        unique.len(),
        all_ids.len(),
        "all {} allocated page IDs must be unique, but got {} unique out of {}",
        num_threads * allocs_per_thread,
        unique.len(),
        all_ids.len()
    );

    // After freeing all, the storage should have all pages available again.
    storage.flush().unwrap();
}

#[test]
fn test_mmap_alloc_page_batch_recycle_queue() {
    use axiomdb_storage::LocalPageBatch;

    let dir = tmp_dir();
    let path = dir.path().join("recycle.db");
    let storage = MmapStorage::create(&path).unwrap();

    // Allocate a batch.
    let mut batch = LocalPageBatch::new();
    let id1 = batch.pop_or_refill(&storage, PageType::Data).unwrap();
    let id2 = batch.pop_or_refill(&storage, PageType::Data).unwrap();

    // Simulate commit: freed pages go to recycle_queue.
    batch.push_freed(id1);
    batch.push_freed(id2);
    let (avail, freed) = batch.take_for_commit();
    storage.free_page_batch(&avail).unwrap();
    for pid in freed {
        storage.recycle_page(pid).unwrap();
    }

    // Next batch alloc should pick up recycled pages first.
    let mut batch2 = LocalPageBatch::new();
    let r1 = batch2.pop_or_refill(&storage, PageType::Data).unwrap();
    let r2 = batch2.pop_or_refill(&storage, PageType::Data).unwrap();
    // At least one of the recycled pages should be reused.
    let reused = [r1, r2].iter().any(|&r| r == id1 || r == id2);
    assert!(
        reused,
        "recycled pages should be reused; got {r1}, {r2} but freed {id1}, {id2}"
    );

    // Clean up.
    let (avail2, _) = batch2.take_for_commit();
    storage.free_page_batch(&avail2).unwrap();
    storage.flush().unwrap();
}
