//! WalReader — streaming reads from the WAL file.
//!
//! ## Design
//!
//! `WalReader` is stateless — it verifies the header in `open()` but does not
//! keep a file handle open. Each scan opens its own `File` handle, which eliminates
//! shared mutable state and allows multiple independent scans.
//!
//! - [`WalReader::scan_forward`]: `BufReader<File>` — amortizes syscalls in sequential reads.
//! - [`WalReader::scan_backward`]: direct seekable `File` — seeks invalidate the `BufReader`
//!   buffer, so backward uses the file handle directly.
//!
//! ## Behavior on corruption
//!
//! Both iterators return `Result<WalEntry>`. On the first error (truncated entry,
//! invalid CRC, unknown type), the item is `Err(...)` and the iterator stops.
//! The caller decides whether to propagate the error or ignore it based on the use case.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use axiomdb_core::error::DbError;

use crate::entry::WalEntry;
use crate::writer::{scan_valid_tail, WAL_HEADER_SIZE, WAL_MAGIC, WAL_VERSION};

// ── WalReader ─────────────────────────────────────────────────────────────────

/// WAL file reader. Stateless — opens a `File` per scan.
pub struct WalReader {
    path: PathBuf,
    logical_end: u64,
}

impl WalReader {
    /// Opens an existing WAL file and verifies its header.
    ///
    /// Does not keep the file handle open — only validates that the file
    /// is a valid WAL (correct magic + version).
    ///
    /// # Errors
    /// - [`DbError::WalInvalidHeader`] if the magic, version, or header size are incorrect
    /// - [`DbError::Io`] if the file does not exist or cannot be read
    pub fn open(path: &Path) -> Result<Self, DbError> {
        verify_header(path)?;
        let mut file = File::open(path)?;
        let tail = scan_valid_tail(&mut file)?;
        Ok(Self {
            path: path.to_path_buf(),
            logical_end: tail.logical_end,
        })
    }

    /// Returns an iterator that reads entries forward from `from_lsn`.
    ///
    /// Entries with `LSN < from_lsn` are skipped in a linear scan from the start.
    /// To read the entire WAL use `from_lsn = 0` (or `from_lsn = 1`).
    ///
    /// The iterator stops (returning `Some(Err(...))`) on the first truncated
    /// or corrupt entry, then returns `None`.
    pub fn scan_forward(&self, from_lsn: u64) -> Result<ForwardIter, DbError> {
        let file = File::open(&self.path)?;
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        // Skip the header — already verified in open()
        reader.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
        Ok(ForwardIter {
            reader,
            from_lsn,
            logical_end: self.logical_end,
            cursor: WAL_HEADER_SIZE as u64,
            done: false,
        })
    }

    /// Returns an iterator that reads entries backward from the last valid entry.
    ///
    /// Returns entries in **decreasing** LSN order — most recent first.
    /// Useful for ROLLBACK (undoing operations from most recent to oldest).
    pub fn scan_backward(&self) -> Result<BackwardIter, DbError> {
        let file = File::open(&self.path)?;
        Ok(BackwardIter {
            file,
            cursor: self.logical_end,
            done: false,
        })
    }
}

// ── ForwardIter ───────────────────────────────────────────────────────────────

/// Sequential WAL read iterator (increasing LSN).
pub struct ForwardIter {
    reader: BufReader<File>,
    from_lsn: u64,
    logical_end: u64,
    cursor: u64,
    done: bool,
}

impl Iterator for ForwardIter {
    type Item = Result<WalEntry, DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.cursor >= self.logical_end {
            return None;
        }

