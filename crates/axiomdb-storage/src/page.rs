use axiomdb_core::error::DbError;

pub const PAGE_SIZE: usize = 16_384;
pub const HEADER_SIZE: usize = 64;
pub const PAGE_MAGIC: u64 = 0x4158494F_4D444200; // "AXIOMDB\0"

// ── PageType ──────────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    Meta = 0,
    Data = 1,
    Index = 2,
    Overflow = 3,
    Free = 4,
}

impl TryFrom<u8> for PageType {
    type Error = DbError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(PageType::Meta),
            1 => Ok(PageType::Data),
            2 => Ok(PageType::Index),
            3 => Ok(PageType::Overflow),
            4 => Ok(PageType::Free),
            _ => Err(DbError::ParseError {
                message: format!("unknown page_type: {v}"),
            }),
        }
    }
}

// ── PageHeader ────────────────────────────────────────────────────────────────

/// 64-byte header (1 cache line). `repr(C)` guarantees exact layout.
///
/// The checksum covers only the body `[HEADER_SIZE..PAGE_SIZE]`, not the
/// header itself, to avoid including the `checksum` field in its own calculation.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PageHeader {
    /// Magic number to detect an invalid file or catastrophic corruption.
    pub magic: u64,
    /// Page type.
    pub page_type: u8,
    /// Status flags (dirty, pinned, compressed…).
    pub flags: u8,
    /// Number of items stored in the body.
    pub item_count: u16,
    /// CRC32c of the body `[HEADER_SIZE..PAGE_SIZE]`.
    pub checksum: u32,
    /// Logical page identifier (its index in the file).
    pub page_id: u64,
    /// Log Sequence Number of the last write that touched this page.
    pub lsn: u64,
    /// Offset from the start of the page where free space begins.
    pub free_start: u16,
    /// Offset from the start of the page where free space ends.
    pub free_end: u16,
    pub _reserved: [u8; 28],
}

// Compile-time check: the header must be exactly 64 bytes.
const _: () = assert!(
    std::mem::size_of::<PageHeader>() == HEADER_SIZE,
    "PageHeader must be exactly 64 bytes"
);

// ── Page ──────────────────────────────────────────────────────────────────────

/// 16 KB page aligned to 64 bytes (cache line).
///
/// Internally it is a raw buffer `[u8; PAGE_SIZE]`; the header is accessed
/// via raw pointers to achieve zero-copy access without serialization.
#[repr(C, align(64))]
pub struct Page {
    data: [u8; PAGE_SIZE],
}

// Compile-time checks.
const _: () = assert!(
    std::mem::size_of::<Page>() == PAGE_SIZE,
    "Page must be exactly PAGE_SIZE bytes"
);
const _: () = assert!(
    std::mem::align_of::<Page>() == 64,
    "Page must be aligned to 64 bytes"
);

impl Page {
    /// Creates a new page initialized with a valid header and zeroed body.
    pub fn new(page_type: PageType, page_id: u64) -> Self {
        let mut page = Page {
            data: [0u8; PAGE_SIZE],
        };

        let hdr = page.header_mut();
        hdr.magic = PAGE_MAGIC;
        hdr.page_type = page_type as u8;
        hdr.flags = 0;
        hdr.item_count = 0;
        hdr.page_id = page_id;
        hdr.lsn = 0;
        hdr.free_start = HEADER_SIZE as u16;
        hdr.free_end = PAGE_SIZE as u16;
        hdr._reserved = [0u8; 28];

        page.update_checksum();
        page
    }

    /// Builds a `Page` from raw bytes, verifying magic and checksum.
    /// Used when reading from disk or mmap.
    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Result<Self, DbError> {
        let page = Page { data: bytes };

        let magic = page.header().magic;
        if magic != PAGE_MAGIC {
            return Err(DbError::ChecksumMismatch {
                page_id: page.header().page_id,
                expected: PAGE_MAGIC as u32,
                got: magic as u32,
            });
        }

        page.verify_checksum()?;
        Ok(page)
    }

