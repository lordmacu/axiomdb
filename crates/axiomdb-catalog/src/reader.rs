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

use crate::{
    bootstrap::{CatalogBootstrap, CatalogPageIds},
    schema::{ColumnDef, IndexDef, TableDef, TableId},
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

    /// Returns the first visible table matching `(schema_name, table_name)`.
    ///
    /// Returns `None` if no such table is visible to the current snapshot.
    pub fn get_table(&self, schema: &str, name: &str) -> Result<Option<TableDef>, DbError> {
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, self.snapshot)?;
        for (_, _, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.schema_name == schema && def.table_name == name {
                return Ok(Some(def));
            }
        }
        Ok(None)
    }

    /// Returns the first visible table with the given `table_id`.
    ///
    /// Returns `None` if no such table is visible to the current snapshot.
    pub fn get_table_by_id(&self, table_id: TableId) -> Result<Option<TableDef>, DbError> {
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, self.snapshot)?;
        for (_, _, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                return Ok(Some(def));
            }
        }
        Ok(None)
    }

    /// Returns all visible tables in the given schema.
    pub fn list_tables(&self, schema: &str) -> Result<Vec<TableDef>, DbError> {
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.schema_name == schema {
                result.push(def);
            }
        }
        Ok(result)
    }

    // ── Column lookups ────────────────────────────────────────────────────────

    /// Returns all visible columns for `table_id`, ordered by `col_idx`.
    pub fn list_columns(&self, table_id: TableId) -> Result<Vec<ColumnDef>, DbError> {
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.columns, self.snapshot)?;
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
    pub fn list_indexes(&self, table_id: TableId) -> Result<Vec<IndexDef>, DbError> {
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, self.snapshot)?;
        let mut result = Vec::new();
        for (_, _, data) in rows {
            let (def, _) = IndexDef::from_bytes(&data)?;
            if def.table_id == table_id {
                result.push(def);
            }
        }
        Ok(result)
    }
}
