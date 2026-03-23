//! Integration tests for WalReader (subphase 3.3).
//!
//! Verify forward scan, backward scan, skip by LSN, stopping on corruption/truncation,
//! and that backward is the exact reverse of forward.

use nexusdb_core::error::DbError;
use nexusdb_wal::{EntryType, WalEntry, WalReader, WalWriter};
use tempfile::tempdir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_insert(txn_id: u64, key: &[u8], value: &[u8]) -> WalEntry {
    WalEntry::new(
        0,
        txn_id,
        EntryType::Insert,
        1,
        key.to_vec(),
        vec![],
        value.to_vec(),
    )
}

/// Writes `count` entries to the WAL and commits. Returns the entries with assigned LSNs.
fn write_n(path: &std::path::Path, count: u64) -> Vec<WalEntry> {
    let mut writer = WalWriter::create(path).unwrap();
    let mut result = Vec::new();
    for i in 0..count {
        let key = format!("key:{:04}", i + 1).into_bytes();
        let val = vec![(i + 1) as u8];
        let mut e = make_insert(1, &key, &val);
        writer.append(&mut e).unwrap();
        result.push(e);
    }
    writer.commit().unwrap();
    result
}

// ── Forward — happy path ─────────────────────────────────────────────────────

#[test]
fn test_forward_reads_all_entries_in_order() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    let written = write_n(&path, 20);

    let reader = WalReader::open(&path).unwrap();
    let read: Vec<WalEntry> = reader
        .scan_forward(0)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(read.len(), 20);
    for (i, entry) in read.iter().enumerate() {
        assert_eq!(entry.lsn, (i + 1) as u64, "incorrect LSN at position {i}");
        assert_eq!(entry.key, written[i].key, "incorrect key at position {i}");
    }
}

#[test]
fn test_forward_lsns_strictly_increasing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 15);

    let reader = WalReader::open(&path).unwrap();
    let lsns: Vec<u64> = reader
        .scan_forward(0)
        .unwrap()
        .map(|r| r.unwrap().lsn)
        .collect();

    for w in lsns.windows(2) {
        assert!(
            w[1] > w[0],
            "LSNs are not strictly increasing: {} → {}",
            w[0],
            w[1]
        );
    }
}

#[test]
fn test_forward_single_entry() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 1);

    let reader = WalReader::open(&path).unwrap();
    let read: Vec<WalEntry> = reader
        .scan_forward(0)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(read.len(), 1);
    assert_eq!(read[0].lsn, 1);
}

#[test]
fn test_forward_empty_wal_returns_no_entries() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty.wal");
    WalWriter::create(&path).unwrap();

    let reader = WalReader::open(&path).unwrap();
    let read: Vec<_> = reader.scan_forward(0).unwrap().collect();
    assert!(read.is_empty(), "empty WAL must produce an empty iterator");
}

// ── Forward — from_lsn ──────────────────────────────────────────────────────

#[test]
fn test_forward_from_lsn_skips_earlier_entries() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 10);

    let reader = WalReader::open(&path).unwrap();
    let read: Vec<WalEntry> = reader
        .scan_forward(6)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(read.len(), 5, "5 entries must be returned (LSN 6–10)");
    assert_eq!(read[0].lsn, 6, "first entry must be LSN 6");
    assert_eq!(read[4].lsn, 10, "last entry must be LSN 10");
}

#[test]
fn test_forward_from_lsn_exact_match() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 5);

    let reader = WalReader::open(&path).unwrap();
    let read: Vec<WalEntry> = reader
        .scan_forward(5)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(read.len(), 1);
    assert_eq!(read[0].lsn, 5);
}

#[test]
fn test_forward_from_lsn_beyond_end_returns_empty() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 5);

    let reader = WalReader::open(&path).unwrap();
    let read: Vec<_> = reader.scan_forward(100).unwrap().collect();
    assert!(
        read.is_empty(),
        "from_lsn beyond the end must return an empty iterator"
    );
}

