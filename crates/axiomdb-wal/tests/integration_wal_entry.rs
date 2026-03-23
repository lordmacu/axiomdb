//! Integration tests for the WAL entry format (subphase 3.1).
//!
//! Verify end-to-end correctness: serialization, deserialization, corruption
//! detection, chained entries, and backward scan.

use axiomdb_core::error::DbError;
use axiomdb_wal::{EntryType, WalEntry, MIN_ENTRY_LEN};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_entry(lsn: u64, et: EntryType, key: &[u8], old: &[u8], new: &[u8]) -> WalEntry {
    WalEntry::new(lsn, 1, et, 1, key.to_vec(), old.to_vec(), new.to_vec())
}

fn rid_bytes(page: u64, slot: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(10);
    b.extend_from_slice(&page.to_le_bytes());
    b.extend_from_slice(&slot.to_le_bytes());
    b
}

// ── Roundtrip — all 7 types ───────────────────────────────────────────────────

#[test]
fn test_roundtrip_all_entry_types() {
    let rid = rid_bytes(5, 3);

    let entries = vec![
        make_entry(1, EntryType::Begin, &[], &[], &[]),
        make_entry(2, EntryType::Commit, &[], &[], &[]),
        make_entry(3, EntryType::Rollback, &[], &[], &[]),
        make_entry(4, EntryType::Insert, b"key:001", &[], &rid),
        make_entry(5, EntryType::Delete, b"key:001", &rid, &[]),
        make_entry(6, EntryType::Update, b"key:001", &rid, &rid_bytes(6, 0)),
        make_entry(7, EntryType::Checkpoint, &[], &[], &[]),
    ];

    for original in &entries {
        let bytes = original.to_bytes();
        let (parsed, consumed) = WalEntry::from_bytes(&bytes)
            .unwrap_or_else(|e| panic!("failed on {:?}: {e}", original.entry_type));

        assert_eq!(
            consumed,
            bytes.len(),
            "bytes_consumed != bytes.len() in {:?}",
            original.entry_type
        );
        assert_eq!(
            &parsed, original,
            "roundtrip failed in {:?}",
            original.entry_type
        );
    }
}

#[test]
fn test_control_entries_have_min_len() {
    for et in [
        EntryType::Begin,
        EntryType::Commit,
        EntryType::Rollback,
        EntryType::Checkpoint,
    ] {
        let entry = make_entry(1, et, &[], &[], &[]);
        assert_eq!(
            entry.to_bytes().len(),
            MIN_ENTRY_LEN,
            "entry {:?} should be exactly {MIN_ENTRY_LEN} bytes",
            et
        );
    }
}

// ── Corruption detection ──────────────────────────────────────────────────────

/// Flip 1 bit at different positions in the buffer — all must be detected.
#[test]
fn test_crc_detects_single_bit_flip() {
    let entry = make_entry(10, EntryType::Insert, b"order:0042", &[], &rid_bytes(8, 1));
    let bytes = entry.to_bytes();
    let len = bytes.len();

    // Positions to corrupt: inside the header, key, and new_value
    // We exclude the last 8 bytes (crc32c + entry_len_2) because modifying them
    // changes the stored crc or the backward len — both errors are equally valid
    let positions_to_test = [4, 12, 20, 21, 27, len - 9];

    for pos in positions_to_test {
        let mut corrupted = bytes.clone();
        corrupted[pos] ^= 0x01;
        let result = WalEntry::from_bytes(&corrupted);
        assert!(
            result.is_err(),
            "corruption at position {pos} was not detected"
        );
    }
}

#[test]
fn test_corruption_in_key() {
    let entry = make_entry(11, EntryType::Delete, b"product:99", &rid_bytes(2, 0), &[]);
    let mut bytes = entry.to_bytes();
    // Corrupt a byte inside the key area (offset 27)
    bytes[27] ^= 0xFF;
    assert!(matches!(
        WalEntry::from_bytes(&bytes),
        Err(DbError::WalChecksumMismatch { .. })
    ));
}

#[test]
fn test_corruption_in_old_value() {
    let entry = make_entry(
        12,
        EntryType::Update,
        b"k",
        &rid_bytes(1, 0),
        &rid_bytes(2, 0),
    );
    let mut bytes = entry.to_bytes();
    // old_value starts at offset 27 + 1(key) + 4(old_val_len) = 32
    bytes[32] ^= 0x01;
    assert!(matches!(
        WalEntry::from_bytes(&bytes),
        Err(DbError::WalChecksumMismatch { .. })
    ));
}

#[test]
fn test_corruption_in_lsn() {
    let entry = make_entry(99, EntryType::Commit, &[], &[], &[]);
    let mut bytes = entry.to_bytes();
    // LSN starts at offset 4
    bytes[4] ^= 0xFF;
    assert!(matches!(
        WalEntry::from_bytes(&bytes),
        Err(DbError::WalChecksumMismatch { .. })
    ));
}

// ── Truncated buffers ─────────────────────────────────────────────────────────

#[test]
fn test_empty_buffer_is_truncated() {
    assert!(matches!(
        WalEntry::from_bytes(&[]),
        Err(DbError::WalEntryTruncated { lsn: 0 })
    ));
}

#[test]
fn test_three_bytes_is_truncated() {
    assert!(matches!(
        WalEntry::from_bytes(&[0x2B, 0x00, 0x00]),
        Err(DbError::WalEntryTruncated { lsn: 0 })
    ));
}

