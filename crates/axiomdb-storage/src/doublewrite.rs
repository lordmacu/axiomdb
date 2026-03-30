//! Doublewrite buffer for torn page repair (Phase 3.8c).
//!
//! A 16 KB `pwrite()` is NOT crash-atomic on 4 KB-block filesystems (APFS,
//! ext4, XFS, ZFS). A power failure mid-write leaves a **torn page**: the
//! first N×4 KB contain new data, the remainder holds the previous state.
//! CRC32c detects this corruption on startup, but without a repair source the
//! database cannot open.
//!
//! The doublewrite (DW) buffer solves this: before every `flush()`, all dirty
//! pages are serialized to a `.dw` file and fsynced. If the main file fsync is
//! interrupted by a crash, `MmapStorage::open` uses the `.dw` file to restore
//! any torn pages.
//!
//! ## DW file format
//!
//! ```text
//! [Header: 16 bytes]
//!   magic:       [u8; 8]  = b"AXMDBLWR"
//!   version:     u32 LE   = 1
//!   slot_count:  u32 LE
//!
//! [Slots: slot_count × 16392 bytes each]
//!   page_id:     u64 LE
//!   page_data:   [u8; PAGE_SIZE]
//!
//! [Footer: 8 bytes]
//!   file_crc:    u32 LE   = CRC32c(header || all slots)
//!   sentinel:    u32 LE   = 0xDEAD_BEEF
//! ```
//!
//! The file is valid only if all three hold:
//! 1. `footer.sentinel == 0xDEAD_BEEF`
//! 2. `CRC32c(header || slots) == footer.file_crc`
//! 3. `file.len() == 16 + slot_count × 16392 + 8`
//!
//! ## Design reference
//!
//! MySQL 8.0.20+ moved InnoDB's doublewrite buffer from an embedded system
//! tablespace zone to a standalone `#ib_*.dblwr` file — better sequential I/O,
//! zero impact on the main tablespace format. AxiomDB follows the same approach.

use std::fs;
use std::path::{Path, PathBuf};

use axiomdb_core::error::DbError;
use tracing::{debug, info, warn};

use crate::page::{HEADER_SIZE, PAGE_SIZE};

// ── Constants ────────────────────────────────────────────────────────────────

/// DW file magic: "AXMDBLWR" (AXiomDB DouBLe WRite).
const DW_MAGIC: [u8; 8] = *b"AXMDBLWR";

/// DW file format version.
const DW_VERSION: u32 = 1;

/// Footer sentinel — written last, validates completeness.
const DW_SENTINEL: u32 = 0xDEAD_BEEF;

/// Header size: magic(8) + version(4) + slot_count(4).
const DW_HEADER_SIZE: usize = 16;

/// Slot size: page_id(8) + page_data(PAGE_SIZE).
const DW_SLOT_SIZE: usize = 8 + PAGE_SIZE;

/// Footer size: file_crc(4) + sentinel(4).
const DW_FOOTER_SIZE: usize = 8;

/// Offset of the `checksum` field inside a PageHeader (same as in mmap.rs).
const CHECKSUM_OFFSET: usize = 12;

// ── DoublewriteBuffer ────────────────────────────────────────────────────────

/// Manages the `.dw` file for torn page repair.
///
/// The struct is intentionally lightweight — no open file handle. The `.dw`
/// file is opened, written, fsynced, and closed within a single
/// `write_and_sync` call to avoid holding extra file descriptors.
pub struct DoublewriteBuffer {
    /// Path to the `.dw` file (e.g. `/data/my.db.dw`).
    path: PathBuf,
}

impl DoublewriteBuffer {
    /// Creates a `DoublewriteBuffer` for the given database file path.
    /// The `.dw` file lives alongside the main `.db` file.
    ///
    /// ```text
    /// main file: /path/to/database.db
    /// DW file:   /path/to/database.db.dw
    /// ```
    pub fn for_db(db_path: &Path) -> Self {
        let mut dw_path = db_path.as_os_str().to_owned();
        dw_path.push(".dw");
        Self {
            path: PathBuf::from(dw_path),
        }
    }

