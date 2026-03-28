//! CatalogReader — snapshot-based read access to the three system catalog tables.
//!
//! All reads use [`HeapChain::scan_visible`] with a [`TransactionSnapshot`] for
//! MVCC-correct visibility. A snapshot taken before a DDL commit does not see
//! the new rows; a snapshot taken after does.
//!
//! Lookups are linear scans over the heap pages — O(n) in the number of catalog
//! rows. This is acceptable in Phase 3 (catalogs rarely exceed hundreds of rows).
//! Index-backed lookups are deferred to a later phase once the bootstrap cycle
//! can be resolved.

use axiomdb_core::{error::DbError, TransactionSnapshot};
use axiomdb_storage::{HeapChain, StorageEngine};

use axiomdb_core::RecordId;

use crate::{
    bootstrap::{CatalogBootstrap, CatalogPageIds},
    schema::{
        ColumnDef, ConstraintDef, DatabaseDef, FkDef, IndexDef, StatsDef, TableDatabaseDef,
        TableDef, TableId, DEFAULT_DATABASE_NAME,
    },
};

// ── CatalogReader ─────────────────────────────────────────────────────────────

/// Read-only access to the three system catalog tables with MVCC snapshot visibility.
pub struct CatalogReader<'a> {
    storage: &'a dyn StorageEngine,
    page_ids: CatalogPageIds,
    snapshot: TransactionSnapshot,
}

impl<'a> CatalogReader<'a> {
    /// Creates a new `CatalogReader` using the given snapshot for visibility.
    ///
    /// Use `TxnManager::snapshot()` for reads outside a transaction (sees all
    /// committed data), or `TxnManager::active_snapshot()` to read within an
    /// active transaction (includes the transaction's own uncommitted writes).
    ///
    /// # Errors
    /// - [`DbError::CatalogNotInitialized`] if [`CatalogBootstrap::init`] has not been called.
    pub fn new(
        storage: &'a dyn StorageEngine,
        snapshot: TransactionSnapshot,
    ) -> Result<Self, DbError> {
        let page_ids = CatalogBootstrap::page_ids(storage)?;
        Ok(Self {
            storage,
            page_ids,
            snapshot,
        })
    }

    // ── Table lookups ─────────────────────────────────────────────────────────

    /// Returns the first visible table matching `(schema_name, table_name)` in
    /// the default database namespace.
    ///
    /// Returns `None` if no such table is visible to the current snapshot.
    pub fn get_table(&mut self, schema: &str, name: &str) -> Result<Option<TableDef>, DbError> {
        self.get_table_in_database(DEFAULT_DATABASE_NAME, schema, name)
    }

    /// Returns the first visible table matching `(database, schema, table)`.
    pub fn get_table_in_database(
        &mut self,
        database: &str,
        schema: &str,
        name: &str,
    ) -> Result<Option<TableDef>, DbError> {
        let rows = HeapChain::scan_visible_ro(self.storage, self.page_ids.tables, self.snapshot)?;
        for (_, _, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.schema_name == schema
                && def.table_name == name
                && self.table_belongs_to_database(def.id, database)?
            {
                return Ok(Some(def));
            }
        }
        Ok(None)
    }

