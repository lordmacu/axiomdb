//! Schema binding — resolves SQL identifier strings to catalog metadata.
//!
//! [`SchemaResolver`] wraps a [`CatalogReader`] and maps table/column name
//! strings to their [`TableDef`], [`ColumnDef`], and [`IndexDef`] counterparts.
//! The Phase 4 executor uses this to convert AST name references into the typed
//! metadata needed for plan construction and type checking.
//!
//! ## Default schema
//!
//! The resolver is constructed with a `default_schema` string (typically
//! `"public"`). Callers may pass `None` as the schema argument to any method
//! and the resolver will substitute the default automatically.
//!
//! ## Case sensitivity
//!
//! All identifier comparisons are case-sensitive. Case-insensitive resolution
//! is deferred to Phase 5 (session charset / collation negotiation).

use axiomdb_core::{error::DbError, TransactionSnapshot};
use axiomdb_storage::StorageEngine;

use crate::{
    reader::CatalogReader,
    schema::{ColumnDef, ConstraintDef, IndexDef, TableDef, TableId},
};

// ── ResolvedTable ─────────────────────────────────────────────────────────────

/// Full metadata for a catalog table, resolved in a single operation.
///
/// The executor uses this to avoid multiple round-trips to the catalog when
/// binding a FROM clause — it obtains the table definition, all its columns
/// (sorted by `col_idx`), all its indexes, and all named constraints at once.
#[derive(Debug, Clone)]
pub struct ResolvedTable {
    /// Table definition (id, schema_name, table_name).
    pub def: TableDef,
    /// All visible columns for this table, sorted ascending by `col_idx`.
    pub columns: Vec<ColumnDef>,
    /// All visible indexes for this table.
    pub indexes: Vec<IndexDef>,
    /// All visible named constraints (CHECK) for this table (Phase 4.22b).
    pub constraints: Vec<ConstraintDef>,
}

// ── SchemaResolver ────────────────────────────────────────────────────────────

/// Name-resolution service over the catalog.
///
/// Wraps a [`CatalogReader`] and applies a `default_schema` so callers can
/// resolve unqualified names like `"users"` without always passing `"public"`.
pub struct SchemaResolver<'a> {
    reader: CatalogReader<'a>,
    default_schema: &'a str,
}

impl<'a> SchemaResolver<'a> {
    /// Creates a resolver backed by `storage` at the given `snapshot`.
    ///
    /// `default_schema` is used when `schema` is `None` in subsequent calls.
    /// Pass `"public"` for MySQL/PostgreSQL-compatible behavior.
    ///
    /// # Errors
    /// - [`DbError::CatalogNotInitialized`] if the catalog has not been bootstrapped.
    pub fn new(
        storage: &'a dyn StorageEngine,
        snapshot: TransactionSnapshot,
        default_schema: &'a str,
    ) -> Result<Self, DbError> {
        let reader = CatalogReader::new(storage, snapshot)?;
        Ok(Self {
            reader,
            default_schema,
        })
    }

    // ── Table resolution ──────────────────────────────────────────────────────

    /// Resolves a table name to its full metadata.
    ///
    /// `schema`: the schema to search in. `None` uses `default_schema`.
    ///
    /// Returns [`ResolvedTable`] with the table definition, all visible columns
    /// sorted by `col_idx`, and all visible indexes.
    ///
    /// # Errors
    /// - [`DbError::TableNotFound`] — no visible table with that name in the schema.
    pub fn resolve_table(
        &mut self,
        schema: Option<&str>,
        table_name: &str,
    ) -> Result<ResolvedTable, DbError> {
        let schema = schema.unwrap_or(self.default_schema);

        let def =
            self.reader
                .get_table(schema, table_name)?
                .ok_or_else(|| DbError::TableNotFound {
                    name: format!("{schema}.{table_name}"),
                })?;

        let columns = self.reader.list_columns(def.id)?;
        let indexes = self.reader.list_indexes(def.id)?;
        let constraints = self.reader.list_constraints(def.id)?;

        Ok(ResolvedTable {
            def,
            columns,
            indexes,
            constraints,
        })
    }

    /// Returns `true` if a visible table with the given name exists.
    ///
    /// `schema`: `None` uses `default_schema`.
    ///
    /// Cheaper than [`resolve_table`] when only existence matters — stops as
    /// soon as the first match is found without loading columns or indexes.
    ///
    /// [`resolve_table`]: SchemaResolver::resolve_table
    pub fn table_exists(
        &mut self,
        schema: Option<&str>,
        table_name: &str,
    ) -> Result<bool, DbError> {
        let schema = schema.unwrap_or(self.default_schema);
        Ok(self.reader.get_table(schema, table_name)?.is_some())
    }

    // ── Column resolution ─────────────────────────────────────────────────────

    /// Resolves a column name within a table to its `ColumnDef`.
    ///
    /// The executor calls this when binding expression references (WHERE clause,
    /// SELECT list, etc.) after it has already resolved the table.
    ///
    /// # Errors
    /// - [`DbError::ColumnNotFound`] — no column with that name in the table.
    pub fn resolve_column(
        &mut self,
        table_id: TableId,
        col_name: &str,
    ) -> Result<ColumnDef, DbError> {
        let columns = self.reader.list_columns(table_id)?;

        if let Some(col) = columns.into_iter().find(|c| c.name == col_name) {
            return Ok(col);
        }

        // Build a qualified label for the error message.
        let table_label = self
            .reader
            .get_table_by_id(table_id)?
            .map(|t| format!("{}.{}", t.schema_name, t.table_name))
            .unwrap_or_else(|| format!("table_id={table_id}"));

        Err(DbError::ColumnNotFound {
            name: col_name.to_string(),
            table: table_label,
        })
    }
}