    /// Returns `true` if the `.dw` file exists on disk.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Serializes `pages` to the `.dw` file and fsyncs.
    ///
    /// Each entry is a `(page_id, raw_page_bytes)` pair where `raw_page_bytes`
    /// is exactly `PAGE_SIZE` bytes.
    ///
    /// After this returns `Ok(())`, the DW file is durable and can be used to
    /// repair any torn page in the main file.
    pub fn write_and_sync(&self, pages: &[(u64, &[u8])]) -> Result<(), DbError> {
        if pages.is_empty() {
            return Ok(());
        }

        // Validate all slices are PAGE_SIZE.
        for (page_id, data) in pages {
            debug_assert_eq!(
                data.len(),
                PAGE_SIZE,
                "DW slot for page {page_id}: expected {PAGE_SIZE} bytes, got {}",
                data.len()
            );
        }

        let slot_count = pages.len();
        let total_size = DW_HEADER_SIZE + slot_count * DW_SLOT_SIZE + DW_FOOTER_SIZE;

        // Build the entire DW file in a single buffer for sequential I/O.
        let mut buf = vec![0u8; total_size];

        // ── Header ───────────────────────────────────────────────────
        buf[0..8].copy_from_slice(&DW_MAGIC);
        buf[8..12].copy_from_slice(&DW_VERSION.to_le_bytes());
        buf[12..16].copy_from_slice(&(slot_count as u32).to_le_bytes());

        // ── Slots ────────────────────────────────────────────────────
        let mut offset = DW_HEADER_SIZE;
        for &(page_id, data) in pages {
            buf[offset..offset + 8].copy_from_slice(&page_id.to_le_bytes());
            buf[offset + 8..offset + 8 + PAGE_SIZE].copy_from_slice(data);
            offset += DW_SLOT_SIZE;
        }

        // ── Footer ───────────────────────────────────────────────────
        // CRC32c covers header + all slots (everything before the footer).
        let file_crc = crc32c::crc32c(&buf[..offset]);
        buf[offset..offset + 4].copy_from_slice(&file_crc.to_le_bytes());
        buf[offset + 4..offset + 8].copy_from_slice(&DW_SENTINEL.to_le_bytes());

        // ── Write + fsync ────────────────────────────────────────────
        use std::io::Write;
        let file = fs::File::create(&self.path)
            .map_err(|e| axiomdb_core::error::classify_io(e, "dw create"))?;
        let mut writer = std::io::BufWriter::new(&file);
        writer
            .write_all(&buf)
            .map_err(|e| axiomdb_core::error::classify_io(e, "dw write"))?;
        writer
            .flush()
            .map_err(|e| axiomdb_core::error::classify_io(e, "dw flush"))?;
        file.sync_all()
            .map_err(|e| axiomdb_core::error::classify_io(e, "dw fsync"))?;

        debug!(
            path = %self.path.display(),
            slot_count,
            bytes = total_size,
            "doublewrite buffer written and fsynced"
        );
        Ok(())
    }

    /// Attempts to recover torn pages from the `.dw` file.
    ///
    /// For each page slot in the DW file, the corresponding page in the main
    /// file is checked via CRC32c. If the CRC is invalid (torn page), the
    /// page is restored from the DW copy.
    ///
    /// Returns the number of pages repaired. Returns `Ok(0)` if the DW file
    /// is invalid, incomplete, or if all main file pages are already valid.
    ///
    /// ## Idempotence
    ///
    /// If recovery is interrupted (crash during repair writes), the DW file
    /// still exists and the next startup reruns recovery. Pages already
    /// repaired have valid CRCs → skipped. Perfectly safe.
    pub fn recover(&self, file: &std::fs::File) -> Result<usize, DbError> {
        if !self.exists() {
            return Ok(0);
        }

        let data =
            fs::read(&self.path).map_err(|e| axiomdb_core::error::classify_io(e, "dw read"))?;

        if !Self::validate(&data) {
            warn!(
                path = %self.path.display(),
                len = data.len(),
                "invalid or incomplete DW file — ignoring"
            );
            return Ok(0);
        }

        let slot_count = u32::from_le_bytes(data[12..16].try_into().expect("4 bytes")) as usize;
        let mut repaired = 0usize;

        for i in 0..slot_count {
            let slot_offset = DW_HEADER_SIZE + i * DW_SLOT_SIZE;

            let page_id = u64::from_le_bytes(
                data[slot_offset..slot_offset + 8]
                    .try_into()
                    .expect("8 bytes"),
            );
            let dw_page_data = &data[slot_offset + 8..slot_offset + 8 + PAGE_SIZE];

            // Read current page from main file.
            let file_offset = page_id * PAGE_SIZE as u64;
            let mut current = [0u8; PAGE_SIZE];
            {
                use std::os::unix::fs::FileExt;
                file.read_exact_at(&mut current, file_offset)
                    .map_err(|e| axiomdb_core::error::classify_io(e, "dw recover read"))?;
            }

            // Check CRC: if invalid, the page is torn → restore from DW.
            let stored_crc = u32::from_le_bytes(
                current[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4]
                    .try_into()
                    .expect("4 bytes"),
            );
            let actual_crc = crc32c::crc32c(&current[HEADER_SIZE..PAGE_SIZE]);

            if stored_crc != actual_crc {
                info!(
                    page_id,
                    stored_crc = format_args!("{stored_crc:#010x}"),
                    actual_crc = format_args!("{actual_crc:#010x}"),
                    "repairing torn page from doublewrite buffer"
                );
                use std::os::unix::fs::FileExt;
                file.write_all_at(dw_page_data, file_offset)
                    .map_err(|e| axiomdb_core::error::classify_io(e, "dw recover write"))?;
                repaired += 1;
            }
        }

        if repaired > 0 {
            // Make repairs durable before the normal startup scan.
            file.sync_all()
                .map_err(|e| axiomdb_core::error::classify_io(e, "dw recover fsync"))?;
        }

        Ok(repaired)
    }

