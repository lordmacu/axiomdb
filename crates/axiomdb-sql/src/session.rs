//! Session context — per-connection state including the schema cache.
//!
//! [`SessionContext`] holds a cache of resolved table schemas to avoid
//! repeating expensive catalog heap scans on every statement.
//!
//! ## The problem without cache
//!
//! Every `execute()` call creates a fresh `SchemaResolver`, which creates a
//! fresh `CatalogReader`, which scans `nexus_tables` and `nexus_columns` linearly
//! (O(n) heap reads) to find the table definition. For a table with 10 columns,
//! this is ~14 mmap page reads per statement — completely dominating the cost of
//! simple queries.
//!
//! ## The solution
//!
//! `SessionContext` caches `ResolvedTable` values by `"schema.table_name"`.
//! The cache is:
//! - **Populated lazily** on the first access to each table.
//! - **Fully invalidated** on any DDL that could change the schema
//!   (CREATE TABLE, DROP TABLE, CREATE INDEX, DROP INDEX, ALTER TABLE).
//! - **Not shared** across connections — each connection owns its context.
//!
//! ## Isolation semantics
//!
//! The cache stores the schema as seen at the time of the first lookup within
//! the current transaction. DDL within the same transaction is applied
//! immediately (invalidation happens before the DDL executes, so the next
//! lookup re-reads the fresh catalog).

use std::collections::HashMap;

use axiomdb_catalog::ResolvedTable;

/// Per-connection state: schema cache + future session variables.
#[derive(Debug, Default)]
pub struct SessionContext {
    /// Cached table schemas keyed by `"schema_name.table_name"`.
    cache: HashMap<String, ResolvedTable>,
}

impl SessionContext {
    /// Creates an empty session context with no cached schemas.
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// Returns the cache key for a table.
    fn key(schema: &str, table: &str) -> String {
        format!("{schema}.{table}")
    }

    /// Returns the cached `ResolvedTable` for the given schema + table, if any.
    pub fn get_table(&self, schema: &str, table: &str) -> Option<&ResolvedTable> {
        self.cache.get(&Self::key(schema, table))
    }

    /// Stores a resolved table in the cache.
    pub fn cache_table(&mut self, schema: &str, table: &str, resolved: ResolvedTable) {
        self.cache.insert(Self::key(schema, table), resolved);
    }

    /// Removes a specific table from the cache.
    ///
    /// Call this before executing a DDL statement that affects one table
    /// (e.g., `ALTER TABLE`, targeted `DROP TABLE`).
    pub fn invalidate_table(&mut self, schema: &str, table: &str) {
        self.cache.remove(&Self::key(schema, table));
    }

    /// Clears the entire schema cache.
    ///
    /// Call this before any DDL statement that could affect multiple tables
    /// (e.g., `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`).
    pub fn invalidate_all(&mut self) {
        self.cache.clear();
    }

    /// Returns the number of cached table schemas.
    pub fn cached_count(&self) -> usize {
        self.cache.len()
    }
}
