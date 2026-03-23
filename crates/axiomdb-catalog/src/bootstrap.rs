//! Catalog bootstrap — allocates root heap pages for the three system tables
//! and records their page IDs in the database meta page.
//!
//! ## Ordering
//!
//! 1. [`CatalogBootstrap::init`] is called once after database creation.
//! 2. It allocates three [`PageType::Data`] pages and writes their IDs +
//!    `catalog_schema_ver = 1` to the meta page in a single `write_page` call.
//! 3. On every subsequent open, [`CatalogBootstrap::page_ids`] reads the IDs
//!    back from the meta page in O(1) without scanning any data pages.
//!
//! [`CatalogReader`] and [`CatalogWriter`] (Phase 3.12) use these page IDs as
//! the roots of the three system-table heaps.

use axiomdb_core::error::DbError;
use axiomdb_storage::{
    read_meta_u32, read_meta_u64, write_catalog_header, write_meta_u32, Page, PageType,
    StorageEngine, CATALOG_COLUMNS_ROOT_BODY_OFFSET, CATALOG_INDEXES_ROOT_BODY_OFFSET,
    CATALOG_SCHEMA_VER_BODY_OFFSET, CATALOG_TABLES_ROOT_BODY_OFFSET, NEXT_INDEX_ID_BODY_OFFSET,
    NEXT_TABLE_ID_BODY_OFFSET,
};

// ── CatalogPageIds ────────────────────────────────────────────────────────────

/// Root heap page IDs for the three system tables.
///
/// These are the starting points for [`CatalogReader`] and [`CatalogWriter`]
/// when scanning or inserting rows into `nexus_tables`, `nexus_columns`, and
/// `nexus_indexes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogPageIds {
    /// Root page of the `nexus_tables` heap.
    pub tables: u64,
    /// Root page of the `nexus_columns` heap.
    pub columns: u64,
    /// Root page of the `nexus_indexes` heap.
    pub indexes: u64,
}

// ── CatalogBootstrap ─────────────────────────────────────────────────────────

/// Stateless catalog bootstrap executor.
pub struct CatalogBootstrap;

impl CatalogBootstrap {
    /// Returns `true` if the catalog has been initialized on this database.
    ///
    /// Reads `catalog_schema_ver` from the meta page — any value `> 0` means
    /// the catalog was initialized by a previous call to [`init`].
    pub fn is_initialized(storage: &dyn StorageEngine) -> Result<bool, DbError> {
        let ver = read_meta_u32(storage, CATALOG_SCHEMA_VER_BODY_OFFSET)?;
        Ok(ver > 0)
    }

    /// Initializes the catalog by allocating root heap pages for the three
    /// system tables and writing their IDs to the meta page.
    ///
    /// **Idempotent**: if the catalog is already initialized, returns the
    /// existing [`CatalogPageIds`] without allocating new pages.
    ///
    /// Flushes storage after writing to guarantee durability.
    pub fn init(storage: &mut dyn StorageEngine) -> Result<CatalogPageIds, DbError> {
        if Self::is_initialized(storage)? {
            return Self::page_ids(storage);
        }

        // Allocate one empty heap root page per system table.
        let tables_root = storage.alloc_page(PageType::Data)?;
        let columns_root = storage.alloc_page(PageType::Data)?;
        let indexes_root = storage.alloc_page(PageType::Data)?;

        // Each root page starts as an empty heap page (valid header + checksum).
        // CatalogWriter (3.12) will insert rows into these pages.
        for &page_id in &[tables_root, columns_root, indexes_root] {
            let page = Page::new(PageType::Data, page_id);
            storage.write_page(page_id, &page)?;
        }

        // Atomically record the page IDs and schema version in the meta page.
        write_catalog_header(storage, tables_root, columns_root, indexes_root, 1)?;

        // Initialize auto-increment sequences: next available ID = 1.
        write_meta_u32(storage, NEXT_TABLE_ID_BODY_OFFSET, 1)?;
        write_meta_u32(storage, NEXT_INDEX_ID_BODY_OFFSET, 1)?;

        storage.flush()?;

        Ok(CatalogPageIds {
            tables: tables_root,
            columns: columns_root,
            indexes: indexes_root,
        })
    }