#[test]
fn test_forward_from_lsn_zero_same_as_all() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 8);

    let reader = WalReader::open(&path).unwrap();
    let count_0 = reader.scan_forward(0).unwrap().count();
    let count_1 = reader.scan_forward(1).unwrap().count();
    assert_eq!(
        count_0, count_1,
        "from_lsn=0 and from_lsn=1 must give the same result"
    );
}

// ── Forward — corruption and truncation ──────────────────────────────────────

#[test]
fn test_forward_stops_on_truncated_tail() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 5);

    // Truncate the file — remove the last 10 bytes (half of the last entry)
    let original_size = std::fs::metadata(&path).unwrap().len();
    let truncated_size = original_size - 10;
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(truncated_size).unwrap();

    let reader = WalReader::open(&path).unwrap();
    let results: Vec<Result<WalEntry, DbError>> = reader.scan_forward(0).unwrap().collect();

    // The first 4 entries must be Ok, the 5th must be Err
    let ok_count = results.iter().filter(|r| r.is_ok()).count();
    let err_count = results.iter().filter(|r| r.is_err()).count();
    assert_eq!(
        ok_count, 4,
        "4 valid entries must be read before the truncated one"
    );
    assert_eq!(
        err_count, 1,
        "exactly 1 error must be returned for the truncated entry"
    );

    // After the error, the iterator returns no more items
    let last = results.last().unwrap();
    assert!(last.is_err(), "the last item must be the truncation error");
}

#[test]
fn test_forward_stops_on_crc_corruption() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 5);

    // Corrupt one byte in the payload of entry 3 (approximately in the middle)
    let mut data = std::fs::read(&path).unwrap();
    // Flip one bit somewhere in the data area (not the 16-byte header)
    // Corrupt at an arbitrary position in the second half of the file
    let corrupt_pos = data.len() / 2;
    data[corrupt_pos] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();

    let reader = WalReader::open(&path).unwrap();
    let results: Vec<Result<WalEntry, DbError>> = reader.scan_forward(0).unwrap().collect();

    // There must be at least 1 error (the corrupt entry)
    let has_err = results.iter().any(|r| r.is_err());
    assert!(has_err, "corruption must produce at least one error");

    // The error must be CRC or truncation — not a panic
    let err = results
        .iter()
        .find(|r| r.is_err())
        .unwrap()
        .as_ref()
        .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::WalChecksumMismatch { .. } | DbError::WalEntryTruncated { .. }
        ),
        "error must be WalChecksumMismatch or WalEntryTruncated, got: {:?}",
        err
    );
}

// ── Backward — happy path ────────────────────────────────────────────────────

#[test]
fn test_backward_reads_entries_in_reverse_lsn_order() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 5);

    let reader = WalReader::open(&path).unwrap();
    let lsns: Vec<u64> = reader
        .scan_backward()
        .unwrap()
        .map(|r| r.unwrap().lsn)
        .collect();

    assert_eq!(
        lsns,
        vec![5, 4, 3, 2, 1],
        "backward must return LSNs in decreasing order"
    );
}

#[test]
fn test_backward_empty_wal_returns_no_entries() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty.wal");
    WalWriter::create(&path).unwrap();

    let reader = WalReader::open(&path).unwrap();
    let read: Vec<_> = reader.scan_backward().unwrap().collect();
    assert!(
        read.is_empty(),
        "backward on empty WAL must produce an empty iterator"
    );
}

#[test]
fn test_backward_single_entry() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 1);

    let reader = WalReader::open(&path).unwrap();
    let read: Vec<WalEntry> = reader
        .scan_backward()
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(read.len(), 1);
    assert_eq!(read[0].lsn, 1);
}

#[test]
fn test_backward_matches_forward_reversed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 12);

    let reader = WalReader::open(&path).unwrap();

    let forward: Vec<WalEntry> = reader
        .scan_forward(0)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let backward: Vec<WalEntry> = reader
        .scan_backward()
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(
        forward.len(),
        backward.len(),
        "forward and backward must have the same count"
    );

    for (i, (f, b)) in forward.iter().zip(backward.iter().rev()).enumerate() {
        assert_eq!(
            f.lsn, b.lsn,
            "LSN mismatch at position {i}: forward={}, backward={}",
            f.lsn, b.lsn
        );
        assert_eq!(f.key, b.key, "key mismatch at position {i}");
        assert_eq!(
            f.new_value, b.new_value,
            "new_value mismatch at position {i}"
        );
    }
}