    /// Reference to the header by interpreting the first 64 bytes.
    ///
    /// # SAFETY
    /// `data` has `PAGE_SIZE >= HEADER_SIZE` bytes and `Page` is `repr(C,
    /// align(64))`, so the first 64 bytes are correctly aligned for
    /// `PageHeader` (`repr(C)`). `PageHeader` has no hidden padding
    /// (verified by the size assert).
    pub fn header(&self) -> &PageHeader {
        unsafe { &*(self.data.as_ptr() as *const PageHeader) }
    }

    /// Mutable reference to the header.
    ///
    /// # SAFETY
    /// Same guarantees as `header()`. Mutability is safe because `Page`
    /// owns the complete buffer and no other alias exists.
    pub fn header_mut(&mut self) -> &mut PageHeader {
        unsafe { &mut *(self.data.as_mut_ptr() as *mut PageHeader) }
    }

    /// Slice of the body (bytes after the header).
    pub fn body(&self) -> &[u8] {
        &self.data[HEADER_SIZE..]
    }

    /// Mutable slice of the body.
    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.data[HEADER_SIZE..]
    }

    /// Raw bytes of the complete page (for writing to disk).
    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.data
    }

    /// Mutable raw bytes of the complete page (for in-place writes to the body).
    ///
    /// Callers must call `update_checksum()` after mutating the body.
    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.data
    }

    /// Verifies that the CRC32c of the body matches `header.checksum`.
    pub fn verify_checksum(&self) -> Result<(), DbError> {
        let expected = self.header().checksum;
        let got = crc32c::crc32c(self.body());
        if expected != got {
            return Err(DbError::ChecksumMismatch {
                page_id: self.header().page_id,
                expected,
                got,
            });
        }
        Ok(())
    }

    /// Recalculates and writes the CRC32c of the body into the header.
    /// Always call before flushing to disk.
    pub fn update_checksum(&mut self) {
        let checksum = crc32c::crc32c(self.body());
        self.header_mut().checksum = checksum;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_sizes() {
        assert_eq!(std::mem::size_of::<PageHeader>(), HEADER_SIZE);
        assert_eq!(std::mem::size_of::<Page>(), PAGE_SIZE);
        assert_eq!(std::mem::align_of::<Page>(), 64);
    }

    #[test]
    fn test_new_page_is_valid() {
        let page = Page::new(PageType::Data, 42);
        assert_eq!(page.header().magic, PAGE_MAGIC);
        assert_eq!(page.header().page_id, 42);
        assert_eq!(page.header().page_type, PageType::Data as u8);
        assert!(page.verify_checksum().is_ok());
    }

    #[test]
    fn test_checksum_detects_corruption() {
        let mut page = Page::new(PageType::Data, 1);
        // Corrupt a byte in the body
        page.data[HEADER_SIZE + 10] ^= 0xFF;
        assert!(page.verify_checksum().is_err());
    }

    #[test]
    fn test_invalid_magic_is_rejected() {
        let mut bytes = [0u8; PAGE_SIZE];
        // Incorrect magic
        bytes[0..8].copy_from_slice(&0xDEADBEEFu64.to_ne_bytes());
        assert!(Page::from_bytes(bytes).is_err());
    }

    #[test]
    fn test_from_bytes_roundtrip() {
        let page = Page::new(PageType::Index, 99);
        let bytes = *page.as_bytes();
        let page2 = Page::from_bytes(bytes).expect("roundtrip must be valid");
        assert_eq!(page2.header().page_id, 99);
        assert_eq!(page2.header().page_type, PageType::Index as u8);
    }

    #[test]
    fn test_update_checksum_after_body_write() {
        let mut page = Page::new(PageType::Data, 5);
        // Write to the body without updating checksum → invalid
        page.data[HEADER_SIZE] = 0xAB;
        assert!(page.verify_checksum().is_err());
        // Update checksum → valid
        page.update_checksum();
        assert!(page.verify_checksum().is_ok());
    }

    #[test]
    fn test_page_types_roundtrip() {
        for (byte, expected) in [
            (0u8, PageType::Meta),
            (1, PageType::Data),
            (2, PageType::Index),
            (3, PageType::Overflow),
            (4, PageType::Free),
        ] {
            assert_eq!(PageType::try_from(byte).unwrap(), expected);
        }
        assert!(PageType::try_from(99).is_err());
    }
}
