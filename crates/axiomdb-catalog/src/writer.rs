//! CatalogWriter — DDL write operations over the three system catalog tables.
//!
//! ## Responsibilities
//!
//! - Insert rows into `axiom_tables`, `axiom_columns`, `axiom_indexes` heap pages.
//! - Delete rows (MVCC: stamps `txn_id_deleted`; rows remain for older snapshots).
//! - WAL-log every mutation via [`TxnManager`] for crash recovery.
//! - Allocate monotonically increasing `TableId` and `IndexId` from the meta page.
//!
//! ## Usage
//!
//! The caller is responsible for `begin()` / `commit()` / `rollback()` on the
//! `TxnManager`. `CatalogWriter` only calls `record_insert` / `record_delete`
//! which require an active transaction.
//!
//! ```rust,ignore
//! txn.begin()?;
//! let mut writer = CatalogWriter::new(&mut storage, &mut txn)?;
//! let table_id = writer.create_table("public", "users")?;
//! writer.create_column(ColumnDef { table_id, col_idx: 0, name: "id".into(),
//!     col_type: ColumnType::BigInt, nullable: false })?;
//! txn.commit()?;
//! ```
//!
//! ## WAL table_id convention for system tables
//!
//! User `TableId`s start at 1 and grow upward. System tables use the top of
//! the `u32` range to avoid collisions:
//!
//! ```text
//! SYSTEM_TABLE_TABLES  = u32::MAX - 2  (axiom_tables)
//! SYSTEM_TABLE_COLUMNS = u32::MAX - 1  (axiom_columns)
//! SYSTEM_TABLE_INDEXES = u32::MAX      (axiom_indexes)
//! ```

use std::sync::Arc;

use axiomdb_core::error::DbError;
use axiomdb_storage::{alloc_index_id, alloc_table_id, HeapChain, Page, PageType, StorageEngine};
use axiomdb_wal::TxnManager;

use crate::{
    bootstrap::{CatalogBootstrap, CatalogPageIds},
    notifier::{CatalogChangeNotifier, SchemaChangeEvent, SchemaChangeKind},
    schema::{ColumnDef, IndexDef, TableDef, TableId},
};

// ── WAL table_id constants for system tables ──────────────────────────────────

/// WAL `table_id` used for inserts/deletes into `axiom_tables`.
pub const SYSTEM_TABLE_TABLES: u32 = u32::MAX - 2;
/// WAL `table_id` used for inserts/deletes into `axiom_columns`.
pub const SYSTEM_TABLE_COLUMNS: u32 = u32::MAX - 1;
/// WAL `table_id` used for inserts/deletes into `axiom_indexes`.
pub const SYSTEM_TABLE_INDEXES: u32 = u32::MAX;

// ── CatalogWriter ─────────────────────────────────────────────────────────────

/// DDL write access to the three system catalog tables.
///
/// Requires an active transaction in the `TxnManager`. All heap mutations
/// are WAL-logged for crash recovery and MVCC correctness.
///
/// Optionally carries a [`CatalogChangeNotifier`] that receives a
/// [`SchemaChangeEvent`] after each successful DDL mutation. Set it via
/// [`with_notifier`]. Without a notifier the writer behaves identically to
/// before — the notifier is purely additive.
///
/// [`with_notifier`]: CatalogWriter::with_notifier
pub struct CatalogWriter<'a> {
    storage: &'a mut dyn StorageEngine,
    txn: &'a mut TxnManager,
    page_ids: CatalogPageIds,
    notifier: Option<Arc<CatalogChangeNotifier>>,
}

impl<'a> CatalogWriter<'a> {
    /// Creates a new `CatalogWriter` without a notifier.
    ///
    /// # Errors
    /// - [`DbError::CatalogNotInitialized`] if [`CatalogBootstrap::init`] has not been called.
    pub fn new(
        storage: &'a mut dyn StorageEngine,
        txn: &'a mut TxnManager,
    ) -> Result<Self, DbError> {
        let page_ids = CatalogBootstrap::page_ids(storage)?;
        Ok(Self {
            storage,
            txn,
            page_ids,
            notifier: None,
        })
    }