// ── Multiple entry types ──────────────────────────────────────────────────────

#[test]
fn test_forward_mixed_entry_types() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    let mut writer = WalWriter::create(&path).unwrap();

    let mut begin = WalEntry::new(0, 1, EntryType::Begin, 0, vec![], vec![], vec![]);
    let mut insert = make_insert(1, b"k1", b"v1");
    let mut update = WalEntry::new(
        0,
        1,
        EntryType::Update,
        1,
        b"k1".to_vec(),
        b"v1".to_vec(),
        b"v2".to_vec(),
    );
    let mut commit = WalEntry::new(0, 1, EntryType::Commit, 0, vec![], vec![], vec![]);

    writer.append(&mut begin).unwrap();
    writer.append(&mut insert).unwrap();
    writer.append(&mut update).unwrap();
    writer.append(&mut commit).unwrap();
    writer.commit().unwrap();

    let reader = WalReader::open(&path).unwrap();
    let entries: Vec<WalEntry> = reader
        .scan_forward(0)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].entry_type, EntryType::Begin);
    assert_eq!(entries[1].entry_type, EntryType::Insert);
    assert_eq!(entries[2].entry_type, EntryType::Update);
    assert_eq!(entries[3].entry_type, EntryType::Commit);
}

// ── Multiple write sessions ───────────────────────────────────────────────────

#[test]
fn test_forward_across_multiple_write_sessions() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    // Session 1
    {
        let mut w = WalWriter::create(&path).unwrap();
        for i in 0..5u64 {
            let key = format!("s1:key{i}").into_bytes();
            let mut e = make_insert(1, &key, &[i as u8]);
            w.append(&mut e).unwrap();
        }
        w.commit().unwrap();
    }

    // Session 2
    {
        let mut w = WalWriter::open(&path).unwrap();
        for i in 0..5u64 {
            let key = format!("s2:key{i}").into_bytes();
            let mut e = make_insert(2, &key, &[(i + 10) as u8]);
            w.append(&mut e).unwrap();
        }
        w.commit().unwrap();
    }

    let reader = WalReader::open(&path).unwrap();
    let entries: Vec<WalEntry> = reader
        .scan_forward(0)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(entries.len(), 10, "10 entries must be read from 2 sessions");
    assert_eq!(entries[0].lsn, 1);
    assert_eq!(entries[9].lsn, 10);

    // LSN must be continuous (1..=10)
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry.lsn, (i + 1) as u64);
    }
}

// ── Multiple independent scans ────────────────────────────────────────────────

#[test]
fn test_multiple_independent_forward_scans() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 5);

    let reader = WalReader::open(&path).unwrap();

    // Two independent scans must give the same result
    let scan1: Vec<u64> = reader
        .scan_forward(0)
        .unwrap()
        .map(|r| r.unwrap().lsn)
        .collect();
    let scan2: Vec<u64> = reader
        .scan_forward(0)
        .unwrap()
        .map(|r| r.unwrap().lsn)
        .collect();

    assert_eq!(scan1, scan2, "multiple forward scans must be idempotent");
}

#[test]
fn test_forward_and_backward_independent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    write_n(&path, 5);

    let reader = WalReader::open(&path).unwrap();

    // Create both iterators — they must be independent
    let mut fwd = reader.scan_forward(0).unwrap();
    let mut bwd = reader.scan_backward().unwrap();

    let first_fwd = fwd.next().unwrap().unwrap().lsn;
    let first_bwd = bwd.next().unwrap().unwrap().lsn;

    assert_eq!(first_fwd, 1, "forward must start at LSN 1");
    assert_eq!(first_bwd, 5, "backward must start at LSN 5");
}
