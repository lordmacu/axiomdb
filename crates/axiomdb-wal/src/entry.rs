//! WAL Entry — binary format of each Write-Ahead Log record.
//!
//! ## On-disk layout
//!
//! ```text
//! Offset    Size    Field
//!      0         4  entry_len      u32 LE — total entry length
//!      4         8  lsn            u64 LE — Log Sequence Number, globally monotonic
//!     12         8  txn_id         u64 LE — Transaction ID (0 = autocommit)
//!     20         1  entry_type     u8     — EntryType
//!     21         4  table_id       u32 LE — table identifier (0 = system)
//!     25         2  key_len        u16 LE — key length in bytes
//!     27  key_len   key            [u8]   — key bytes
//!      ?         4  old_val_len    u32 LE — old value length (0 on INSERT)
//!      ?  old_len   old_value      [u8]   — old value (empty on INSERT)
//!      ?         4  new_val_len    u32 LE — new value length (0 on DELETE)
//!      ?  new_len   new_value      [u8]   — new value (empty on DELETE)
//!      ?         4  crc32c         u32 LE — CRC32c of all preceding bytes
//!      ?         4  entry_len_2    u32 LE — copy of entry_len for backward scan
//! ```
//!
//! **Minimum size** (BEGIN/COMMIT/ROLLBACK without key or values): 43 bytes.
//!
//! ## Backward scan
//!
//! To traverse the WAL backward (ROLLBACK, crash recovery):
//! ```text
//! entry_start_pos = entry_end_pos - entry_len_2
//! ```
//! where `entry_len_2` are the last 4 bytes of the entry.

use axiomdb_core::error::DbError;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Size of the fixed header before variable fields.
/// 4 (entry_len) + 8 (lsn) + 8 (txn_id) + 1 (entry_type) + 4 (table_id) + 2 (key_len) = 27
const FIXED_HEADER: usize = 27;

/// Size of the fixed trailer after variable fields.
/// 4 (old_val_len) + 4 (new_val_len) + 4 (crc32c) + 4 (entry_len_2) = 16
/// — plus variable payloads in between.
/// Total fixed overhead (without payloads):
/// FIXED_HEADER + 4 (old_val_len) + 4 (new_val_len) + 4 (crc32c) + 4 (entry_len_2) = 43
pub const MIN_ENTRY_LEN: usize = 43;

// ── EntryType ─────────────────────────────────────────────────────────────────

/// Type of operation recorded in the WAL.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    /// Start of an explicit transaction (`BEGIN`).
    Begin = 1,
    /// Transaction commit (`COMMIT`).
    Commit = 2,
    /// Transaction rollback (`ROLLBACK`).
    Rollback = 3,
    /// Insertion of a new key→value pair. `old_value` is empty.
    Insert = 4,
    /// Deletion of a key→value pair. `new_value` is empty, `old_value` = value before deletion.
    Delete = 5,
    /// Modification of a key→value pair. Both `old_value` and `new_value` are present.
    Update = 6,
    /// Checkpoint point — marks how far data is on disk. No payload.
    Checkpoint = 7,
    /// Full-table delete (DELETE without WHERE or TRUNCATE TABLE).
    /// `key` contains `root_page_id` as 8 bytes LE.
    /// No per-row payload — undo by scanning the heap chain.
    Truncate = 8,
    /// Bulk-insert page image — replaces N `Insert` entries with one entry per page.
    ///
    /// `key`       = `page_id` as u64 LE (8 bytes).
    /// `old_value` = empty.
    /// `new_value` = `[page_bytes: PAGE_SIZE][num_slots: u16 LE][slot_id × N: u16 LE]`.
    ///
    /// Crash recovery undoes an uncommitted `PageWrite` by marking each embedded
    /// `slot_id` dead on the given page — identical to undoing N `Insert` entries.
    /// The `page_bytes` are preserved for future REDO support (Phase 3.8b).
    PageWrite = 9,
    /// Stable-RID in-place update.
    ///
    /// `old_value` = `[page_id:8][slot_id:2][old tuple image...]`
    /// `new_value` = `[page_id:8][slot_id:2][new tuple image...]`
    ///
    /// Undo restores the exact old tuple image at the same physical location.
    UpdateInPlace = 10,
}