    /// Attaches a [`CatalogChangeNotifier`].
    ///
    /// After this call, every DDL operation fires the appropriate
    /// [`SchemaChangeEvent`] on the notifier immediately after the heap
    /// mutation succeeds (before commit — see notifier module docs for
    /// firing semantics).
    ///
    /// Returns `self` for builder-style chaining:
    /// ```rust,ignore
    /// let writer = CatalogWriter::new(&mut storage, &mut txn)?
    ///     .with_notifier(Arc::clone(&notifier));
    /// ```
    pub fn with_notifier(mut self, notifier: Arc<CatalogChangeNotifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    // ── Internal: fire notification ───────────────────────────────────────────

    /// Fires a schema change event on the notifier, if one is set.
    ///
    /// Called after every successful DDL mutation. `txn_id` is taken from the
    /// active transaction; falls back to 0 if somehow called outside one
    /// (should not happen — DDL methods verify active txn before calling this).
    fn fire(&self, kind: SchemaChangeKind) {
        if let Some(n) = &self.notifier {
            let txn_id = self.txn.active_txn_id().unwrap_or(0);
            n.notify(&SchemaChangeEvent { kind, txn_id });
        }
    }

    // ── Table operations ──────────────────────────────────────────────────────

    /// Allocates a new `TableId`, initializes a heap root page for user row data,
    /// and inserts a row into `axiom_tables`.
    ///
    /// The row is WAL-logged as an Insert entry with
    /// `table_id = SYSTEM_TABLE_TABLES` and key = `allocated_table_id` as LE bytes.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::CatalogNotInitialized`] if sequences have not been seeded.
    /// - [`DbError::SequenceOverflow`] if the table ID space is exhausted.
    pub fn create_table(&mut self, schema: &str, name: &str) -> Result<TableId, DbError> {
        let table_id = alloc_table_id(self.storage)?;

        // Allocate and initialize the heap root page for this table's user data.
        // This page becomes the root of the HeapChain used by TableEngine.
        let data_root_page_id = self.storage.alloc_page(PageType::Data)?;
        let root_page = Page::new(PageType::Data, data_root_page_id);
        self.storage.write_page(data_root_page_id, &root_page)?;

        let def = TableDef {
            id: table_id,
            data_root_page_id,
            schema_name: schema.to_string(),
            table_name: name.to_string(),
        };
        let data = def.to_bytes();

        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let (page_id, slot_id) =
            HeapChain::insert(self.storage, self.page_ids.tables, &data, txn_id)?;

        let key = table_id.to_le_bytes();
        self.txn
            .record_insert(SYSTEM_TABLE_TABLES, &key, &data, page_id, slot_id)?;

        self.fire(SchemaChangeKind::TableCreated { table_id });
        Ok(table_id)
    }

    // ── Column operations ─────────────────────────────────────────────────────

    /// Inserts a column definition row into `axiom_columns`.
    ///
    /// The caller is responsible for setting `col_idx` to the correct 0-based
    /// position. No uniqueness check is performed here (enforced by the executor).
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn create_column(&mut self, def: ColumnDef) -> Result<(), DbError> {
        let data = def.to_bytes();

        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let (page_id, slot_id) =
            HeapChain::insert(self.storage, self.page_ids.columns, &data, txn_id)?;

        // Key: (table_id, col_idx) as 6 bytes LE for WAL lookup.
        let mut key = [0u8; 6];
        key[0..4].copy_from_slice(&def.table_id.to_le_bytes());
        key[4..6].copy_from_slice(&def.col_idx.to_le_bytes());
        self.txn
            .record_insert(SYSTEM_TABLE_COLUMNS, &key, &data, page_id, slot_id)?;

        Ok(())
    }

    // ── Index operations ──────────────────────────────────────────────────────

    /// Allocates a new `index_id` and inserts an index definition row into
    /// `axiom_indexes`.
    ///
    /// The `def.index_id` field is ignored — the writer allocates a fresh ID
    /// from the meta page sequence and stores it in the row.
    ///
    /// Returns the allocated `index_id`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::SequenceOverflow`] if the index ID space is exhausted.
    pub fn create_index(&mut self, def: IndexDef) -> Result<u32, DbError> {
        let index_id = alloc_index_id(self.storage)?;

        // Build the row with the allocated index_id.
        let row = IndexDef { index_id, ..def };
        let data = row.to_bytes();

        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let (page_id, slot_id) =
            HeapChain::insert(self.storage, self.page_ids.indexes, &data, txn_id)?;

        let key = index_id.to_le_bytes();
        self.txn
            .record_insert(SYSTEM_TABLE_INDEXES, &key, &data, page_id, slot_id)?;

        self.fire(SchemaChangeKind::IndexCreated {
            index_id,
            table_id: row.table_id,
        });
        Ok(index_id)
    }

    // ── Drop operations ───────────────────────────────────────────────────────

    /// Marks all rows for `table_id` as deleted in `axiom_tables`,
    /// `axiom_columns`, and `axiom_indexes`.
    ///
    /// Uses `active_snapshot()` to see the writer's own uncommitted inserts,
    /// so a table created and immediately dropped in the same transaction is
    /// handled correctly.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn delete_table(&mut self, table_id: TableId) -> Result<(), DbError> {
        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let snap = self.txn.active_snapshot()?;

        // Collect rows first (releases the immutable borrow on storage).
        let table_rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;
        let col_rows = HeapChain::scan_visible(self.storage, self.page_ids.columns, snap)?;
        let idx_rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, snap)?;

