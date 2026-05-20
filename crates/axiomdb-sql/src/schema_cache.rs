//! Schema cache — avoids repeated catalog heap scans for the same table.
//!
//! ## Problem
//!
//! `analyze()` calls `CatalogReader::get_table()` + `list_columns()` on every
//! statement. Each call does a full `HeapChain::scan_visible()` of the catalog
//! pages. For 10K consecutive INSERTs into the same table this means 20,000
//! heap scans of a schema that never changes between rows.
//!
//! ## Solution
//!
//! `SchemaCache` stores `(TableDef, Vec<ColumnDef>)` keyed by
//! `(database_name, schema_name, table_name)`. The caller creates one cache per
//! "session" or "batch" and passes it to `analyze_cached()`. Cache misses fall
//! back to the normal catalog scan and populate the cache for subsequent calls.
//!
//! ## Invalidation
//!
//! Call `SchemaCache::invalidate()` after any DDL statement (CREATE TABLE,
//! DROP TABLE, ALTER TABLE) to force the next lookup to re-read from the catalog.
//! In the executor, DDL handlers should call this before returning.
//!
//! ## Thread safety
//!
//! `SchemaCache` is `!Send` — it must be owned by a single thread/task. For
//! concurrent workloads, each connection gets its own cache.

use std::collections::HashMap;

use axiomdb_catalog::schema::{ColumnDef, TableDef, TableId};

/// Cache key: (database_name, schema_name, table_name).
type TableKey = (String, String, String);

/// In-memory cache of catalog metadata valid for one session or batch.
///
/// Create with [`SchemaCache::new`], pass to [`analyze_cached`], call
/// [`SchemaCache::invalidate`] after DDL.
#[derive(Default)]
pub struct SchemaCache {
    /// `(database, schema, table_name)` → `TableDef`
    tables: HashMap<TableKey, TableDef>,
    /// `table_id` → ordered `Vec<ColumnDef>`
    columns: HashMap<TableId, Vec<ColumnDef>>,
    /// Attack 22 (real): `table_id` → `schema_version`. Updated on every
    /// `insert`, cleared by `invalidate`. Used by
    /// `PlanDeps::is_stale_via_cache` to skip the catalog-heap probe for
    /// every dep on a cache lookup — the cache holds the same
    /// `schema_version` the catalog would return, and DDL invalidates
    /// the cache before the next query runs.
    id_to_version: HashMap<TableId, u64>,
}

impl SchemaCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a cached table definition.
    pub fn get_table(&self, database: &str, schema: &str, name: &str) -> Option<&TableDef> {
        self.tables
            .get(&(database.to_string(), schema.to_string(), name.to_string()))
    }

    /// Look up cached columns for a table.
    pub fn get_columns(&self, table_id: TableId) -> Option<&Vec<ColumnDef>> {
        self.columns.get(&table_id)
    }

    /// Store a table definition and its columns.
    pub fn insert(
        &mut self,
        database: &str,
        schema: &str,
        name: &str,
        table_def: TableDef,
        columns: Vec<ColumnDef>,
    ) {
        let id = table_def.id;
        let version = table_def.schema_version;
        self.tables.insert(
            (database.to_string(), schema.to_string(), name.to_string()),
            table_def,
        );
        self.columns.insert(id, columns);
        self.id_to_version.insert(id, version);
    }

    /// Drop all cached entries. Call after any DDL statement.
    pub fn invalidate(&mut self) {
        self.tables.clear();
        self.columns.clear();
        self.id_to_version.clear();
    }

    /// Attack 22 (real): O(1) lookup of a table's `schema_version`
    /// from the in-memory cache. Used by `PlanDeps::is_stale_via_cache`
    /// to skip the catalog-heap probe for every cached-plan lookup.
    ///
    /// Returns `None` when the table is not in the cache — caller must
    /// fall back to a `CatalogReader::get_table_schema_version` probe
    /// for correctness (the table may exist but just not be cached yet).
    pub fn get_schema_version(&self, table_id: TableId) -> Option<u64> {
        self.id_to_version.get(&table_id).copied()
    }

    /// Number of cached tables (for diagnostics / tests).
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}