    /// Returns the first visible table with the given `table_id`.
    ///
    /// Returns `None` if no such table is visible to the current snapshot.
    pub fn get_table_by_id(&mut self, table_id: TableId) -> Result<Option<TableDef>, DbError> {
        let rows = HeapChain::scan_visible_ro(self.storage, self.page_ids.tables, self.snapshot)?;
        for (_, _, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                return Ok(Some(def));
            }
        }
        Ok(None)
    }

    /// Returns all visible tables in the given schema in the default database.
    pub fn list_tables(&mut self, schema: &str) -> Result<Vec<TableDef>, DbError> {
        self.list_tables_in_database(DEFAULT_DATABASE_NAME, schema)
    }

    /// Returns all visible tables in the given `(database, schema)`.
    pub fn list_tables_in_database(
        &mut self,
        database: &str,
        schema: &str,
    ) -> Result<Vec<TableDef>, DbError> {
        let rows = HeapChain::scan_visible_ro(self.storage, self.page_ids.tables, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.schema_name == schema && self.table_belongs_to_database(def.id, database)? {
                result.push(def);
            }
        }
        Ok(result)
    }

    /// Returns `true` if the logical database exists.
    pub fn database_exists(&mut self, name: &str) -> Result<bool, DbError> {
        Ok(self.get_database(name)?.is_some())
    }

    /// Returns the visible database definition if present.
    pub fn get_database(&mut self, name: &str) -> Result<Option<DatabaseDef>, DbError> {
        let root = self.page_ids.databases;
        if root == 0 {
            return if name == DEFAULT_DATABASE_NAME {
                Ok(Some(DatabaseDef {
                    name: DEFAULT_DATABASE_NAME.to_string(),
                }))
            } else {
                Ok(None)
            };
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        for (_, _, data) in rows {
            let (def, _) = DatabaseDef::from_bytes(&data)?;
            if def.name == name {
                return Ok(Some(def));
            }
        }
        Ok(None)
    }

    /// Returns all visible database definitions.
    pub fn list_databases(&mut self) -> Result<Vec<DatabaseDef>, DbError> {
        let root = self.page_ids.databases;
        if root == 0 {
            return Ok(vec![DatabaseDef {
                name: DEFAULT_DATABASE_NAME.to_string(),
            }]);
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        let mut out = Vec::new();
        for (_, _, data) in rows {
            let (def, _) = DatabaseDef::from_bytes(&data)?;
            out.push(def);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Returns the explicit database binding for `table_id`, if present.
    pub fn get_table_database_binding(
        &mut self,
        table_id: TableId,
    ) -> Result<Option<TableDatabaseDef>, DbError> {
        let root = self.page_ids.table_databases;
        if root == 0 {
            return Ok(None);
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        for (_, _, data) in rows {
            let (def, _) = TableDatabaseDef::from_bytes(&data)?;
            if def.table_id == table_id {
                return Ok(Some(def));
            }
        }
        Ok(None)
    }

    /// Returns the effective owning database for `table_id`, applying the
    /// legacy fallback to [`DEFAULT_DATABASE_NAME`] when no binding exists.
    pub fn effective_table_database(&mut self, table_id: TableId) -> Result<String, DbError> {
        Ok(self
            .get_table_database_binding(table_id)?
            .map(|b| b.database_name)
            .unwrap_or_else(|| DEFAULT_DATABASE_NAME.to_string()))
    }

    /// Returns all visible tables owned by `database` across every schema.
    pub fn list_tables_owned_by_database(
        &mut self,
        database: &str,
    ) -> Result<Vec<TableDef>, DbError> {
        let rows = HeapChain::scan_visible_ro(self.storage, self.page_ids.tables, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if self.table_belongs_to_database(def.id, database)? {
                result.push(def);
            }
        }
        Ok(result)
    }

    fn table_belongs_to_database(
        &mut self,
        table_id: TableId,
        database: &str,
    ) -> Result<bool, DbError> {
        Ok(self.effective_table_database(table_id)? == database)
    }

    // ── Column lookups ────────────────────────────────────────────────────────

    /// Returns all visible columns for `table_id`, ordered by `col_idx`.
    pub fn list_columns(&mut self, table_id: TableId) -> Result<Vec<ColumnDef>, DbError> {
        let rows = HeapChain::scan_visible_ro(self.storage, self.page_ids.columns, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            let (def, _) = ColumnDef::from_bytes(&data)?;
            if def.table_id == table_id {
                result.push(def);
            }
        }
        result.sort_by_key(|c| c.col_idx);
        Ok(result)
    }

    // ── Index lookups ─────────────────────────────────────────────────────────

    /// Returns all visible indexes for `table_id`.
    pub fn list_indexes(&mut self, table_id: TableId) -> Result<Vec<IndexDef>, DbError> {
        let rows = HeapChain::scan_visible_ro(self.storage, self.page_ids.indexes, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            let (def, _) = IndexDef::from_bytes(&data)?;
            if def.table_id == table_id {
                result.push(def);
            }
        }
        Ok(result)
    }

    // ── Constraint lookups (Phase 4.22b) ─────────────────────────────────────

    /// Returns all visible constraints for `table_id`.
    pub fn list_constraints(&mut self, table_id: TableId) -> Result<Vec<ConstraintDef>, DbError> {
        let root = self.page_ids.constraints;
        if root == 0 {
            return Ok(Vec::new()); // legacy database — no constraints table yet
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            if let Ok((def, _)) = ConstraintDef::from_bytes(&data) {
                if def.table_id == table_id {
                    result.push(def);
                }
            }
        }
        Ok(result)
    }

    /// Finds a constraint by name on `table_id`. Returns `None` if not found.
    pub fn get_constraint_by_name(
        &mut self,
        table_id: TableId,
        name: &str,
    ) -> Result<Option<ConstraintDef>, DbError> {
        let root = self.page_ids.constraints;
        if root == 0 {
            return Ok(None);
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        for (_, _, data) in rows {
            if let Ok((def, _)) = ConstraintDef::from_bytes(&data) {
                if def.table_id == table_id && def.name == name {
                    return Ok(Some(def));
                }
            }
        }
        Ok(None)
    }

    /// Scans the raw constraints heap returning (RecordId, row_bytes) pairs.
    ///
    /// Used by `CatalogWriter::drop_constraint` to locate the physical slot.
    pub(crate) fn scan_constraints_root(
        storage: &dyn StorageEngine,
        root: u64,
        snapshot: TransactionSnapshot,
    ) -> Result<Vec<(RecordId, Vec<u8>)>, DbError> {
        if root == 0 {
            return Ok(Vec::new());
        }
        let rows = HeapChain::scan_visible_ro(storage, root, snapshot)?;
        Ok(rows
            .into_iter()
            .map(|(page_id, slot_id, data)| (RecordId { page_id, slot_id }, data))
            .collect())
    }

    // ── FK reads (Phase 6.5) ──────────────────────────────────────────────────

    /// Returns all FK constraints where `table_id` is the **child** table.
    ///
    /// Returns an empty Vec for pre-6.5 databases (FK root = 0).
    pub fn list_fk_constraints(&mut self, table_id: TableId) -> Result<Vec<FkDef>, DbError> {
        let root = self.page_ids.foreign_keys;
        if root == 0 {
            return Ok(Vec::new());
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            if let Ok((def, _)) = FkDef::from_bytes(&data) {
                if def.child_table_id == table_id {
                    result.push(def);
                }
            }
        }
        Ok(result)
    }

    /// Returns all FK constraints where `parent_table_id` is the **parent** table.
    ///
    /// Used by DELETE/UPDATE parent enforcement to find all child tables that
    /// reference this table. Returns empty Vec for pre-6.5 databases.
    pub fn list_fk_constraints_referencing(
        &mut self,
        parent_table_id: u32,
    ) -> Result<Vec<FkDef>, DbError> {
        let root = self.page_ids.foreign_keys;
        if root == 0 {
            return Ok(Vec::new());
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            if let Ok((def, _)) = FkDef::from_bytes(&data) {
                if def.parent_table_id == parent_table_id {
                    result.push(def);
                }
            }
        }
        Ok(result)
    }

    /// Finds a FK constraint by name on `child_table_id`. Returns `None` if
    /// not found.
    pub fn get_fk_by_name(
        &mut self,
        child_table_id: TableId,
        name: &str,
    ) -> Result<Option<FkDef>, DbError> {
        let root = self.page_ids.foreign_keys;
        if root == 0 {
            return Ok(None);
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        for (_, _, data) in rows {
            if let Ok((def, _)) = FkDef::from_bytes(&data) {
                if def.child_table_id == child_table_id && def.name == name {
                    return Ok(Some(def));
                }
            }
        }
        Ok(None)
    }

    // ── Statistics reads (Phase 6.10) ─────────────────────────────────────────

    /// Returns the stats for a specific `(table_id, col_idx)`.
    /// Returns `None` for pre-6.10 databases (stats root = 0).
    pub fn get_stats(&mut self, table_id: u32, col_idx: u16) -> Result<Option<StatsDef>, DbError> {
        let root = self.page_ids.stats;
        if root == 0 {
            return Ok(None);
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        for (_, _, data) in rows {
            if let Ok((def, _)) = StatsDef::from_bytes(&data) {
                if def.table_id == table_id && def.col_idx == col_idx {
                    return Ok(Some(def));
                }
            }
        }
        Ok(None)
    }

    /// Returns all stats rows for `table_id`.
    /// Returns empty vec for pre-6.10 databases.
    pub fn list_stats(&mut self, table_id: u32) -> Result<Vec<StatsDef>, DbError> {
        let root = self.page_ids.stats;
        if root == 0 {
            return Ok(vec![]);
        }
        let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            if let Ok((def, _)) = StatsDef::from_bytes(&data) {
                if def.table_id == table_id {
                    result.push(def);
                }
            }
        }
        Ok(result)
    }

    /// Scans the raw stats heap returning (RecordId, row_bytes) pairs.
    /// Used by `CatalogWriter::upsert_stats` to locate rows to delete.
    pub(crate) fn scan_stats_root(
        storage: &dyn StorageEngine,
        root: u64,
        snapshot: TransactionSnapshot,
    ) -> Result<Vec<(RecordId, Vec<u8>)>, DbError> {
        if root == 0 {
            return Ok(vec![]);
        }
        let rows = HeapChain::scan_visible_ro(storage, root, snapshot)?;
        Ok(rows
            .into_iter()
            .map(|(page_id, slot_id, data)| (RecordId { page_id, slot_id }, data))
            .collect())
    }

    /// Scans the raw databases heap returning (RecordId, row_bytes) pairs.
    pub(crate) fn scan_databases_root(
        storage: &dyn StorageEngine,
        root: u64,
        snapshot: TransactionSnapshot,
    ) -> Result<Vec<(RecordId, Vec<u8>)>, DbError> {
        if root == 0 {
            return Ok(Vec::new());
        }
        let rows = HeapChain::scan_visible_ro(storage, root, snapshot)?;
        Ok(rows
            .into_iter()
            .map(|(page_id, slot_id, data)| (RecordId { page_id, slot_id }, data))
            .collect())
    }

    /// Scans the raw table-database binding heap returning (RecordId, row_bytes) pairs.
    pub(crate) fn scan_table_databases_root(
        storage: &dyn StorageEngine,
        root: u64,
        snapshot: TransactionSnapshot,
    ) -> Result<Vec<(RecordId, Vec<u8>)>, DbError> {
        if root == 0 {
            return Ok(Vec::new());
        }
        let rows = HeapChain::scan_visible_ro(storage, root, snapshot)?;
        Ok(rows
            .into_iter()
            .map(|(page_id, slot_id, data)| (RecordId { page_id, slot_id }, data))
            .collect())
    }

    /// Scans the raw FK heap returning (RecordId, row_bytes) pairs.
    ///
    /// Used by `CatalogWriter::drop_foreign_key` to locate the physical slot.
    pub(crate) fn scan_fk_root(
        storage: &dyn StorageEngine,
        root: u64,
        snapshot: TransactionSnapshot,
    ) -> Result<Vec<(RecordId, Vec<u8>)>, DbError> {
        if root == 0 {
            return Ok(Vec::new());
        }
        let rows = HeapChain::scan_visible_ro(storage, root, snapshot)?;
        Ok(rows
            .into_iter()
            .map(|(page_id, slot_id, data)| (RecordId { page_id, slot_id }, data))
            .collect())
    }
}
