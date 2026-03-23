//! WalWriter — append-only writes to the global WAL file.
//!
//! ## Guarantees
//!
//! - **Durability**: only entries followed by [`WalWriter::commit`] are on disk.
//!   Entries written with [`WalWriter::append`] without a subsequent commit are lost on crash.
//! - **Monotonic LSN**: the WalWriter is the sole owner of the LSN counter.
//!   No caller can assign duplicate or out-of-order LSNs.
//! - **Integrity**: the 24-byte header (v2) allows detecting invalid files
//!   and recovering the base LSN after WAL rotation.
//!
//! ## WAL file header (v2, 24 bytes)
//!
//! ```text
//! [0..8]   magic: u64      "AXIOMWAL\0"
//! [8..10]  version: u16    2
//! [10..14] _reserved: [u8; 4]
//! [14..22] start_lsn: u64  0 for a fresh WAL; checkpoint_lsn for a rotated WAL
//! ```
//!
//! `start_lsn` ensures that after WAL rotation (where the file is truncated
//! to just the header), `WalWriter::open()` can compute the correct `next_lsn`
//! even when there are no entries in the file.
//!
//! ## Typical usage
//!
//! ```rust,ignore
//! let mut w = WalWriter::create(path)?;
//!
//! // Transaction entries (buffered in RAM via BufWriter, no fsync yet)
//! let mut begin = WalEntry::new(0, txn_id, EntryType::Begin, 0, vec![], vec![], vec![]);
//! w.append(&mut begin)?;
//!
//! let mut insert = WalEntry::new(0, txn_id, EntryType::Insert, table_id, key, vec![], value);
//! w.append(&mut insert)?;
//!
//! let mut commit = WalEntry::new(0, txn_id, EntryType::Commit, 0, vec![], vec![], vec![]);
//! w.append(&mut commit)?;
//!
//! // fsync — guarantees on-disk durability
//! w.commit()?;
//! ```

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use axiomdb_core::error::DbError;

use crate::entry::WalEntry;

// ── WAL file constants ────────────────────────────────────────────────────────

/// Magic number for the WAL file: "AXIOMWAL" in little-endian.
pub const WAL_MAGIC: u64 = 0x4C41574D_4F495841; // b"AXIOMWAL" as u64 LE

/// Current WAL format version.
pub const WAL_VERSION: u16 = 2;

/// Size of the WAL file header in bytes (v2).
pub const WAL_HEADER_SIZE: usize = 24;

/// Byte offset of `start_lsn` within the WAL file header.
const START_LSN_OFFSET: usize = 14;

const _: () = assert!(
    START_LSN_OFFSET + 8 <= WAL_HEADER_SIZE,
    "start_lsn must fit in header"
);

/// Internal BufWriter capacity — 64 KB amortizes syscalls without excessive memory use.
const BUF_CAPACITY: usize = 64 * 1024;

// ── WalWriter ─────────────────────────────────────────────────────────────────

/// Append-only writer for the global WAL file.
///
/// Manages the global LSN, buffers writes in RAM, and fsyncs only on commit.
pub struct WalWriter {
    writer: BufWriter<File>,
    next_lsn: u64,
    /// Current byte position in the file (includes header + all written entries).
    offset: u64,
}

impl WalWriter {
    /// Creates a new WAL file at `path` with `start_lsn = 0` (fresh database).
    ///
    /// Fails if the file already exists — does not overwrite existing WALs.
    /// Writes the 24-byte v2 header and fsyncs before returning.
    pub fn create(path: &Path) -> Result<Self, DbError> {
        let mut file = File::create_new(path)?;
        write_header(&mut file, 0)?;
        file.sync_all()?;

        let offset = WAL_HEADER_SIZE as u64;
        Ok(Self {
            writer: BufWriter::with_capacity(BUF_CAPACITY, file),
            next_lsn: 1,
            offset,
        })
    }

    /// Opens an existing WAL file to continue writing.
    ///
    /// Verifies the magic header and version. Uses `start_lsn` from the header
    /// and the last scanned entry LSN to compute `next_lsn`, ensuring monotonicity
    /// even after WAL rotation (where the file may be empty after the header).
    ///
    /// `next_lsn = max(scan_last_lsn, header.start_lsn) + 1`
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let mut file = OpenOptions::new().read(true).append(true).open(path)?;

        let start_lsn = read_and_verify_header(&mut file, path)?;
        let last_lsn = scan_last_lsn(&mut file)?;

        // Use whichever is larger: the last entry's LSN or the header's start_lsn.
        // This handles the rotated-empty-WAL case: start_lsn = checkpoint_lsn,
        // scan returns 0 → next_lsn = checkpoint_lsn + 1. Monotonicity preserved.
        let next_lsn = last_lsn.max(start_lsn) + 1;

