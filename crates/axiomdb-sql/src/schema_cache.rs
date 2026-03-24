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
//! `(schema_name, table_name)`. The caller creates one cache per "session" or
//! "batch" and passes it to `analyze_cached()`. Cache misses fall back to the
//! normal catalog scan and populate the cache for subsequent calls.
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

/// Cache key: (schema_name, table_name).
type TableKey = (String, String);

/// In-memory cache of catalog metadata valid for one session or batch.
///
/// Create with [`SchemaCache::new`], pass to [`analyze_cached`], call
/// [`SchemaCache::invalidate`] after DDL.
#[derive(Default)]
pub struct SchemaCache {
    /// `(schema, table_name)` → `TableDef`
    tables: HashMap<TableKey, TableDef>,
    /// `table_id` → ordered `Vec<ColumnDef>`
    columns: HashMap<TableId, Vec<ColumnDef>>,
}

impl SchemaCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a cached table definition.
    pub fn get_table(&self, schema: &str, name: &str) -> Option<&TableDef> {
        self.tables.get(&(schema.to_string(), name.to_string()))
    }

    /// Look up cached columns for a table.
    pub fn get_columns(&self, table_id: TableId) -> Option<&Vec<ColumnDef>> {
        self.columns.get(&table_id)
    }

    /// Store a table definition and its columns.
    pub fn insert(
        &mut self,
        schema: &str,
        name: &str,
        table_def: TableDef,
        columns: Vec<ColumnDef>,
    ) {
        let id = table_def.id;
        self.tables
            .insert((schema.to_string(), name.to_string()), table_def);
        self.columns.insert(id, columns);
    }

    /// Drop all cached entries. Call after any DDL statement.
    pub fn invalidate(&mut self) {
        self.tables.clear();
        self.columns.clear();
    }

    /// Number of cached tables (for diagnostics / tests).
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}