    /// Removes the `.dw` file. Non-fatal if the file does not exist.
    pub fn cleanup(&self) -> Result<(), DbError> {
        match fs::remove_file(&self.path) {
            Ok(()) => {
                debug!(path = %self.path.display(), "doublewrite file removed");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(axiomdb_core::error::classify_io(e, "dw cleanup")),
        }
    }

    // ── Private ──────────────────────────────────────────────────────────────

    /// Validates the DW file structure: magic, version, size, CRC, sentinel.
    fn validate(data: &[u8]) -> bool {
        // Minimum: header + footer (zero slots is valid).
        if data.len() < DW_HEADER_SIZE + DW_FOOTER_SIZE {
            return false;
        }

        // Magic.
        if data[0..8] != DW_MAGIC {
            return false;
        }

        // Version.
        let version = u32::from_le_bytes(data[8..12].try_into().expect("4 bytes"));
        if version != DW_VERSION {
            return false;
        }

        // Size consistency.
        let slot_count = u32::from_le_bytes(data[12..16].try_into().expect("4 bytes")) as usize;
        let expected_size = DW_HEADER_SIZE + slot_count * DW_SLOT_SIZE + DW_FOOTER_SIZE;
        if data.len() != expected_size {
            return false;
        }

        // Sentinel (last 4 bytes of file).
        let sentinel_offset = expected_size - 4;
        let sentinel = u32::from_le_bytes(
            data[sentinel_offset..sentinel_offset + 4]
                .try_into()
                .expect("4 bytes"),
        );
        if sentinel != DW_SENTINEL {
            return false;
        }

        // CRC32c: covers header + all slots (everything before footer).
        let payload_end = expected_size - DW_FOOTER_SIZE;
        let expected_crc = u32::from_le_bytes(
            data[payload_end..payload_end + 4]
                .try_into()
                .expect("4 bytes"),
        );
        let actual_crc = crc32c::crc32c(&data[..payload_end]);

        expected_crc == actual_crc
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Page, PageType};

    /// Helper: creates a valid page with known data for testing.
    fn make_test_page(page_id: u64, fill_byte: u8) -> Page {
        let mut page = Page::new(PageType::Data, page_id);
        page.body_mut()[0] = fill_byte;
        page.body_mut()[1] = fill_byte.wrapping_add(1);
        page.update_checksum();
        page
    }

    /// Helper: builds a DW buffer in a temp directory.
    fn make_dw(dir: &tempfile::TempDir) -> DoublewriteBuffer {
        let db_path = dir.path().join("test.db");
        DoublewriteBuffer::for_db(&db_path)
    }

    // ── validate tests ───────────────────────────────────────────────

    #[test]
    fn test_validate_empty() {
        assert!(!DoublewriteBuffer::validate(&[]));
    }

    #[test]
    fn test_validate_truncated_header() {
        assert!(!DoublewriteBuffer::validate(&[0u8; 10]));
    }

    #[test]
    fn test_validate_wrong_magic() {
        let mut data = build_valid_dw(&[]);
        data[0] = 0xFF; // corrupt magic
        assert!(!DoublewriteBuffer::validate(&data));
    }

    #[test]
    fn test_validate_wrong_version() {
        let mut data = build_valid_dw(&[]);
        // Set version to 99
        data[8..12].copy_from_slice(&99u32.to_le_bytes());
        // Recompute CRC (won't match sentinel though)
        assert!(!DoublewriteBuffer::validate(&data));
    }

    #[test]
    fn test_validate_wrong_slot_count() {
        let mut data = build_valid_dw(&[]);
        // Claim 5 slots but file only has 0
        data[12..16].copy_from_slice(&5u32.to_le_bytes());
        assert!(!DoublewriteBuffer::validate(&data));
    }

    #[test]
    fn test_validate_wrong_sentinel() {
        let mut data = build_valid_dw(&[]);
        let last = data.len();
        data[last - 1] = 0x00; // corrupt sentinel
        assert!(!DoublewriteBuffer::validate(&data));
    }

    #[test]
    fn test_validate_wrong_crc() {
        let mut data = build_valid_dw(&[(42, &[0xABu8; PAGE_SIZE])]);
        // Corrupt one byte in slot data
        data[DW_HEADER_SIZE + 10] ^= 0xFF;
        assert!(!DoublewriteBuffer::validate(&data));
    }

    #[test]
    fn test_validate_valid_zero_slots() {
        let data = build_valid_dw(&[]);
        assert!(DoublewriteBuffer::validate(&data));
    }

    #[test]
    fn test_validate_valid_one_slot() {
        let data = build_valid_dw(&[(1, &[0xCDu8; PAGE_SIZE])]);
        assert!(DoublewriteBuffer::validate(&data));
    }

    #[test]
    fn test_validate_valid_many_slots() {
        let pages: Vec<(u64, [u8; PAGE_SIZE])> =
            (0..10).map(|i| (i, [i as u8; PAGE_SIZE])).collect();
        let refs: Vec<(u64, &[u8])> = pages
            .iter()
            .map(|(id, data)| (*id, data.as_slice()))
            .collect();
        let data = build_valid_dw(&refs);
        assert!(DoublewriteBuffer::validate(&data));
    }

    // ── write + recover roundtrip ────────────────────────────────────

    #[test]
    fn test_write_and_sync_creates_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let dw = make_dw(&dir);

        let page = make_test_page(5, 0xAB);
        dw.write_and_sync(&[(5, page.as_bytes())]).unwrap();

        assert!(dw.exists());
        let data = fs::read(&dw.path).unwrap();
        assert!(DoublewriteBuffer::validate(&data));
    }