        let offset = file.seek(SeekFrom::End(0))?;

        Ok(Self {
            writer: BufWriter::with_capacity(BUF_CAPACITY, file),
            next_lsn,
            offset,
        })
    }

    /// Truncates the WAL file at `path` to just the v2 header with `start_lsn`.
    ///
    /// Used by WAL rotation after a successful checkpoint. The file is truncated
    /// to 0, the new header is written, and the file is fsynced.
    ///
    /// After this call, `WalWriter::open(path)` will return a writer with
    /// `next_lsn = start_lsn + 1`.
    ///
    /// # Safety ordering
    /// Caller must ensure `Checkpointer::checkpoint()` has completed successfully
    /// (all pages and the Checkpoint WAL entry are durable) before calling this.
    pub fn rotate_file(path: &Path, start_lsn: u64) -> Result<(), DbError> {
        let mut file = OpenOptions::new().write(true).open(path)?;
        file.set_len(0)?;
        write_header(&mut file, start_lsn)?;
        file.sync_all()?;
        Ok(())
    }

    /// Assigns the next LSN to the entry and writes it to the RAM buffer.
    ///
    /// **Does not fsync** — the entry is not durable until [`commit`](Self::commit) is called.
    ///
    /// Returns the LSN assigned to the entry.
    pub fn append(&mut self, entry: &mut WalEntry) -> Result<u64, DbError> {
        let lsn = self.next_lsn;
        entry.lsn = lsn;

        let bytes = entry.to_bytes();
        self.writer.write_all(&bytes)?;

        self.next_lsn += 1;
        self.offset += bytes.len() as u64;

        Ok(lsn)
    }

    /// Flushes the buffer to the OS and fsyncs — guarantees on-disk durability.
    ///
    /// Must be called after writing the COMMIT entry of a transaction.
    /// If the process dies before `commit()`, entries in the buffer are lost.
    pub fn commit(&mut self) -> Result<(), DbError> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }

    /// Returns the last assigned LSN. `0` if no entry has been written.
    pub fn current_lsn(&self) -> u64 {
        self.next_lsn.saturating_sub(1)
    }

    /// Returns the current byte position in the file (header + written entries).
    pub fn file_offset(&self) -> u64 {
        self.offset
    }

    /// Flushes the internal `BufWriter` buffer to the OS without fsync.
    ///
    /// After this call, WAL entries are visible to other readers on the same
    /// host (through the kernel page cache) but NOT guaranteed to survive a
    /// power failure.
    ///
    /// Used in tests to simulate a process crash: flush the buffer so entries
    /// are readable, then drop without committing.
    pub fn flush_buffer(&mut self) -> Result<(), DbError> {
        self.writer.flush().map_err(DbError::Io)
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Writes the 24-byte v2 header to the file.
///
/// `start_lsn` is stored at bytes [14..22]. For fresh WALs pass 0;
/// for rotated WALs pass the checkpoint LSN.
fn write_header(file: &mut File, start_lsn: u64) -> Result<(), DbError> {
    let mut header = [0u8; WAL_HEADER_SIZE];
    header[0..8].copy_from_slice(&WAL_MAGIC.to_le_bytes());
    header[8..10].copy_from_slice(&WAL_VERSION.to_le_bytes());
    // bytes [10..14]: reserved, already zero
    header[START_LSN_OFFSET..START_LSN_OFFSET + 8].copy_from_slice(&start_lsn.to_le_bytes());
    file.write_all(&header)?;
    Ok(())
}

/// Reads and verifies the WAL file header.
///
/// Returns `start_lsn` from the header, used by `open()` to compute `next_lsn`
/// correctly after WAL rotation.
fn read_and_verify_header(file: &mut File, path: &Path) -> Result<u64, DbError> {
    file.seek(SeekFrom::Start(0))?;

    let mut header = [0u8; WAL_HEADER_SIZE];
    file.read_exact(&mut header)
        .map_err(|_| DbError::WalInvalidHeader {
            path: path.display().to_string(),
        })?;

    let magic = u64::from_le_bytes([
        header[0], header[1], header[2], header[3], header[4], header[5], header[6], header[7],
    ]);
    let version = u16::from_le_bytes([header[8], header[9]]);

    if magic != WAL_MAGIC || version != WAL_VERSION {
        return Err(DbError::WalInvalidHeader {
            path: path.display().to_string(),
        });
    }

    let start_lsn = u64::from_le_bytes([
        header[START_LSN_OFFSET],
        header[START_LSN_OFFSET + 1],
        header[START_LSN_OFFSET + 2],
        header[START_LSN_OFFSET + 3],
        header[START_LSN_OFFSET + 4],
        header[START_LSN_OFFSET + 5],
        header[START_LSN_OFFSET + 6],
        header[START_LSN_OFFSET + 7],
    ]);

    Ok(start_lsn)
}

/// Scans entries from offset 16 and returns the LSN of the last valid entry.
///
/// Stops at the first truncated or invalid-CRC entry — partial entries
/// written before a crash do not count.
/// Returns `0` if there are no valid entries.
fn scan_last_lsn(file: &mut File) -> Result<u64, DbError> {
    file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;

    let file_len = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;

    let data_len = (file_len as usize).saturating_sub(WAL_HEADER_SIZE);
    if data_len == 0 {
        return Ok(0);
    }

    let mut buf = vec![0u8; data_len];
    file.read_exact(&mut buf)?;

    let mut pos = 0usize;
    let mut last_lsn = 0u64;

    while pos < buf.len() {
        match WalEntry::from_bytes(&buf[pos..]) {
            Ok((entry, consumed)) => {
                last_lsn = entry.lsn;
                pos += consumed;
            }
            Err(_) => break, // truncated or corrupt entry — end of valid WAL
        }
    }

    Ok(last_lsn)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryType;
    use tempfile::tempdir;

    fn make_insert(txn_id: u64, table_id: u32) -> WalEntry {
        WalEntry::new(
            0, // LSN assigned by the writer
            txn_id,
            EntryType::Insert,
            table_id,
            b"key:test".to_vec(),
            vec![],
            vec![1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        )
    }

    #[test]
    fn test_empty_wal_file_is_header_size_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        WalWriter::create(&path).unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), WAL_HEADER_SIZE); // 24 bytes (v2 header)
    }

    #[test]
    fn test_header_version_is_2() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        WalWriter::create(&path).unwrap();

        let data = std::fs::read(&path).unwrap();
        let version = u16::from_le_bytes([data[8], data[9]]);
        assert_eq!(version, WAL_VERSION); // 2
    }

    #[test]
    fn test_fresh_wal_start_lsn_is_zero() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        WalWriter::create(&path).unwrap();

        let data = std::fs::read(&path).unwrap();
        let start_lsn = u64::from_le_bytes([
            data[14], data[15], data[16], data[17], data[18], data[19], data[20], data[21],
        ]);
        assert_eq!(start_lsn, 0);
    }

    #[test]
    fn test_rotate_file_sets_start_lsn() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        WalWriter::create(&path).unwrap();

        WalWriter::rotate_file(&path, 42).unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), WAL_HEADER_SIZE); // only header remains
        let start_lsn = u64::from_le_bytes([
            data[14], data[15], data[16], data[17], data[18], data[19], data[20], data[21],
        ]);
        assert_eq!(start_lsn, 42);
    }

    #[test]
    fn test_open_after_rotate_starts_from_correct_lsn() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        WalWriter::create(&path).unwrap();

        // Rotate: start_lsn = 100 (simulating checkpoint_lsn = 100)
        WalWriter::rotate_file(&path, 100).unwrap();

        // Open: next_lsn should be 101
        let mut w = WalWriter::open(&path).unwrap();
        assert_eq!(w.current_lsn(), 100); // before any append, current = start_lsn

        let mut entry = make_insert(1, 1);
        let lsn = w.append(&mut entry).unwrap();
        assert_eq!(lsn, 101); // continues from 101, not 1
    }

    #[test]
    fn test_lsn_starts_at_1() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut w = WalWriter::create(&path).unwrap();

        let mut entry = make_insert(1, 1);
        let lsn = w.append(&mut entry).unwrap();
        assert_eq!(lsn, 1);
        assert_eq!(entry.lsn, 1);
    }

    #[test]
    fn test_lsn_increments() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut w = WalWriter::create(&path).unwrap();

        for expected_lsn in 1u64..=10 {
            let mut entry = make_insert(1, 1);
            let lsn = w.append(&mut entry).unwrap();
            assert_eq!(lsn, expected_lsn);
        }
        assert_eq!(w.current_lsn(), 10);
    }

    #[test]
    fn test_current_lsn_zero_before_append() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = WalWriter::create(&path).unwrap();
        assert_eq!(w.current_lsn(), 0);
    }

    #[test]
    fn test_file_offset_grows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut w = WalWriter::create(&path).unwrap();

        let initial = w.file_offset();
        assert_eq!(initial, WAL_HEADER_SIZE as u64);

        let mut entry = make_insert(1, 1);
        w.append(&mut entry).unwrap();
        assert!(w.file_offset() > initial);
    }
}