        // Delete matching rows from axiom_tables.
        for (page_id, slot_id, data) in table_rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn
                    .record_delete(SYSTEM_TABLE_TABLES, &key, &data, page_id, slot_id)?;
            }
        }

        // Delete matching columns from axiom_columns.
        for (page_id, slot_id, data) in col_rows {
            let (def, _) = ColumnDef::from_bytes(&data)?;
            if def.table_id == table_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn
                    .record_delete(SYSTEM_TABLE_COLUMNS, &key, &data, page_id, slot_id)?;
            }
        }

        // Delete matching indexes from axiom_indexes; collect dropped index_ids for events.
        let mut dropped_index_ids: Vec<u32> = Vec::new();
        for (page_id, slot_id, data) in idx_rows {
            let (def, _) = IndexDef::from_bytes(&data)?;
            if def.table_id == table_id {
                dropped_index_ids.push(def.index_id);
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn
                    .record_delete(SYSTEM_TABLE_INDEXES, &key, &data, page_id, slot_id)?;
            }
        }

        // Fire notifications after all mutations succeed.
        self.fire(SchemaChangeKind::TableDropped { table_id });
        for index_id in dropped_index_ids {
            self.fire(SchemaChangeKind::IndexDropped { index_id, table_id });
        }

        Ok(())
    }

    /// Marks the index row with `index_id` as deleted in `axiom_indexes`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::CatalogIndexNotFound`] if no visible index with that ID exists.
    pub fn delete_index(&mut self, index_id: u32) -> Result<(), DbError> {
        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let snap = self.txn.active_snapshot()?;

        let rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = IndexDef::from_bytes(&data)?;
            if def.index_id == index_id {
                let table_id = def.table_id;
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = index_id.to_le_bytes();
                self.txn
                    .record_delete(SYSTEM_TABLE_INDEXES, &key, &data, page_id, slot_id)?;
                self.fire(SchemaChangeKind::IndexDropped { index_id, table_id });
                return Ok(());
            }
        }

        Err(DbError::CatalogIndexNotFound { index_id })
    }
}