    #[test]
    fn test_write_empty_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let dw = make_dw(&dir);
        dw.write_and_sync(&[]).unwrap();
        assert!(!dw.exists());
    }

    #[test]
    fn test_cleanup_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let dw = make_dw(&dir);

        let page = make_test_page(2, 0x11);
        dw.write_and_sync(&[(2, page.as_bytes())]).unwrap();
        assert!(dw.exists());

        dw.cleanup().unwrap();
        assert!(!dw.exists());
    }

    #[test]
    fn test_cleanup_noop_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let dw = make_dw(&dir);
        assert!(!dw.exists());
        dw.cleanup().unwrap(); // should not error
    }

    #[test]
    fn test_recover_repairs_torn_page() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let dw = DoublewriteBuffer::for_db(&db_path);

        // Create a small "database file" with 3 pages.
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&db_path)
            .unwrap();
        {
            use std::os::unix::fs::FileExt;

            let p0 = make_test_page(0, 0x10);
            let p1 = make_test_page(1, 0x20);
            let p2 = make_test_page(2, 0x30);
            file.write_all_at(p0.as_bytes(), 0).unwrap();
            file.write_all_at(p1.as_bytes(), PAGE_SIZE as u64).unwrap();
            file.write_all_at(p2.as_bytes(), 2 * PAGE_SIZE as u64)
                .unwrap();
            file.sync_all().unwrap();
        }

        // Write DW file with good copies of pages 1 and 2.
        let good_p1 = make_test_page(1, 0x20);
        let good_p2 = make_test_page(2, 0x30);
        dw.write_and_sync(&[(1, good_p1.as_bytes()), (2, good_p2.as_bytes())])
            .unwrap();

        // Simulate a torn page: corrupt page 2 in the main file.
        {
            use std::os::unix::fs::FileExt;
            // Flip some bytes in the middle of page 2 → CRC mismatch.
            let corrupt_offset = 2 * PAGE_SIZE as u64 + 1000;
            file.write_all_at(&[0xFF, 0xFF, 0xFF, 0xFF], corrupt_offset)
                .unwrap();
            file.sync_all().unwrap();
        }

        // Recover.
        let repaired = dw.recover(&file).unwrap();
        assert_eq!(repaired, 1, "exactly one torn page should be repaired");

        // Verify page 2 is restored.
        {
            use std::os::unix::fs::FileExt;
            let mut buf = [0u8; PAGE_SIZE];
            file.read_exact_at(&mut buf, 2 * PAGE_SIZE as u64).unwrap();
            assert_eq!(buf[HEADER_SIZE], 0x30, "body[0] should be restored");
            assert_eq!(buf[HEADER_SIZE + 1], 0x31, "body[1] should be restored");
        }
    }

    #[test]
    fn test_recover_skips_valid_pages() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let dw = DoublewriteBuffer::for_db(&db_path);

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&db_path)
            .unwrap();

        let page = make_test_page(0, 0xAA);
        {
            use std::os::unix::fs::FileExt;
            file.write_all_at(page.as_bytes(), 0).unwrap();
            file.sync_all().unwrap();
        }

        // DW has the same page — no corruption.
        dw.write_and_sync(&[(0, page.as_bytes())]).unwrap();

        let repaired = dw.recover(&file).unwrap();
        assert_eq!(repaired, 0, "no pages should need repair");
    }

    #[test]
    fn test_recover_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let dw = DoublewriteBuffer::for_db(&db_path);

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&db_path)
            .unwrap();

        let page = make_test_page(0, 0xBB);
        {
            use std::os::unix::fs::FileExt;
            file.write_all_at(page.as_bytes(), 0).unwrap();
            file.sync_all().unwrap();
        }

        // DW with good page.
        dw.write_and_sync(&[(0, page.as_bytes())]).unwrap();

        // Corrupt page in main file.
        {
            use std::os::unix::fs::FileExt;
            file.write_all_at(&[0xFF; 8], 500).unwrap();
            file.sync_all().unwrap();
        }

        // First recovery.
        let r1 = dw.recover(&file).unwrap();
        assert_eq!(r1, 1);

        // Second recovery (idempotent — page already repaired).
        let r2 = dw.recover(&file).unwrap();
        assert_eq!(r2, 0);
    }

    #[test]
    fn test_recover_invalid_dw_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let dw = DoublewriteBuffer::for_db(&db_path);

        // Write garbage to DW file.
        fs::write(&dw.path, b"this is not a valid dw file").unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&db_path)
            .unwrap();
        file.set_len(PAGE_SIZE as u64).unwrap();

        let repaired = dw.recover(&file).unwrap();
        assert_eq!(repaired, 0);
    }

    #[test]
    fn test_recover_no_dw_file_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let dw = DoublewriteBuffer::for_db(&db_path);

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&db_path)
            .unwrap();

        let repaired = dw.recover(&file).unwrap();
        assert_eq!(repaired, 0);
    }

    // ── Helpers ──────────────────────────────────────────────────────

    /// Builds a valid DW file buffer for testing validate().
    fn build_valid_dw(pages: &[(u64, &[u8])]) -> Vec<u8> {
        let slot_count = pages.len();
        let total = DW_HEADER_SIZE + slot_count * DW_SLOT_SIZE + DW_FOOTER_SIZE;
        let mut buf = vec![0u8; total];

        // Header.
        buf[0..8].copy_from_slice(&DW_MAGIC);
        buf[8..12].copy_from_slice(&DW_VERSION.to_le_bytes());
        buf[12..16].copy_from_slice(&(slot_count as u32).to_le_bytes());

        // Slots.
        let mut offset = DW_HEADER_SIZE;
        for &(page_id, data) in pages {
            buf[offset..offset + 8].copy_from_slice(&page_id.to_le_bytes());
            let len = data.len().min(PAGE_SIZE);
            buf[offset + 8..offset + 8 + len].copy_from_slice(&data[..len]);
            offset += DW_SLOT_SIZE;
        }

        // Footer.
        let file_crc = crc32c::crc32c(&buf[..offset]);
        buf[offset..offset + 4].copy_from_slice(&file_crc.to_le_bytes());
        buf[offset + 4..offset + 8].copy_from_slice(&DW_SENTINEL.to_le_bytes());

        buf
    }
}
