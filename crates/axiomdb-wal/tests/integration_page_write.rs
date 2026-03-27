//! PageWrite WAL entry integration tests — Phase 3.18.
//!
//! Tests cover:
//! - EntryType::PageWrite round-trip through WalEntry serialization.
//! - record_page_writes() produces WAL entries readable by WalReader.
//! - Crash recovery undoes uncommitted PageWrite by marking slots dead.
//! - Committed PageWrite rows are visible after clean restart.
//! - record_page_writes with 0 pages is a no-op.

use std::path::PathBuf;

use axiomdb_storage::{
    heap::{insert_tuple, read_tuple},
    MmapStorage, Page, PageType, StorageEngine,
};
use axiomdb_wal::{CrashRecovery, EntryType, TxnManager, WalEntry, WalReader};
use tempfile::TempDir;

// ── TestEnv ───────────────────────────────────────────────────────────────────

struct TestEnv {
    _dir: TempDir,
    pub db: PathBuf,
    pub wal: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create test tmp dir");
        let db = dir.path().join("test.db");
        let wal = dir.path().join("test.wal");
        Self { _dir: dir, db, wal }
    }
}

fn make_storage_with_page(env: &TestEnv) -> (MmapStorage, u64) {
    let mut storage = MmapStorage::create(&env.db).unwrap();
    let page_id = storage.alloc_page(PageType::Data).unwrap();
    let page = Page::new(PageType::Data, page_id);
    storage.write_page(page_id, &page).unwrap();
    (storage, page_id)
}

// Insert rows into a page and return (page_id, Vec<slot_id>).
fn insert_rows_on_page(
    storage: &mut MmapStorage,
    txn: &mut TxnManager,
    page_id: u64,
    rows: &[&[u8]],
) -> Vec<u16> {
    let txn_id = txn.active_txn_id().unwrap();
    let raw = *storage.read_page(page_id).unwrap().as_bytes();
    let mut page = Page::from_bytes(raw).unwrap();
    let mut slot_ids = Vec::new();
    for &row in rows {
        let slot_id = insert_tuple(&mut page, row, txn_id).unwrap();
        slot_ids.push(slot_id);
    }
    storage.write_page(page_id, &page).unwrap();
    slot_ids
}

// ── EntryType::PageWrite round-trip ──────────────────────────────────────────

#[test]
fn test_page_write_entry_type_value() {
    // PageWrite = 9, must not conflict with existing types.
    assert_eq!(EntryType::PageWrite as u8, 9);
    assert_eq!(EntryType::Truncate as u8, 8);
}

