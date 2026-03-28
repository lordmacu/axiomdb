//! Integration tests for the WalWriter (subphase 3.2).
//!
//! Verify durability, LSN, header, reopen, and simulated crash behavior.

use std::fs;

use axiomdb_core::error::DbError;
use axiomdb_wal::{EntryType, WalEntry, WalWriter, WAL_HEADER_SIZE, WAL_MAGIC, WAL_VERSION};
use tempfile::tempdir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn begin(txn_id: u64) -> WalEntry {
    WalEntry::new(0, txn_id, EntryType::Begin, 0, vec![], vec![], vec![])
}

fn insert(txn_id: u64, key: &[u8]) -> WalEntry {
    WalEntry::new(
        0,
        txn_id,
        EntryType::Insert,
        1,
        key.to_vec(),
        vec![],
        rid(1, 0),
    )
}

fn commit_entry(txn_id: u64) -> WalEntry {
    WalEntry::new(0, txn_id, EntryType::Commit, 0, vec![], vec![], vec![])
}

fn rid(page: u64, slot: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(10);
    b.extend_from_slice(&page.to_le_bytes());
    b.extend_from_slice(&slot.to_le_bytes());
    b
}

// ── Header ────────────────────────────────────────────────────────────────────

#[test]
fn test_create_writes_correct_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    WalWriter::create(&path).unwrap();

    let data = fs::read(&path).unwrap();
    assert!(
        data.len() >= WAL_HEADER_SIZE,
        "newly created WAL must contain the header inside the reserved region"
    );

    let magic = u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    let version = u16::from_le_bytes([data[8], data[9]]);

    assert_eq!(magic, WAL_MAGIC, "wrong magic");
    assert_eq!(version, WAL_VERSION, "wrong version");

    // Reserved bytes must be zero
    assert_eq!(&data[10..16], &[0u8; 6], "reserved bytes must be zero");
}

#[test]
fn test_open_rejects_invalid_magic() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    // Create file with incorrect magic
    let mut bad_header = [0u8; WAL_HEADER_SIZE];
    bad_header[0..8].copy_from_slice(&0xDEAD_BEEF_CAFE_1234u64.to_le_bytes());
    bad_header[8..10].copy_from_slice(&WAL_VERSION.to_le_bytes());
    fs::write(&path, bad_header).unwrap();

    assert!(
        matches!(
            WalWriter::open(&path),
            Err(DbError::WalInvalidHeader { .. })
        ),
        "incorrect magic should return WalInvalidHeader"
    );
}

#[test]
fn test_open_rejects_unknown_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    let mut bad_header = [0u8; WAL_HEADER_SIZE];
    bad_header[0..8].copy_from_slice(&WAL_MAGIC.to_le_bytes());
    bad_header[8..10].copy_from_slice(&999u16.to_le_bytes()); // unknown version
    fs::write(&path, bad_header).unwrap();

    assert!(
        matches!(
            WalWriter::open(&path),
            Err(DbError::WalInvalidHeader { .. })
        ),
        "unknown version should return WalInvalidHeader"
    );
}

#[test]
fn test_create_fails_if_file_exists() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    WalWriter::create(&path).unwrap();

    // Second create on the same file must fail
    assert!(
        WalWriter::create(&path).is_err(),
        "create() on an existing file must fail"
    );
}

// ── Durability ────────────────────────────────────────────────────────────────

#[test]
fn test_append_without_commit_not_durable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    {
        let mut w = WalWriter::create(&path).unwrap();
        let mut e = insert(1, b"key:001");
        w.append(&mut e).unwrap();
        // Drop without commit — BufWriter flushes on drop but does NOT fsync.
        // In a real crash the entries would be lost. Here we verify that
        // without commit the on-disk file may not contain the entries.
        // Note: in tests (no real crash) the Drop flush may write them,
        // but the system guarantee is that only commit() ensures durability.
    }

    // Reopen: even though the Drop flush may have written to the OS, only
    // commit() guarantees durability. We verify that open() works
    // regardless of the state.
    let w2 = WalWriter::open(&path).unwrap();
    // What matters: open() does not panic or corrupt the WAL
    let _ = w2.current_lsn();
}