impl TryFrom<u8> for EntryType {
    type Error = DbError;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            1 => Ok(Self::Begin),
            2 => Ok(Self::Commit),
            3 => Ok(Self::Rollback),
            4 => Ok(Self::Insert),
            5 => Ok(Self::Delete),
            6 => Ok(Self::Update),
            7 => Ok(Self::Checkpoint),
            8 => Ok(Self::Truncate),
            9 => Ok(Self::PageWrite),
            10 => Ok(Self::UpdateInPlace),
            _ => Err(DbError::WalUnknownEntryType { byte }),
        }
    }
}

// ── WalEntry ──────────────────────────────────────────────────────────────────

/// Logical Write-Ahead Log record.
///
/// Represents a semantic operation (INSERT, DELETE, UPDATE, transaction control).
/// Serialization to bytes is done with [`WalEntry::to_bytes`]; deserialization
/// with [`WalEntry::from_bytes`].
///
/// The `entry_len` and `crc32c` fields are not stored in memory — they are
/// calculated automatically on serialization and verified on deserialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalEntry {
    /// Log Sequence Number — globally monotonic number. Assigned by the WalWriter.
    pub lsn: u64,
    /// Transaction identifier. `0` = autocommit (no explicit transaction).
    pub txn_id: u64,
    /// Operation type.
    pub entry_type: EntryType,
    /// Identifier of the affected table. `0` = system/meta.
    pub table_id: u32,
    /// Operation key. Empty for control entries (Begin, Commit, Rollback, Checkpoint).
    pub key: Vec<u8>,
    /// Value before the change. Empty on INSERT and control entries.
    pub old_value: Vec<u8>,
    /// Value after the change. Empty on DELETE and control entries.
    pub new_value: Vec<u8>,
}

impl WalEntry {
    /// Creates a new `WalEntry`.
    pub fn new(
        lsn: u64,
        txn_id: u64,
        entry_type: EntryType,
        table_id: u32,
        key: Vec<u8>,
        old_value: Vec<u8>,
        new_value: Vec<u8>,
    ) -> Self {
        Self {
            lsn,
            txn_id,
            entry_type,
            table_id,
            key,
            old_value,
            new_value,
        }
    }

    /// Calculates the total serialized size in bytes, without allocating.
    ///
    /// Useful for the WalWriter to pre-allocate the exact buffer.
    pub fn serialized_len(&self) -> usize {
        MIN_ENTRY_LEN + self.key.len() + self.old_value.len() + self.new_value.len()
    }