#[test]
fn test_page_write_entry_roundtrip() {
    // Compact format: [num_slots: u16 LE][slot_ids: u16 LE each] — no page bytes.
    let slot_ids: &[u16] = &[0, 1, 2];
    let page_id: u64 = 42;

    let mut new_value = Vec::with_capacity(2 + slot_ids.len() * 2);
    new_value.extend_from_slice(&(slot_ids.len() as u16).to_le_bytes());
    for &s in slot_ids {
        new_value.extend_from_slice(&s.to_le_bytes());
    }

    let entry = WalEntry::new(
        1,
        7,
        EntryType::PageWrite,
        1,
        page_id.to_le_bytes().to_vec(),
        vec![],
        new_value.clone(),
    );

    let bytes = entry.to_bytes();
    let (parsed, consumed) = WalEntry::from_bytes(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(parsed.entry_type, EntryType::PageWrite);
    assert_eq!(parsed.key, page_id.to_le_bytes());
    assert_eq!(parsed.old_value, Vec::<u8>::new());
    // num_slots at offset 0
    let ns = u16::from_le_bytes([parsed.new_value[0], parsed.new_value[1]]);
    assert_eq!(ns, 3);
    // slot_ids follow immediately
    let s0 = u16::from_le_bytes([parsed.new_value[2], parsed.new_value[3]]);
    assert_eq!(s0, 0);
}

// ── record_page_writes ────────────────────────────────────────────────────────

#[test]
fn test_record_page_writes_empty_is_noop() {
    let env = TestEnv::new();
    let (_, _) = make_storage_with_page(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();

    txn.begin().unwrap();
    txn.record_page_writes(1, &[]).unwrap();
    txn.commit().unwrap();
    // No panic, no error.
}

#[test]
fn test_record_page_writes_produces_readable_wal_entries() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage_with_page(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();

    txn.begin().unwrap();
    let slot_ids = insert_rows_on_page(
        &mut storage,
        &mut txn,
        page_id,
        &[b"row0", b"row1", b"row2"],
    );
    txn.record_page_writes(1, &[(page_id, &slot_ids)]).unwrap();
    txn.commit().unwrap();

    // Read back WAL and find the PageWrite entry.
    let reader = WalReader::open(&env.wal).unwrap();
    let entries: Vec<_> = reader
        .scan_forward(0)
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|e| e.entry_type == EntryType::PageWrite)
        .collect();

    assert_eq!(entries.len(), 1, "expected exactly 1 PageWrite entry");
    let e = &entries[0];

    // Verify key = page_id LE
    assert_eq!(e.key.len(), 8);
    let recovered_pid = u64::from_le_bytes(e.key[..8].try_into().unwrap());
    assert_eq!(recovered_pid, page_id);

    // Compact format: [num_slots: u16 LE][slot_ids: u16 LE each] — no page bytes.
    assert!(e.new_value.len() >= 2 + slot_ids.len() * 2);

    // Verify num_slots at offset 0
    let ns = u16::from_le_bytes([e.new_value[0], e.new_value[1]]) as usize;
    assert_eq!(ns, slot_ids.len());

    // Verify slot_ids starting at offset 2
    for (i, &expected_slot) in slot_ids.iter().enumerate() {
        let off = 2 + i * 2;
        let got = u16::from_le_bytes([e.new_value[off], e.new_value[off + 1]]);
        assert_eq!(got, expected_slot);
    }
}

// ── Crash recovery ────────────────────────────────────────────────────────────

/// Crash without commit — uncommitted PageWrite must be undone on recovery.
#[test]
fn test_crash_recovery_undoes_uncommitted_page_write() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage_with_page(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();

    txn.begin().unwrap();
    let slot_ids = insert_rows_on_page(
        &mut storage,
        &mut txn,
        page_id,
        &[b"should be lost", b"also lost", b"gone too"],
    );
    txn.record_page_writes(1, &[(page_id, &slot_ids)]).unwrap();
    // Flush to OS page cache (simulates crash — WAL in kernel buffer, no fsync).
    // Drop without commit: undo_ops lost, WAL has Begin+PageWrite but no Commit.
    drop(txn);

    // Recovery must undo the PageWrite: mark all embedded slot_ids dead.
    let (_, result) = TxnManager::open_with_recovery(&mut storage, &env.wal).unwrap();
    assert_eq!(result.undone_txns, 1, "expected 1 in-progress txn undone");

    // Verify each slot is now dead (read_tuple returns None for dead slots).
    let raw = *storage.read_page(page_id).unwrap().as_bytes();
    let page = Page::from_bytes(raw).unwrap();
    for &slot_id in &slot_ids {
        // A dead slot either returns None from read_tuple (slot killed) or
        // read_tuple returns Some but with txn_id_deleted set.
        // mark_slot_dead zeros the slot entry so read_tuple returns None.
        let result = read_tuple(&page, slot_id).unwrap();
        assert!(
            result.is_none(),
            "slot {slot_id} should be dead after recovery of uncommitted PageWrite"
        );
    }
}

/// Clean commit — rows must be visible after restart (no recovery needed).
#[test]
fn test_committed_page_write_rows_visible_after_restart() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage_with_page(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();

    txn.begin().unwrap();
    let slot_ids = insert_rows_on_page(
        &mut storage,
        &mut txn,
        page_id,
        &[b"row_a", b"row_b", b"row_c"],
    );
    txn.record_page_writes(1, &[(page_id, &slot_ids)]).unwrap();
    txn.commit().unwrap(); // fsync — durable

    drop(txn);

    // Reopen — no recovery needed (clean commit).
    let txn2 = TxnManager::open(&env.wal).unwrap();
    assert!(
        !CrashRecovery::is_needed(&storage, &env.wal).unwrap(),
        "recovery must not be needed after clean commit"
    );

    // Rows must still be visible.
    let snap = txn2.snapshot();
    let raw = *storage.read_page(page_id).unwrap().as_bytes();
    let page = Page::from_bytes(raw).unwrap();
    for (&slot_id, expected_data) in slot_ids.iter().zip([b"row_a".as_ref(), b"row_b", b"row_c"]) {
        let (header, data) = read_tuple(&page, slot_id)
            .unwrap()
            .expect("committed row must exist");
        let visible = header.txn_id_created < snap.snapshot_id;
        assert!(visible, "committed row slot {slot_id} must be visible");
        assert_eq!(data, expected_data);
    }
}

/// Recovery idempotency — running recovery twice must not corrupt the heap.
#[test]
fn test_page_write_recovery_is_idempotent() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage_with_page(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();

    txn.begin().unwrap();
    let slot_ids = insert_rows_on_page(&mut storage, &mut txn, page_id, &[b"idempotent"]);
    txn.record_page_writes(1, &[(page_id, &slot_ids)]).unwrap();
    drop(txn); // no commit

    // First recovery
    let (_, r1) = TxnManager::open_with_recovery(&mut storage, &env.wal).unwrap();
    assert_eq!(r1.undone_txns, 1);

    // Second recovery — WAL still has Begin+PageWrite (no Commit), so the scan
    // again finds 1 in-progress txn and re-applies mark_slot_dead.
    // mark_slot_dead is idempotent (AlreadyDeleted → Ok), so no corruption.
    let (_, r2) = TxnManager::open_with_recovery(&mut storage, &env.wal).unwrap();
    assert_eq!(
        r2.undone_txns, 1,
        "WAL still shows 1 in-progress txn on second scan"
    );

    // Slots must still be dead — idempotent undo preserves correctness.
    let raw2 = *storage.read_page(page_id).unwrap().as_bytes();
    let page2 = Page::from_bytes(raw2).unwrap();
    for &slot_id in &slot_ids {
        assert!(
            read_tuple(&page2, slot_id).unwrap().is_none(),
            "slot {slot_id} must still be dead after second recovery"
        );
    }
}