#[test]
fn test_append_commit_durable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    {
        let mut w = WalWriter::create(&path).unwrap();

        let mut b = begin(1);
        w.append(&mut b).unwrap();

        let mut ins = insert(1, b"product:001");
        w.append(&mut ins).unwrap();

        let mut c = commit_entry(1);
        w.append(&mut c).unwrap();

        w.commit().unwrap(); // fsync — durable
    }

    // The file must contain header + 3 entries
    let data = fs::read(&path).unwrap();
    assert!(
        data.len() > WAL_HEADER_SIZE,
        "file must contain entries after commit"
    );

    // Reopen and verify that entries are readable
    let w2 = WalWriter::open(&path).unwrap();
    assert_eq!(
        w2.current_lsn(),
        3,
        "3 entries should have been persisted (LSN 1,2,3)"
    );
}

// ── LSN ───────────────────────────────────────────────────────────────────────

#[test]
fn test_open_continues_lsn() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    // Session 1: write 3 entries
    {
        let mut w = WalWriter::create(&path).unwrap();
        for _ in 0..3 {
            let mut e = begin(1);
            w.append(&mut e).unwrap();
        }
        w.commit().unwrap();
    }

    // Session 2: next LSN must be 4
    {
        let mut w = WalWriter::open(&path).unwrap();
        assert_eq!(
            w.current_lsn(),
            3,
            "last LSN from previous session must be 3"
        );

        let mut e = begin(2);
        let lsn = w.append(&mut e).unwrap();
        assert_eq!(lsn, 4, "first LSN of new session must be 4");
        w.commit().unwrap();
    }

    // Session 3: verify continuity
    let w = WalWriter::open(&path).unwrap();
    assert_eq!(w.current_lsn(), 4);
}

#[test]
fn test_lsn_monotonic_across_many_appends() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    let mut w = WalWriter::create(&path).unwrap();

    let mut prev_lsn = 0u64;
    for i in 0..100u64 {
        let key = format!("key:{:04}", i);
        let mut e = insert(1, key.as_bytes());
        let lsn = w.append(&mut e).unwrap();
        assert!(lsn > prev_lsn, "LSN must be strictly increasing");
        prev_lsn = lsn;
    }
    assert_eq!(w.current_lsn(), 100);
}

// ── file_offset ───────────────────────────────────────────────────────────────

#[test]
fn test_file_offset_grows_with_each_append() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    let mut w = WalWriter::create(&path).unwrap();

    let mut offsets = vec![w.file_offset()];

    for i in 0..5u64 {
        let key = format!("k{}", i);
        let mut e = insert(1, key.as_bytes());
        w.append(&mut e).unwrap();
        offsets.push(w.file_offset());
    }

    for i in 0..offsets.len() - 1 {
        assert!(
            offsets[i + 1] > offsets[i],
            "file_offset must grow: {} -> {}",
            offsets[i],
            offsets[i + 1]
        );
    }
}

// ── Multiple commits ──────────────────────────────────────────────────────────

#[test]
fn test_multiple_commits_all_entries_durable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    {
        let mut w = WalWriter::create(&path).unwrap();

        // Transaction 1
        let mut b1 = begin(1);
        w.append(&mut b1).unwrap();
        let mut ins1 = insert(1, b"a");
        w.append(&mut ins1).unwrap();
        let mut c1 = commit_entry(1);
        w.append(&mut c1).unwrap();
        w.commit().unwrap();

        // Transaction 2
        let mut b2 = begin(2);
        w.append(&mut b2).unwrap();
        let mut ins2 = insert(2, b"b");
        w.append(&mut ins2).unwrap();
        let mut c2 = commit_entry(2);
        w.append(&mut c2).unwrap();
        w.commit().unwrap();
    }

    // 6 entries in total
    let w = WalWriter::open(&path).unwrap();
    assert_eq!(
        w.current_lsn(),
        6,
        "6 entries should have been written across 2 transactions"
    );
}

// ── Empty WAL ─────────────────────────────────────────────────────────────────

#[test]
fn test_open_empty_wal_lsn_starts_at_1() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    WalWriter::create(&path).unwrap(); // header only, no entries

    let mut w = WalWriter::open(&path).unwrap();
    assert_eq!(w.current_lsn(), 0, "empty WAL must have current_lsn == 0");

    let mut e = begin(1);
    let lsn = w.append(&mut e).unwrap();
    assert_eq!(lsn, 1, "first entry in empty WAL must have LSN 1");
}