#[test]
fn test_buffer_one_byte_short() {
    let entry = make_entry(20, EntryType::Insert, b"k", &[], &rid_bytes(1, 0));
    let bytes = entry.to_bytes();
    let truncated = &bytes[..bytes.len() - 1];
    assert!(matches!(
        WalEntry::from_bytes(truncated),
        Err(DbError::WalEntryTruncated { .. })
    ));
}

// ── Chained entries ───────────────────────────────────────────────────────────

#[test]
fn test_chain_of_100_entries() {
    let mut buf: Vec<u8> = Vec::new();
    let count = 100usize;

    // Serialize: BEGIN + 98 INSERTs + COMMIT
    let begin = make_entry(0, EntryType::Begin, &[], &[], &[]);
    buf.extend_from_slice(&begin.to_bytes());

    for i in 1u64..99 {
        let key = format!("{:08}", i);
        let entry = make_entry(i, EntryType::Insert, key.as_bytes(), &[], &rid_bytes(i, 0));
        buf.extend_from_slice(&entry.to_bytes());
    }

    let commit = make_entry(99, EntryType::Commit, &[], &[], &[]);
    buf.extend_from_slice(&commit.to_bytes());

    // Deserialize in a loop
    let mut pos = 0usize;
    let mut parsed_count = 0usize;
    let mut last_lsn = 0u64;

    while pos < buf.len() {
        let (entry, consumed) = WalEntry::from_bytes(&buf[pos..])
            .unwrap_or_else(|e| panic!("failed at pos {pos}: {e}"));

        assert!(
            entry.lsn >= last_lsn,
            "non-monotonic LSN: {} < {}",
            entry.lsn,
            last_lsn
        );
        last_lsn = entry.lsn;

        pos += consumed;
        parsed_count += 1;
    }

    assert_eq!(
        parsed_count, count,
        "expected {count} entries, parsed {parsed_count}"
    );
    assert_eq!(pos, buf.len(), "not all bytes were consumed");
}

// ── Backward scan ────────────────────────────────────────────────────────────

#[test]
fn test_backward_scan_offset() {
    let entries = vec![
        make_entry(1, EntryType::Begin, &[], &[], &[]),
        make_entry(2, EntryType::Insert, b"key:001", &[], &rid_bytes(5, 0)),
        make_entry(3, EntryType::Insert, b"key:002", &[], &rid_bytes(6, 0)),
        make_entry(4, EntryType::Commit, &[], &[], &[]),
    ];

    // Serialize all into a contiguous buffer and save start offsets
    let mut buf: Vec<u8> = Vec::new();
    let mut start_offsets: Vec<usize> = Vec::new();

    for entry in &entries {
        start_offsets.push(buf.len());
        buf.extend_from_slice(&entry.to_bytes());
    }

    let total = buf.len();

    // Traverse backward using entry_len_2 (last 4 bytes of each entry)
    let mut pos = total;
    let mut backward_lsns: Vec<u64> = Vec::new();

    while pos > 0 {
        // Read entry_len_2 (last 4 bytes of the current entry)
        let len_pos = pos - 4;
        let entry_len = u32::from_le_bytes([
            buf[len_pos],
            buf[len_pos + 1],
            buf[len_pos + 2],
            buf[len_pos + 3],
        ]) as usize;

        let entry_start = pos - entry_len;
        let (entry, _) = WalEntry::from_bytes(&buf[entry_start..pos]).unwrap();
        backward_lsns.push(entry.lsn);
        pos = entry_start;
    }

    // LSNs in reverse order must be [4, 3, 2, 1]
    let forward_lsns: Vec<u64> = entries.iter().map(|e| e.lsn).collect();
    let expected_backward: Vec<u64> = forward_lsns.iter().rev().copied().collect();
    assert_eq!(
        backward_lsns, expected_backward,
        "backward scan did not produce LSNs in reverse order"
    );
}

// ── serialized_len ───────────────────────────────────────────────────────────

#[test]
fn test_serialized_len_equals_to_bytes_len_all_types() {
    let rid = rid_bytes(1, 0);
    let cases = vec![
        make_entry(1, EntryType::Begin, &[], &[], &[]),
        make_entry(2, EntryType::Insert, b"longkey_test", &[], &rid),
        make_entry(3, EntryType::Delete, b"k", &rid, &[]),
        make_entry(4, EntryType::Update, b"abc", &rid, &rid_bytes(2, 1)),
        make_entry(5, EntryType::Checkpoint, &[], &[], &[]),
    ];
    for entry in &cases {
        assert_eq!(
            entry.serialized_len(),
            entry.to_bytes().len(),
            "serialized_len() != to_bytes().len() in {:?}",
            entry.entry_type
        );
    }
}

// ── Fields correctly parsed ───────────────────────────────────────────────────

#[test]
fn test_fields_preserved_after_roundtrip() {
    let entry = WalEntry::new(
        0xDEAD_BEEF_CAFE_1234,
        0x1111_2222_3333_4444,
        EntryType::Update,
        0xABCD_EF01,
        b"namespace:key:version:3".to_vec(),
        rid_bytes(100, 5),
        rid_bytes(101, 7),
    );

    let (parsed, _) = WalEntry::from_bytes(&entry.to_bytes()).unwrap();

    assert_eq!(parsed.lsn, 0xDEAD_BEEF_CAFE_1234);
    assert_eq!(parsed.txn_id, 0x1111_2222_3333_4444);
    assert_eq!(parsed.entry_type, EntryType::Update);
    assert_eq!(parsed.table_id, 0xABCD_EF01);
    assert_eq!(parsed.key, b"namespace:key:version:3");
    assert_eq!(parsed.old_value, rid_bytes(100, 5));
    assert_eq!(parsed.new_value, rid_bytes(101, 7));
}