    /// Serializes this entry **directly into** an existing `Vec<u8>` without
    /// creating an intermediate allocation.
    ///
    /// This is the batch-write counterpart of [`to_bytes`]. Call this in a
    /// loop for all entries in a transaction, then write the accumulated buffer
    /// to the WAL file in a single `write_all()`. This follows the same
    /// strategy LMDB uses: accumulate all dirty-page writes, then flush once.
    ///
    /// The `lsn` argument must be the LSN already assigned to this entry.
    pub fn serialize_into(&self, buf: &mut Vec<u8>) {
        let start = buf.len();
        let total = self.serialized_len();
        buf.reserve(total);

        // ── Fixed header ──────────────────────────────────────────────────────
        buf.extend_from_slice(&(total as u32).to_le_bytes()); // entry_len
        buf.extend_from_slice(&self.lsn.to_le_bytes()); // lsn
        buf.extend_from_slice(&self.txn_id.to_le_bytes()); // txn_id
        buf.push(self.entry_type as u8); // entry_type
        buf.extend_from_slice(&self.table_id.to_le_bytes()); // table_id
        buf.extend_from_slice(&(self.key.len() as u16).to_le_bytes()); // key_len

        // ── Variable payload ──────────────────────────────────────────────────
        buf.extend_from_slice(&self.key);
        buf.extend_from_slice(&(self.old_value.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.old_value);
        buf.extend_from_slice(&(self.new_value.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.new_value);

        // ── CRC32c over all bytes written in this entry ───────────────────────
        let crc = crc32c::crc32c(&buf[start..]);
        buf.extend_from_slice(&crc.to_le_bytes());

        // ── Trailer for backward scan ─────────────────────────────────────────
        buf.extend_from_slice(&(total as u32).to_le_bytes()); // entry_len_2

        debug_assert_eq!(
            buf.len() - start,
            total,
            "serialize_into: serialized_len() mismatch"
        );
    }

    /// Serializes the entry to binary format ready to write to the WAL file.
    ///
    /// The result includes `entry_len`, all fields, `crc32c`, and `entry_len_2`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let total = self.serialized_len();
        let mut buf = Vec::with_capacity(total);

        let total_u32 = total as u32;

        // ── Fixed header ─────────────────────────────────────────
        buf.extend_from_slice(&total_u32.to_le_bytes()); // entry_len
        buf.extend_from_slice(&self.lsn.to_le_bytes()); // lsn
        buf.extend_from_slice(&self.txn_id.to_le_bytes()); // txn_id
        buf.push(self.entry_type as u8); // entry_type
        buf.extend_from_slice(&self.table_id.to_le_bytes()); // table_id
        buf.extend_from_slice(&(self.key.len() as u16).to_le_bytes()); // key_len

        // ── Variable payload ─────────────────────────────────────
        buf.extend_from_slice(&self.key); // key

        buf.extend_from_slice(&(self.old_value.len() as u32).to_le_bytes()); // old_val_len
        buf.extend_from_slice(&self.old_value); // old_value

        buf.extend_from_slice(&(self.new_value.len() as u32).to_le_bytes()); // new_val_len
        buf.extend_from_slice(&self.new_value); // new_value

        // ── CRC32c (covers everything above) ─────────────────────
        let crc = crc32c::crc32c(&buf);
        buf.extend_from_slice(&crc.to_le_bytes()); // crc32c

        // ── Trailer for backward scan ────────────────────────────
        buf.extend_from_slice(&total_u32.to_le_bytes()); // entry_len_2

        debug_assert_eq!(
            buf.len(),
            total,
            "serialized_len() does not match to_bytes().len()"
        );
        buf
    }

    /// Deserializes a `WalEntry` from a byte slice.
    ///
    /// Returns `(entry, bytes_consumed)`. The caller can loop, incrementing
    /// the offset, to parse chained entries.
    ///
    /// # Errors
    /// - [`DbError::WalEntryTruncated`] — buffer is shorter than the entry
    /// - [`DbError::WalChecksumMismatch`] — CRC32c does not match
    /// - [`DbError::WalUnknownEntryType`] — unknown entry type
    pub fn from_bytes(buf: &[u8]) -> Result<(Self, usize), DbError> {
        // We need at least 4 bytes to read entry_len
        if buf.len() < 4 {
            return Err(DbError::WalEntryTruncated { lsn: 0 });
        }

        let entry_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

        if buf.len() < entry_len {
            return Err(DbError::WalEntryTruncated { lsn: 0 });
        }

        // We need at least MIN_ENTRY_LEN bytes for a valid entry
        if entry_len < MIN_ENTRY_LEN {
            return Err(DbError::WalEntryTruncated { lsn: 0 });
        }

        // ── Read fixed header ────────────────────────────────────
        let lsn = u64::from_le_bytes([
            buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
        ]);
        let txn_id = u64::from_le_bytes([
            buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
        ]);
        let entry_type = EntryType::try_from(buf[20])?;
        let table_id = u32::from_le_bytes([buf[21], buf[22], buf[23], buf[24]]);
        let key_len = u16::from_le_bytes([buf[25], buf[26]]) as usize;

        let mut pos = FIXED_HEADER;

        // ── Read variable payload ────────────────────────────────
        if pos + key_len > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let key = buf[pos..pos + key_len].to_vec();
        pos += key_len;

        if pos + 4 > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let old_val_len =
            u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4;

        if pos + old_val_len > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let old_value = buf[pos..pos + old_val_len].to_vec();
        pos += old_val_len;

        if pos + 4 > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let new_val_len =
            u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4;

        if pos + new_val_len > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let new_value = buf[pos..pos + new_val_len].to_vec();
        pos += new_val_len;

        // ── Verify CRC32c ────────────────────────────────────────
        if pos + 4 > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let stored_crc = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        let computed_crc = crc32c::crc32c(&buf[..pos]);
        if stored_crc != computed_crc {
            return Err(DbError::WalChecksumMismatch {
                lsn,
                expected: stored_crc,
                got: computed_crc,
            });
        }
        pos += 4;

        // ── Verify entry_len_2 (backward scan) ───────────────────
        if pos + 4 > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let entry_len_2 =
            u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        if entry_len_2 != entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }

        Ok((
            Self {
                lsn,
                txn_id,
                entry_type,
                table_id,
                key,
                old_value,
                new_value,
            },
            entry_len,
        ))
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_insert(lsn: u64) -> WalEntry {
        WalEntry::new(
            lsn,
            42,
            EntryType::Insert,
            1,
            b"user:001".to_vec(),
            vec![],
            vec![5u8, 0, 0, 0, 0, 0, 0, 0, 3, 0], // simulated RecordId (10 bytes)
        )
    }

    #[test]
    fn test_entry_type_roundtrip() {
        for byte in [1u8, 2, 3, 4, 5, 6, 7, 8] {
            let et = EntryType::try_from(byte).unwrap();
            assert_eq!(et as u8, byte);
        }
    }

    #[test]
    fn test_entry_type_unknown() {
        assert!(matches!(
            EntryType::try_from(0u8),
            Err(DbError::WalUnknownEntryType { byte: 0 })
        ));
        assert!(matches!(
            EntryType::try_from(255u8),
            Err(DbError::WalUnknownEntryType { byte: 255 })
        ));
    }

    #[test]
    fn test_serialized_len_matches_to_bytes() {
        let entry = make_insert(1);
        assert_eq!(entry.serialized_len(), entry.to_bytes().len());
    }

    #[test]
    fn test_min_entry_len_begin() {
        let begin = WalEntry::new(1, 0, EntryType::Begin, 0, vec![], vec![], vec![]);
        assert_eq!(begin.to_bytes().len(), MIN_ENTRY_LEN);
        assert_eq!(begin.serialized_len(), MIN_ENTRY_LEN);
    }

    #[test]
    fn test_entry_len_repeated_at_end() {
        let entry = make_insert(5);
        let bytes = entry.to_bytes();
        let len = bytes.len();
        // First 4 bytes == last 4 bytes
        let front = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let back = u32::from_le_bytes([
            bytes[len - 4],
            bytes[len - 3],
            bytes[len - 2],
            bytes[len - 1],
        ]);
        assert_eq!(
            front, back,
            "entry_len at the start must match entry_len_2 at the end"
        );
    }

    #[test]
    fn test_roundtrip_insert() {
        let entry = make_insert(100);
        let bytes = entry.to_bytes();
        let (parsed, consumed) = WalEntry::from_bytes(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_roundtrip_begin() {
        let entry = WalEntry::new(1, 7, EntryType::Begin, 0, vec![], vec![], vec![]);
        let bytes = entry.to_bytes();
        let (parsed, consumed) = WalEntry::from_bytes(&bytes).unwrap();
        assert_eq!(consumed, MIN_ENTRY_LEN);
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_crc_corruption_detected() {
        let entry = make_insert(10);
        let mut bytes = entry.to_bytes();
        // Flip one bit in the payload (position 30 = inside key)
        bytes[30] ^= 0xFF;
        assert!(matches!(
            WalEntry::from_bytes(&bytes),
            Err(DbError::WalChecksumMismatch { .. })
        ));
    }

    #[test]
    fn test_truncated_buffer() {
        let entry = make_insert(20);
        let bytes = entry.to_bytes();
        // Buffer shorter than entry_len
        let truncated = &bytes[..bytes.len() - 1];
        assert!(matches!(
            WalEntry::from_bytes(truncated),
            Err(DbError::WalEntryTruncated { .. })
        ));
    }

    #[test]
    fn test_empty_buffer() {
        assert!(matches!(
            WalEntry::from_bytes(&[]),
            Err(DbError::WalEntryTruncated { lsn: 0 })
        ));
    }
}
