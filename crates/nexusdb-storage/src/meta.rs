//! Meta page (page 0) reader/writer for database-level metadata.
//!
//! ## Layout of page 0 body
//!
//! ```text
//! body[0..8]   magic: u64 LE         — "NEXUSDB\0" (written by MmapStorage)
//! body[8..16]  page_count: u64 LE    — total pages in the file
//! body[16..24] checkpoint_lsn: u64 LE — LSN of last successful checkpoint (0 = none)
//! body[24..]   reserved for future use
//! ```
//!
//! Only `checkpoint_lsn` is managed here. `magic` and `page_count` are
//! managed by `MmapStorage` directly.

use nexusdb_core::error::DbError;

use crate::{
    engine::StorageEngine,
    page::{Page, HEADER_SIZE},
};

/// Byte offset of `checkpoint_lsn` within the page body (not the full page).
pub const CHECKPOINT_LSN_BODY_OFFSET: usize = 16;

/// Byte offset of `checkpoint_lsn` within the full page bytes (body starts at HEADER_SIZE).
const CHECKPOINT_LSN_PAGE_OFFSET: usize = HEADER_SIZE + CHECKPOINT_LSN_BODY_OFFSET;

const _: () = assert!(
    CHECKPOINT_LSN_PAGE_OFFSET + 8 <= crate::page::PAGE_SIZE,
    "checkpoint_lsn field must fit within page 0"
);

/// Reads the LSN of the last successful checkpoint from the meta page (page 0).
///
/// Returns `0` if the database has never been checkpointed.
pub fn read_checkpoint_lsn(storage: &dyn StorageEngine) -> Result<u64, DbError> {
    let page = storage.read_page(0)?;
    let raw = page.as_bytes();
    Ok(u64::from_le_bytes([
        raw[CHECKPOINT_LSN_PAGE_OFFSET],
        raw[CHECKPOINT_LSN_PAGE_OFFSET + 1],
        raw[CHECKPOINT_LSN_PAGE_OFFSET + 2],
        raw[CHECKPOINT_LSN_PAGE_OFFSET + 3],
        raw[CHECKPOINT_LSN_PAGE_OFFSET + 4],
        raw[CHECKPOINT_LSN_PAGE_OFFSET + 5],
        raw[CHECKPOINT_LSN_PAGE_OFFSET + 6],
        raw[CHECKPOINT_LSN_PAGE_OFFSET + 7],
    ]))
}

/// Writes `lsn` into the `checkpoint_lsn` field of the meta page (page 0).
///
/// Caller must flush storage after this call to guarantee durability.
pub fn write_checkpoint_lsn(storage: &mut dyn StorageEngine, lsn: u64) -> Result<(), DbError> {
    // Read → modify → write (StorageEngine has no read_page_mut).
    let bytes = *storage.read_page(0)?.as_bytes();
    let mut page = Page::from_bytes(bytes)?;
    page.as_bytes_mut()[CHECKPOINT_LSN_PAGE_OFFSET..CHECKPOINT_LSN_PAGE_OFFSET + 8]
        .copy_from_slice(&lsn.to_le_bytes());
    page.update_checksum();
    storage.write_page(0, &page)?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryStorage, PageType};

    fn storage_with_meta() -> MemoryStorage {
        // MemoryStorage::new() already allocates page 0 as Meta.
        MemoryStorage::new()
    }

    #[test]
    fn test_fresh_db_checkpoint_lsn_is_zero() {
        let storage = storage_with_meta();
        assert_eq!(read_checkpoint_lsn(&storage).unwrap(), 0);
    }

    #[test]
    fn test_write_then_read_checkpoint_lsn() {
        let mut storage = storage_with_meta();
        write_checkpoint_lsn(&mut storage, 42).unwrap();
        assert_eq!(read_checkpoint_lsn(&storage).unwrap(), 42);
    }

    #[test]
    fn test_checkpoint_lsn_overwrites_previous() {
        let mut storage = storage_with_meta();
        write_checkpoint_lsn(&mut storage, 10).unwrap();
        write_checkpoint_lsn(&mut storage, 99).unwrap();
        assert_eq!(read_checkpoint_lsn(&storage).unwrap(), 99);
    }

    #[test]
    fn test_write_does_not_corrupt_other_meta_fields() {
        let mut storage = storage_with_meta();
        // page_count lives at body[8..16]; checkpoint_lsn at body[16..24].
        // Writing checkpoint_lsn must not touch page_count.
        let count_before = storage.page_count();
        write_checkpoint_lsn(&mut storage, 77).unwrap();
        assert_eq!(storage.page_count(), count_before);
        // Checksum must still be valid.
        assert!(storage.read_page(0).unwrap().verify_checksum().is_ok());
    }

    #[test]
    fn test_alloc_pages_do_not_corrupt_checkpoint_lsn() {
        let mut storage = storage_with_meta();
        write_checkpoint_lsn(&mut storage, 55).unwrap();
        // Allocate a page — this may update page_count in the meta page.
        storage.alloc_page(PageType::Data).unwrap();
        // checkpoint_lsn must be preserved.
        assert_eq!(read_checkpoint_lsn(&storage).unwrap(), 55);
    }
}
