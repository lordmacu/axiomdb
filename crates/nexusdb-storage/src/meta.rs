//! Meta page (page 0) reader/writer for database-level metadata.
//!
//! ## Layout of page 0 body
//!
//! ```text
//! body[0..8]   db_magic: u64 LE        — "NEXUSDB\1" (MmapStorage)
//! body[8..12]  db_version: u32 LE      — MmapStorage version
//! body[12..16] _pad: u32
//! body[16..24] page_count: u64 LE      — MmapStorage manages this
//! body[24..32] checkpoint_lsn: u64 LE  — LSN of last checkpoint (0 = none)
//! body[32..40] catalog_tables_root: u64 LE  — nexus_tables heap root page (0 = uninit)
//! body[40..48] catalog_columns_root: u64 LE — nexus_columns heap root page (0 = uninit)
//! body[48..56] catalog_indexes_root: u64 LE — nexus_indexes heap root page (0 = uninit)
//! body[56..60] catalog_schema_ver: u32 LE   — 0 = uninitialized, 1 = v1
//! body[60..64] _catalog_pad: u32
//! ```

use nexusdb_core::error::DbError;

use crate::{
    engine::StorageEngine,
    page::{Page, HEADER_SIZE},
};

/// Byte offset of `checkpoint_lsn` within the page body (not the full page).
///
/// ## Meta page body layout (MmapStorage DbFileMeta)
/// ```text
/// body[0..8]   db_magic: u64
/// body[8..12]  version: u32
/// body[12..16] _pad: u32
/// body[16..24] page_count: u64   ← MmapStorage uses this field
/// body[24..32] checkpoint_lsn    ← we start here, safely after page_count
/// ```
pub const CHECKPOINT_LSN_BODY_OFFSET: usize = 24;

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

// ── Catalog header ────────────────────────────────────────────────────────────

/// body offset of `catalog_tables_root` — root heap page for `nexus_tables`.
pub const CATALOG_TABLES_ROOT_BODY_OFFSET: usize = 32;
/// body offset of `catalog_columns_root` — root heap page for `nexus_columns`.
pub const CATALOG_COLUMNS_ROOT_BODY_OFFSET: usize = 40;
/// body offset of `catalog_indexes_root` — root heap page for `nexus_indexes`.
pub const CATALOG_INDEXES_ROOT_BODY_OFFSET: usize = 48;
/// body offset of `catalog_schema_ver: u32` — 0 = uninitialized, 1 = v1.
pub const CATALOG_SCHEMA_VER_BODY_OFFSET: usize = 56;

const _: () = assert!(
    HEADER_SIZE + CATALOG_SCHEMA_VER_BODY_OFFSET + 4 <= crate::page::PAGE_SIZE,
    "catalog header must fit within page 0"
);

/// Reads a single `u64` from the meta page at `body_offset`.
pub fn read_meta_u64(storage: &dyn StorageEngine, body_offset: usize) -> Result<u64, DbError> {
    let page = storage.read_page(0)?;
    let raw = page.as_bytes();
    let off = HEADER_SIZE + body_offset;
    Ok(u64::from_le_bytes([
        raw[off],
        raw[off + 1],
        raw[off + 2],
        raw[off + 3],
        raw[off + 4],
        raw[off + 5],
        raw[off + 6],
        raw[off + 7],
    ]))
}

/// Reads a single `u32` from the meta page at `body_offset`.
pub fn read_meta_u32(storage: &dyn StorageEngine, body_offset: usize) -> Result<u32, DbError> {
    let page = storage.read_page(0)?;
    let raw = page.as_bytes();
    let off = HEADER_SIZE + body_offset;
    Ok(u32::from_le_bytes([
        raw[off],
        raw[off + 1],
        raw[off + 2],
        raw[off + 3],
    ]))
}

/// Writes the entire catalog header block to the meta page in a single `write_page` call.
///
/// Caller must flush storage afterward to guarantee durability.
pub fn write_catalog_header(
    storage: &mut dyn StorageEngine,
    tables_root: u64,
    columns_root: u64,
    indexes_root: u64,
    schema_ver: u32,
) -> Result<(), DbError> {
    let bytes = *storage.read_page(0)?.as_bytes();
    let mut page = Page::from_bytes(bytes)?;
    let raw = page.as_bytes_mut();

    let b = HEADER_SIZE;
    raw[b + 32..b + 40].copy_from_slice(&tables_root.to_le_bytes());
    raw[b + 40..b + 48].copy_from_slice(&columns_root.to_le_bytes());
    raw[b + 48..b + 56].copy_from_slice(&indexes_root.to_le_bytes());
    raw[b + 56..b + 60].copy_from_slice(&schema_ver.to_le_bytes());

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
        // page_count lives at body[16..24] (DbFileMeta layout).
        // checkpoint_lsn lives at body[24..32] — must not overlap.
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