        loop {
            if self.cursor >= self.logical_end {
                return None;
            }
            if self.cursor + 4 > self.logical_end {
                self.done = true;
                return None;
            }
            // ── Read entry_len (4 bytes) ──────────────────────────────────────
            let mut len_buf = [0u8; 4];
            match self.reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Clean EOF — the WAL ended without truncated entries
                    return None;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e.into()));
                }
            }

            let entry_len = u32::from_le_bytes(len_buf) as usize;
            if self.cursor + entry_len as u64 > self.logical_end {
                self.done = true;
                return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
            }

            if entry_len < crate::entry::MIN_ENTRY_LEN {
                self.done = true;
                return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
            }

            // ── Read the rest of the entry ────────────────────────────────────
            let rest_len = entry_len - 4;
            let mut buf = vec![0u8; entry_len];
            buf[0..4].copy_from_slice(&len_buf);

            match self.reader.read_exact(&mut buf[4..]) {
                Ok(()) => {}
                Err(_) => {
                    self.done = true;
                    return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
                }
            }

            let _ = rest_len; // used implicitly in the slice above

            // ── Parse and verify CRC ──────────────────────────────────────────
            match WalEntry::from_bytes(&buf) {
                Ok((entry, _consumed)) => {
                    self.cursor += entry_len as u64;
                    if entry.lsn < self.from_lsn {
                        // Skip this entry and continue to the next
                        continue;
                    }
                    return Some(Ok(entry));
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

// ── BackwardIter ──────────────────────────────────────────────────────────────

/// Reverse WAL read iterator (decreasing LSN).
///
/// Uses the `entry_len_2` field (last 4 bytes of each entry) to navigate
/// backward without reading the entire file.
pub struct BackwardIter {
    file: File,
    /// Position in the file of the byte immediately after the last unread entry.
    /// Initially = file_len. Decreases with each entry read.
    cursor: u64,
    done: bool,
}

impl Iterator for BackwardIter {
    type Item = Result<WalEntry, DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        // We have reached the start of the entries area
        if self.cursor <= WAL_HEADER_SIZE as u64 {
            return None;
        }

        // ── Read entry_len_2 (last 4 bytes of the current entry) ─────────────
        if self.cursor < WAL_HEADER_SIZE as u64 + 4 {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        let trailer_pos = self.cursor - 4;
        if let Err(e) = self.file.seek(SeekFrom::Start(trailer_pos)) {
            self.done = true;
            return Some(Err(e.into()));
        }

        let mut len_buf = [0u8; 4];
        if self.file.read_exact(&mut len_buf).is_err() {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        let entry_len = u32::from_le_bytes(len_buf) as u64;

        if entry_len < crate::entry::MIN_ENTRY_LEN as u64 {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        // ── Calculate entry start ─────────────────────────────────────────────
        if self.cursor < entry_len {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        let entry_start = self.cursor - entry_len;

        if entry_start < WAL_HEADER_SIZE as u64 {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        // ── Read the complete entry ───────────────────────────────────────────
        if let Err(e) = self.file.seek(SeekFrom::Start(entry_start)) {
            self.done = true;
            return Some(Err(e.into()));
        }

        let mut buf = vec![0u8; entry_len as usize];
        if self.file.read_exact(&mut buf).is_err() {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        // ── Parse and verify CRC ──────────────────────────────────────────────
        match WalEntry::from_bytes(&buf) {
            Ok((entry, _)) => {
                self.cursor = entry_start;
                Some(Ok(entry))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn verify_header(path: &Path) -> Result<(), DbError> {
    let mut file = File::open(path)?;
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

    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryType;
    use crate::writer::WalWriter;
    use tempfile::tempdir;

    fn write_entries(path: &Path, count: u64) -> Vec<WalEntry> {
        let mut writer = WalWriter::create(path).unwrap();
        let mut entries = Vec::new();
        for i in 0..count {
            let mut e = WalEntry::new(
                0,
                i + 1,
                EntryType::Insert,
                1,
                format!("key:{:04}", i).into_bytes(),
                vec![],
                vec![i as u8, 0, 0, 0],
            );
            writer.append(&mut e).unwrap();
            entries.push(e);
        }
        writer.commit().unwrap();
        entries
    }

    #[test]
    fn test_open_valid_wal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        WalWriter::create(&path).unwrap();
        assert!(WalReader::open(&path).is_ok());
    }

    #[test]
    fn test_open_nonexistent() {
        let path = std::path::Path::new("/tmp/nonexistent_wal_file_axiomdb.wal");
        assert!(matches!(WalReader::open(path), Err(DbError::Io(_))));
    }

    #[test]
    fn test_open_invalid_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.wal");
        std::fs::write(&path, b"BADMAGIC00000000").unwrap();
        assert!(matches!(
            WalReader::open(&path),
            Err(DbError::WalInvalidHeader { .. })
        ));
    }

    #[test]
    fn test_forward_empty_wal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.wal");
        WalWriter::create(&path).unwrap();
        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader.scan_forward(0).unwrap().collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_backward_empty_wal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.wal");
        WalWriter::create(&path).unwrap();
        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader.scan_backward().unwrap().collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_forward_all_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let written = write_entries(&path, 10);

        let reader = WalReader::open(&path).unwrap();
        let read: Vec<WalEntry> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(read.len(), 10);
        for (i, entry) in read.iter().enumerate() {
            assert_eq!(entry.lsn, written[i].lsn);
            assert_eq!(entry.key, written[i].key);
        }
    }

    #[test]
    fn test_forward_from_lsn_skips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        write_entries(&path, 10);

        let reader = WalReader::open(&path).unwrap();
        let read: Vec<WalEntry> = reader
            .scan_forward(6)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(read.len(), 5); // LSN 6, 7, 8, 9, 10
        assert_eq!(read[0].lsn, 6);
        assert_eq!(read[4].lsn, 10);
    }

    #[test]
    fn test_backward_all_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        write_entries(&path, 5);

        let reader = WalReader::open(&path).unwrap();
        let lsns: Vec<u64> = reader
            .scan_backward()
            .unwrap()
            .map(|r| r.unwrap().lsn)
            .collect();

        assert_eq!(lsns, vec![5, 4, 3, 2, 1]);
    }

    #[test]
    fn test_backward_matches_forward_reversed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        write_entries(&path, 8);

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

        assert_eq!(forward.len(), backward.len());
        for (f, b) in forward.iter().zip(backward.iter().rev()) {
            assert_eq!(f.lsn, b.lsn);
            assert_eq!(f.key, b.key);
        }
    }

    #[test]
    fn test_forward_ignores_reserved_zero_tail() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("reserved-tail.wal");
        write_entries(&path, 3);

        let original_len = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(original_len + 4096)
            .unwrap();

        let reader = WalReader::open(&path).unwrap();
        let lsns: Vec<u64> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap().lsn)
            .collect();
        assert_eq!(lsns, vec![1, 2, 3]);
    }

    #[test]
    fn test_backward_starts_at_logical_end_not_physical_eof() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("reserved-tail-backward.wal");
        write_entries(&path, 4);

        let original_len = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(original_len + 8192)
            .unwrap();

        let reader = WalReader::open(&path).unwrap();
        let lsns: Vec<u64> = reader
            .scan_backward()
            .unwrap()
            .map(|r| r.unwrap().lsn)
            .collect();
        assert_eq!(lsns, vec![4, 3, 2, 1]);
    }
}