    /// Reads the catalog page IDs from the meta page.
    ///
    /// # Errors
    /// Returns [`DbError::CatalogNotInitialized`] if the catalog has not been
    /// set up yet (call [`init`] first).
    pub fn page_ids(storage: &dyn StorageEngine) -> Result<CatalogPageIds, DbError> {
        if !Self::is_initialized(storage)? {
            return Err(DbError::CatalogNotInitialized);
        }
        let tables = read_meta_u64(storage, CATALOG_TABLES_ROOT_BODY_OFFSET)?;
        let columns = read_meta_u64(storage, CATALOG_COLUMNS_ROOT_BODY_OFFSET)?;
        let indexes = read_meta_u64(storage, CATALOG_INDEXES_ROOT_BODY_OFFSET)?;
        Ok(CatalogPageIds {
            tables,
            columns,
            indexes,
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_storage::{MemoryStorage, MmapStorage};

    // ── MemoryStorage tests (fast) ────────────────────────────────────────────

    #[test]
    fn test_fresh_db_not_initialized() {
        let storage = MemoryStorage::new();
        assert!(!CatalogBootstrap::is_initialized(&storage).unwrap());
    }

    #[test]
    fn test_init_sets_schema_ver_1() {
        let mut storage = MemoryStorage::new();
        CatalogBootstrap::init(&mut storage).unwrap();
        assert!(CatalogBootstrap::is_initialized(&storage).unwrap());
    }

    #[test]
    fn test_init_allocates_three_pages() {
        let mut storage = MemoryStorage::new();
        let ids = CatalogBootstrap::init(&mut storage).unwrap();

        // All three page IDs must be distinct and non-zero.
        assert!(ids.tables > 0);
        assert!(ids.columns > 0);
        assert!(ids.indexes > 0);
        assert_ne!(ids.tables, ids.columns);
        assert_ne!(ids.columns, ids.indexes);
        assert_ne!(ids.tables, ids.indexes);
    }

    #[test]
    fn test_init_is_idempotent() {
        let mut storage = MemoryStorage::new();
        let ids1 = CatalogBootstrap::init(&mut storage).unwrap();
        let ids2 = CatalogBootstrap::init(&mut storage).unwrap(); // second call
        assert_eq!(ids1, ids2, "double-init must return the same page IDs");
    }

    #[test]
    fn test_page_ids_error_when_not_initialized() {
        let storage = MemoryStorage::new();
        let err = CatalogBootstrap::page_ids(&storage).unwrap_err();
        assert!(
            matches!(err, DbError::CatalogNotInitialized),
            "expected CatalogNotInitialized, got: {err}"
        );
    }

    #[test]
    fn test_page_ids_correct_after_init() {
        let mut storage = MemoryStorage::new();
        let init_ids = CatalogBootstrap::init(&mut storage).unwrap();
        let read_ids = CatalogBootstrap::page_ids(&storage).unwrap();
        assert_eq!(init_ids, read_ids);
    }

    // ── MmapStorage test (real I/O, persistence) ──────────────────────────────

    #[test]
    fn test_catalog_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // Session 1: init catalog.
        let init_ids = {
            let mut storage = MmapStorage::create(&db_path).unwrap();
            CatalogBootstrap::init(&mut storage).unwrap()
        };

        // Session 2: reopen and verify.
        {
            let storage = MmapStorage::open(&db_path).unwrap();
            assert!(CatalogBootstrap::is_initialized(&storage).unwrap());
            let read_ids = CatalogBootstrap::page_ids(&storage).unwrap();
            assert_eq!(read_ids, init_ids, "catalog page IDs must survive reopen");
        }
    }
}
