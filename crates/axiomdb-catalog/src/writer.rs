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
use axiomdb_storage::{
    alloc_constraint_id, alloc_fk_id, alloc_index_id, alloc_table_id, HeapChain, Page, PageType,
    StorageEngine,
};
use axiomdb_wal::TxnManager;

use crate::{
    bootstrap::{CatalogBootstrap, CatalogPageIds},
    notifier::{CatalogChangeNotifier, SchemaChangeEvent, SchemaChangeKind},
    schema::{ColumnDef, ConstraintDef, FkDef, IndexDef, StatsDef, TableDef, TableId},
};

// ── WAL table_id constants for system tables ──────────────────────────────────

/// WAL `table_id` used for inserts/deletes into `axiom_tables`.
pub const SYSTEM_TABLE_TABLES: u32 = u32::MAX - 2;
/// WAL `table_id` used for inserts/deletes into `axiom_columns`.
pub const SYSTEM_TABLE_COLUMNS: u32 = u32::MAX - 1;
/// WAL `table_id` used for inserts/deletes into `axiom_indexes`.
pub const SYSTEM_TABLE_INDEXES: u32 = u32::MAX;
/// WAL `table_id` used for inserts/deletes into `axiom_constraints`.
pub const SYSTEM_TABLE_CONSTRAINTS: u32 = u32::MAX - 3;
/// WAL `table_id` used for inserts/deletes into `axiom_foreign_keys` (Phase 6.5).
pub const SYSTEM_TABLE_FOREIGN_KEYS: u32 = u32::MAX - 4;
/// WAL `table_id` used for inserts/deletes into `axiom_stats` (Phase 6.10).
pub const SYSTEM_TABLE_STATS: u32 = u32::MAX - 5;

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

    /// Marks the column row with `(table_id, col_idx)` as deleted in `axiom_columns`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::Internal`] if the column row is not found (caller must validate first).
    pub fn delete_column(&mut self, table_id: TableId, col_idx: u16) -> Result<(), DbError> {
        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let snap = self.txn.active_snapshot()?;
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.columns, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = ColumnDef::from_bytes(&data)?;
            if def.table_id == table_id && def.col_idx == col_idx {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let mut key = [0u8; 6];
                key[0..4].copy_from_slice(&table_id.to_le_bytes());
                key[4..6].copy_from_slice(&col_idx.to_le_bytes());
                self.txn
                    .record_delete(SYSTEM_TABLE_COLUMNS, &key, &data, page_id, slot_id)?;
                return Ok(());
            }
        }
        Err(DbError::Internal {
            message: format!("delete_column: col_idx={col_idx} not found for table_id={table_id}"),
        })
    }

    /// Renames a column by deleting the old catalog row and inserting a new one.
    ///
    /// All other fields (`col_type`, `nullable`, `auto_increment`, `col_idx`) are preserved.
    pub fn rename_column(
        &mut self,
        table_id: TableId,
        col_idx: u16,
        new_name: String,
    ) -> Result<(), DbError> {
        let snap = self.txn.active_snapshot()?;
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.columns, snap)?;

        // Find and remember the old ColumnDef.
        let old_def = rows
            .into_iter()
            .find_map(|(_, _, data)| {
                ColumnDef::from_bytes(&data).ok().and_then(|(def, _)| {
                    if def.table_id == table_id && def.col_idx == col_idx {
                        Some(def)
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| DbError::Internal {
                message: format!(
                    "rename_column: col_idx={col_idx} not found for table_id={table_id}"
                ),
            })?;

        self.delete_column(table_id, col_idx)?;
        self.create_column(ColumnDef {
            name: new_name,
            ..old_def
        })?;
        Ok(())
    }

    /// Renames a table by replacing its `TableDef` row in the catalog.
    ///
    /// The `table_id` and `data_root_page_id` are preserved.
    pub fn rename_table(
        &mut self,
        table_id: TableId,
        new_name: String,
        schema: &str,
    ) -> Result<(), DbError> {
        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let snap = self.txn.active_snapshot()?;
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                // Delete old row.
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn
                    .record_delete(SYSTEM_TABLE_TABLES, &key, &data, page_id, slot_id)?;

                // Insert new row with updated name.
                let new_def = TableDef {
                    table_name: new_name,
                    schema_name: schema.to_string(),
                    ..def
                };
                let new_data = new_def.to_bytes();
                let (pg2, sl2) =
                    HeapChain::insert(self.storage, self.page_ids.tables, &new_data, txn_id)?;
                self.txn
                    .record_insert(SYSTEM_TABLE_TABLES, &key, &new_data, pg2, sl2)?;
                return Ok(());
            }
        }
        Err(DbError::Internal {
            message: format!("rename_table: table_id={table_id} not found"),
        })
    }

    /// Replaces the `data_root_page_id` of a table in `axiom_tables`.
    ///
    /// Used by the bulk-empty fast path (Phase 5.16) to rotate the heap root
    /// to a freshly-allocated empty page. All other `TableDef` fields are preserved.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::Internal`] if `table_id` is not found in `axiom_tables`.
    pub fn update_table_data_root(
        &mut self,
        table_id: TableId,
        new_root_page_id: u64,
    ) -> Result<(), DbError> {
        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let snap = self.txn.active_snapshot()?;
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                // Delete old row.
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn
                    .record_delete(SYSTEM_TABLE_TABLES, &key, &data, page_id, slot_id)?;

                // Insert new row with updated data_root_page_id.
                let new_def = TableDef {
                    data_root_page_id: new_root_page_id,
                    ..def
                };
                let new_data = new_def.to_bytes();
                let (pg2, sl2) =
                    HeapChain::insert(self.storage, self.page_ids.tables, &new_data, txn_id)?;
                self.txn
                    .record_insert(SYSTEM_TABLE_TABLES, &key, &new_data, pg2, sl2)?;
                return Ok(());
            }
        }
        Err(DbError::Internal {
            message: format!(
                "update_table_data_root: table_id={table_id} not found in axiom_tables"
            ),
        })
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

    /// Updates the `root_page_id` of an existing index.
    ///
    /// Called after a B-Tree root split during DML — the old catalog row is
    /// deleted and a new one is inserted with the updated `root_page_id`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::CatalogIndexNotFound`] if no visible index with that ID exists.
    pub fn update_index_root(&mut self, index_id: u32, new_root: u64) -> Result<(), DbError> {
        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let snap = self.txn.active_snapshot()?;

        let rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, snap)?;
        for (page_id, slot_id, data) in rows {
            let (def, _) = IndexDef::from_bytes(&data)?;
            if def.index_id == index_id {
                // Delete old row.
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = index_id.to_le_bytes();
                self.txn
                    .record_delete(SYSTEM_TABLE_INDEXES, &key, &data, page_id, slot_id)?;

                // Insert updated row.
                let updated = IndexDef {
                    root_page_id: new_root,
                    ..def
                };
                let new_data = updated.to_bytes();
                let (new_page_id, new_slot_id) =
                    HeapChain::insert(self.storage, self.page_ids.indexes, &new_data, txn_id)?;
                self.txn.record_insert(
                    SYSTEM_TABLE_INDEXES,
                    &key,
                    &new_data,
                    new_page_id,
                    new_slot_id,
                )?;
                return Ok(());
            }
        }
        Err(DbError::CatalogIndexNotFound { index_id })
    }

    // ── Constraint operations (Phase 4.22b) ───────────────────────────────────

    /// Allocates a new `constraint_id` and inserts a constraint definition row
    /// into `axiom_constraints`.
    ///
    /// Returns the allocated `constraint_id`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn create_constraint(&mut self, def: ConstraintDef) -> Result<u32, DbError> {
        let constraint_id = alloc_constraint_id(self.storage)?;
        let constraints_root = CatalogBootstrap::ensure_constraints_root(self.storage)?;

        let row = ConstraintDef {
            constraint_id,
            ..def
        };
        let data = row.to_bytes();

        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let (page_id, slot_id) = HeapChain::insert(self.storage, constraints_root, &data, txn_id)?;

        let key = constraint_id.to_le_bytes();
        self.txn
            .record_insert(SYSTEM_TABLE_CONSTRAINTS, &key, &data, page_id, slot_id)?;

        Ok(constraint_id)
    }

    /// MVCC-deletes the constraint row with `constraint_id` from `axiom_constraints`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - Returns `Ok(())` silently if the constraint is not found (idempotent).
    pub fn drop_constraint(&mut self, constraint_id: u32) -> Result<(), DbError> {
        let constraints_root = CatalogBootstrap::ensure_constraints_root(self.storage)?;
        let snap = self.txn.active_snapshot()?;
        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;

        let rids = crate::reader::CatalogReader::scan_constraints_root(
            self.storage,
            constraints_root,
            snap,
        )?;
        for (rid, data) in rids {
            if let Ok((def, _)) = ConstraintDef::from_bytes(&data) {
                if def.constraint_id == constraint_id {
                    let key = constraint_id.to_le_bytes();
                    axiomdb_storage::HeapChain::delete(
                        self.storage,
                        rid.page_id,
                        rid.slot_id,
                        txn_id,
                    )?;
                    self.txn.record_delete(
                        SYSTEM_TABLE_CONSTRAINTS,
                        &key,
                        &data,
                        rid.page_id,
                        rid.slot_id,
                    )?;
                    return Ok(());
                }
            }
        }
        Ok(()) // not found — idempotent
    }

    // ── FK operations (Phase 6.5) ─────────────────────────────────────────────

    /// Allocates a new `fk_id` and inserts a FK definition row into
    /// `axiom_foreign_keys`.
    ///
    /// Returns the allocated `fk_id`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn create_foreign_key(&mut self, def: FkDef) -> Result<u32, DbError> {
        let fk_id = alloc_fk_id(self.storage)?;
        let fk_root = CatalogBootstrap::ensure_fk_root(self.storage)?;

        let row = FkDef { fk_id, ..def };
        let data = row.to_bytes();

        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;
        let (page_id, slot_id) = HeapChain::insert(self.storage, fk_root, &data, txn_id)?;

        let key = fk_id.to_le_bytes();
        self.txn
            .record_insert(SYSTEM_TABLE_FOREIGN_KEYS, &key, &data, page_id, slot_id)?;

        Ok(fk_id)
    }

    /// MVCC-deletes the FK row with `fk_id` from `axiom_foreign_keys`.
    ///
    /// Returns `Ok(())` silently if the FK is not found (idempotent).
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn drop_foreign_key(&mut self, fk_id: u32) -> Result<(), DbError> {
        let fk_root = match CatalogBootstrap::page_ids(self.storage) {
            Ok(ids) if ids.foreign_keys != 0 => ids.foreign_keys,
            _ => return Ok(()), // no FK table yet — nothing to drop
        };
        let snap = self.txn.active_snapshot()?;
        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;

        let rows = crate::reader::CatalogReader::scan_fk_root(self.storage, fk_root, snap)?;
        for (rid, data) in rows {
            if let Ok((def, _)) = FkDef::from_bytes(&data) {
                if def.fk_id == fk_id {
                    let key = fk_id.to_le_bytes();
                    axiomdb_storage::HeapChain::delete(
                        self.storage,
                        rid.page_id,
                        rid.slot_id,
                        txn_id,
                    )?;
                    self.txn.record_delete(
                        SYSTEM_TABLE_FOREIGN_KEYS,
                        &key,
                        &data,
                        rid.page_id,
                        rid.slot_id,
                    )?;
                    return Ok(());
                }
            }
        }
        Ok(()) // not found — idempotent
    }

    // ── Statistics operations (Phase 6.10) ───────────────────────────────────

    /// Upserts per-column statistics into `axiom_stats`.
    ///
    /// If a row already exists for `(table_id, col_idx)`, it is MVCC-deleted
    /// and the new row is inserted. Both operations run within the same txn.
    ///
    /// Called at `CREATE INDEX` (bootstrap) and `ANALYZE` (refresh).
    /// Statistics writes are advisory — callers may ignore errors.
    pub fn upsert_stats(&mut self, def: StatsDef) -> Result<(), DbError> {
        let stats_root = CatalogBootstrap::ensure_stats_root(self.storage)?;
        let snap = self.txn.active_snapshot()?;
        let txn_id = self
            .txn
            .active_txn_id()
            .ok_or(DbError::NoActiveTransaction)?;

        // MVCC-delete existing row for this (table_id, col_idx) if present.
        let existing =
            crate::reader::CatalogReader::scan_stats_root(self.storage, stats_root, snap)?;
        for (rid, old_data) in existing {
            if let Ok((old_def, _)) = StatsDef::from_bytes(&old_data) {
                if old_def.table_id == def.table_id && old_def.col_idx == def.col_idx {
                    let key = [
                        old_def.table_id.to_le_bytes(),
                        [old_def.col_idx as u8, (old_def.col_idx >> 8) as u8, 0, 0],
                    ]
                    .concat();
                    axiomdb_storage::HeapChain::delete(
                        self.storage,
                        rid.page_id,
                        rid.slot_id,
                        txn_id,
                    )?;
                    self.txn.record_delete(
                        SYSTEM_TABLE_STATS,
                        &key,
                        &old_data,
                        rid.page_id,
                        rid.slot_id,
                    )?;
                    break;
                }
            }
        }

        // Insert new stats row.
        let data = def.to_bytes();
        let key = [
            def.table_id.to_le_bytes(),
            [def.col_idx as u8, (def.col_idx >> 8) as u8, 0, 0],
        ]
        .concat();
        let (page_id, slot_id) = HeapChain::insert(self.storage, stats_root, &data, txn_id)?;
        self.txn
            .record_insert(SYSTEM_TABLE_STATS, &key, &data, page_id, slot_id)?;
        Ok(())
    }
}
