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
use axiomdb_storage::HeapChain;
use axiomdb_storage::{
    read_meta_u32, read_meta_u64, write_catalog_header, write_meta_u32, write_meta_u64, Page,
    PageType, StorageEngine, CATALOG_COLUMNS_ROOT_BODY_OFFSET,
    CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET, CATALOG_DATABASES_ROOT_BODY_OFFSET,
    CATALOG_FOREIGN_KEYS_ROOT_BODY_OFFSET, CATALOG_INDEXES_ROOT_BODY_OFFSET,
    CATALOG_SCHEMA_VER_BODY_OFFSET, CATALOG_STATS_ROOT_BODY_OFFSET,
    CATALOG_TABLES_ROOT_BODY_OFFSET, CATALOG_TABLE_DATABASES_ROOT_BODY_OFFSET,
    NEXT_INDEX_ID_BODY_OFFSET, NEXT_TABLE_ID_BODY_OFFSET,
};

use crate::schema::{DatabaseDef, DEFAULT_DATABASE_NAME};

// ── CatalogPageIds ────────────────────────────────────────────────────────────

/// Root heap page IDs for the five system tables.
///
/// These are the starting points for [`CatalogReader`] and [`CatalogWriter`]
/// when scanning or inserting rows into `axiom_tables`, `axiom_columns`,
/// `axiom_indexes`, `axiom_constraints`, and `axiom_foreign_keys`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogPageIds {
    /// Root page of the `axiom_tables` heap.
    pub tables: u64,
    /// Root page of the `axiom_columns` heap.
    pub columns: u64,
    /// Root page of the `axiom_indexes` heap.
    pub indexes: u64,
    /// Root page of the `axiom_constraints` heap (Phase 4.22b).
    /// Zero on databases created before Phase 4.22b; lazily initialized on
    /// first use via `CatalogBootstrap::ensure_constraints_root()`.
    pub constraints: u64,
    /// Root page of the `axiom_foreign_keys` heap (Phase 6.5).
    /// Zero on databases created before Phase 6.5; lazily initialized on
    /// first use via `CatalogBootstrap::ensure_fk_root()`.
    pub foreign_keys: u64,
    /// Root page of the `axiom_stats` heap (Phase 6.10).
    /// Zero on pre-6.10 databases; lazily initialized on first write.
    /// `list_stats` returns empty vec when zero (no stats yet).
    pub stats: u64,
    /// Root page of the `axiom_databases` heap (Phase 22b.3a).
    /// Zero on legacy databases before upgrade initialization.
    pub databases: u64,
    /// Root page of the `axiom_table_databases` heap (Phase 22b.3a).
    /// Zero on legacy databases before upgrade initialization.
    pub table_databases: u64,
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
            return Self::ensure_database_roots(storage);
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

        // Also allocate the constraints root (Phase 4.22b).
        let constraints_root = storage.alloc_page(PageType::Data)?;
        let constraints_page = Page::new(PageType::Data, constraints_root);
        storage.write_page(constraints_root, &constraints_page)?;
        write_meta_u64(
            storage,
            CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET,
            constraints_root,
        )?;

        // Initialize auto-increment sequences: next available ID = 1.
        write_meta_u32(storage, NEXT_TABLE_ID_BODY_OFFSET, 1)?;
        write_meta_u32(storage, NEXT_INDEX_ID_BODY_OFFSET, 1)?;

        // Allocate the foreign keys root (Phase 6.5).
        let fk_root = storage.alloc_page(PageType::Data)?;
        let fk_page = Page::new(PageType::Data, fk_root);
        storage.write_page(fk_root, &fk_page)?;
        write_meta_u64(storage, CATALOG_FOREIGN_KEYS_ROOT_BODY_OFFSET, fk_root)?;

        // Allocate the stats root (Phase 6.10).
        let stats_root = storage.alloc_page(PageType::Data)?;
        let stats_page = Page::new(PageType::Data, stats_root);
        storage.write_page(stats_root, &stats_page)?;
        write_meta_u64(storage, CATALOG_STATS_ROOT_BODY_OFFSET, stats_root)?;

        // Allocate the databases root (Phase 22b.3a).
        let databases_root = storage.alloc_page(PageType::Data)?;
        let databases_page = Page::new(PageType::Data, databases_root);
        storage.write_page(databases_root, &databases_page)?;
        write_meta_u64(storage, CATALOG_DATABASES_ROOT_BODY_OFFSET, databases_root)?;

        // Allocate the table-database bindings root (Phase 22b.3a).
        let table_databases_root = storage.alloc_page(PageType::Data)?;
        let table_databases_page = Page::new(PageType::Data, table_databases_root);
        storage.write_page(table_databases_root, &table_databases_page)?;
        write_meta_u64(
            storage,
            CATALOG_TABLE_DATABASES_ROOT_BODY_OFFSET,
            table_databases_root,
        )?;

        // Seed the default logical database so fresh databases and upgraded
        // ones share the same catalog shape.
        let default_db = DatabaseDef {
            name: DEFAULT_DATABASE_NAME.to_string(),
        };
        let _ = HeapChain::insert(storage, databases_root, &default_db.to_bytes(), 0)?;

        storage.flush()?;

        Ok(CatalogPageIds {
            tables: tables_root,
            columns: columns_root,
            indexes: indexes_root,
            constraints: constraints_root,
            foreign_keys: fk_root,
            stats: stats_root,
            databases: databases_root,
            table_databases: table_databases_root,
        })
    }

    /// Reads the catalog page IDs from the meta page.
    ///
    /// For `constraints`: if the value is 0 (database created before Phase
    /// 4.22b), returns 0 — callers must call [`ensure_constraints_root`]
    /// before writing to the constraints heap.
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
        let constraints = read_meta_u64(storage, CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET)?;
        let foreign_keys = read_meta_u64(storage, CATALOG_FOREIGN_KEYS_ROOT_BODY_OFFSET)?;
        let stats = read_meta_u64(storage, CATALOG_STATS_ROOT_BODY_OFFSET)?;
        let databases = read_meta_u64(storage, CATALOG_DATABASES_ROOT_BODY_OFFSET)?;
        let table_databases = read_meta_u64(storage, CATALOG_TABLE_DATABASES_ROOT_BODY_OFFSET)?;
        Ok(CatalogPageIds {
            tables,
            columns,
            indexes,
            constraints,
            foreign_keys,
            stats,
            databases,
            table_databases,
        })
    }

    /// Ensures the Phase 22b.3a database catalog roots exist.
    ///
    /// Used when opening a legacy database created before databases became
    /// first-class catalog objects. Idempotent.
    pub fn ensure_database_roots(
        storage: &mut dyn StorageEngine,
    ) -> Result<CatalogPageIds, DbError> {
        let mut ids = Self::page_ids(storage)?;

        if ids.databases == 0 {
            let root = storage.alloc_page(PageType::Data)?;
            let page = Page::new(PageType::Data, root);
            storage.write_page(root, &page)?;
            write_meta_u64(storage, CATALOG_DATABASES_ROOT_BODY_OFFSET, root)?;

            let default_db = DatabaseDef {
                name: DEFAULT_DATABASE_NAME.to_string(),
            };
            let _ = HeapChain::insert(storage, root, &default_db.to_bytes(), 0)?;
            ids.databases = root;
        }

        if ids.table_databases == 0 {
            let root = storage.alloc_page(PageType::Data)?;
            let page = Page::new(PageType::Data, root);
            storage.write_page(root, &page)?;
            write_meta_u64(storage, CATALOG_TABLE_DATABASES_ROOT_BODY_OFFSET, root)?;
            ids.table_databases = root;
        }

        if ids.databases == 0 || ids.table_databases == 0 {
            return Err(DbError::Internal {
                message: "database catalog roots not initialized".into(),
            });
        }

        storage.flush()?;
        Ok(ids)
    }

    /// Ensures the `axiom_constraints` root page exists.
    ///
    /// If the database was created before Phase 4.22b, the constraints root is
    /// 0. This method allocates and persists it on first call. Idempotent.
    ///
    /// Returns the (possibly newly allocated) constraints root page ID.
    pub fn ensure_constraints_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError> {
        let root = read_meta_u64(storage, CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET)?;
        if root != 0 {
            return Ok(root);
        }
        let new_root = storage.alloc_page(PageType::Data)?;
        let page = Page::new(PageType::Data, new_root);
        storage.write_page(new_root, &page)?;
        write_meta_u64(storage, CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET, new_root)?;
        storage.flush()?;
        Ok(new_root)
    }

    /// Ensures the `axiom_stats` root page exists (Phase 6.10).
    ///
    /// Lazily initialized on first write. Pre-6.10 databases return empty stats.
    pub fn ensure_stats_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError> {
        let root = read_meta_u64(storage, CATALOG_STATS_ROOT_BODY_OFFSET)?;
        if root != 0 {
            return Ok(root);
        }
        let new_root = storage.alloc_page(PageType::Data)?;
        let page = Page::new(PageType::Data, new_root);
        storage.write_page(new_root, &page)?;
        write_meta_u64(storage, CATALOG_STATS_ROOT_BODY_OFFSET, new_root)?;
        storage.flush()?;
        Ok(new_root)
    }

    /// Ensures the `axiom_foreign_keys` root page exists (Phase 6.5).
    ///
    /// If the database was created before Phase 6.5, the FK root is 0.
    /// This method allocates and persists it on first call. Idempotent.
    ///
    /// Returns the (possibly newly allocated) FK root page ID.
    pub fn ensure_fk_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError> {
        let root = read_meta_u64(storage, CATALOG_FOREIGN_KEYS_ROOT_BODY_OFFSET)?;
        if root != 0 {
            return Ok(root);
        }
        let new_root = storage.alloc_page(PageType::Data)?;
        let page = Page::new(PageType::Data, new_root);
        storage.write_page(new_root, &page)?;
        write_meta_u64(storage, CATALOG_FOREIGN_KEYS_ROOT_BODY_OFFSET, new_root)?;
        storage.flush()?;
        Ok(new_root)
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
