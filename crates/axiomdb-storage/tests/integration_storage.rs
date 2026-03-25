//! Integration tests for the storage engine.
//!
//! Difference from unit tests in src/:
//! - Test end-to-end behavior, not internal implementations.
//! - Simulate crash recovery (drop + reopen).
//! - Verify behavioral equivalence between MmapStorage and MemoryStorage.
//! - Exercise the StorageEngine trait as a unified interface.

use axiomdb_storage::{MemoryStorage, MmapStorage, Page, PageType, StorageEngine};
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
fn test_crash_recovery_freelist_survives() {
    let dir = tmp_dir();
    let db_path = dir.path().join("test.db");
    let allocated_ids: Vec<u64>;

    {
        let mut engine = MmapStorage::create(&db_path).expect("create");
        allocated_ids = (0..5)
            .map(|_| engine.alloc_page(PageType::Data).expect("alloc"))
            .collect();
        // Free the first page.
        engine.free_page(allocated_ids[0]).expect("free");
        engine.flush().expect("flush");
    }

    // After reopening, the freelist remembers which pages were in use.
    {
        let mut engine = MmapStorage::open(&db_path).expect("reopen");
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
        let mut engine = MmapStorage::create(&db_path).expect("create");
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

    // free + realloc reuses.
    engine.free_page(id1).expect("free");
    let id_reused = engine.alloc_page(PageType::Data).expect("realloc");
    assert_eq!(id_reused, id1);

    // double-free: free id2 (still in use) and then free it again.
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
    let mut engine = MmapStorage::create(&db_path).expect("create");
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
    let mut engine = MemoryStorage::new();
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
    let mut engine = MmapStorage::create(&dir.path().join("empty_flush.db")).expect("create");
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
        let mut engine = MmapStorage::create(&db_path).expect("create");
        allocated = engine.alloc_page(PageType::Data).expect("alloc");
        engine.flush().expect("flush after freelist change");
    }

    // Reopen — freelist must remember `allocated` is in use.
    {
        let mut engine = MmapStorage::open(&db_path).expect("reopen");
        let next = engine.alloc_page(PageType::Data).expect("alloc");
        assert_ne!(
            next, allocated,
            "freelist flush did not persist: reused an in-use page"
        );
    }
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
        let offset = page_id as u64 * axiomdb_storage::PAGE_SIZE as u64
            + axiomdb_storage::HEADER_SIZE as u64
            + 100;
        file.seek(SeekFrom::Start(offset)).expect("seek");
        file.write_all(&[0xFFu8]).expect("write corruption");
    }

    // Reopen and verify that reading detects the corruption.
    {
        let engine = MmapStorage::open(&db_path).expect("reopen");
        let result = engine.read_page(page_id);
        assert!(
            result.is_err(),
            "invalid checksum should return an error, not corrupt data"
        );
    }
}
