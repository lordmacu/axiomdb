//! INFORMATION_SCHEMA virtual database — MySQL-compatible read-only views
//! of catalog metadata (4.20c).
//!
//! Provides:
//! - Column schemas for each virtual IS table (used by the analyzer to resolve
//!   column names/indices without hitting the real catalog)
//! - Helper to build a synthetic [`ColumnDef`] list for the analyzer's `BoundTable`
//!
//! The executor counterpart lives in `executor/information_schema_exec.rs`.

use axiomdb_catalog::schema::{ColumnDef as CatalogColumnDef, ColumnType};

/// Synthetic table_id used for all IS virtual `ColumnDef` objects.
/// Must not collide with real table IDs; 0 is safe (real IDs start from 1).
const IS_TABLE_ID: u32 = 0;

// ── Column schemas ────────────────────────────────────────────────────────────

/// Column names and types for `information_schema.TABLES`.
pub static IS_TABLES_COLS: &[(&str, ColumnType)] = &[
    ("TABLE_CATALOG", ColumnType::Text),
    ("TABLE_SCHEMA", ColumnType::Text),
    ("TABLE_NAME", ColumnType::Text),
    ("TABLE_TYPE", ColumnType::Text),
    ("ENGINE", ColumnType::Text),
    ("VERSION", ColumnType::BigInt),
    ("ROW_FORMAT", ColumnType::Text),
    ("TABLE_ROWS", ColumnType::BigInt),
    ("AVG_ROW_LENGTH", ColumnType::BigInt),
    ("DATA_LENGTH", ColumnType::BigInt),
    ("MAX_DATA_LENGTH", ColumnType::BigInt),
    ("INDEX_LENGTH", ColumnType::BigInt),
    ("DATA_FREE", ColumnType::BigInt),
    ("AUTO_INCREMENT", ColumnType::BigInt),
    ("CREATE_TIME", ColumnType::Text),
    ("UPDATE_TIME", ColumnType::Text),
    ("CHECK_TIME", ColumnType::Text),
    ("TABLE_COLLATION", ColumnType::Text),
    ("CHECKSUM", ColumnType::BigInt),
    ("CREATE_OPTIONS", ColumnType::Text),
    ("TABLE_COMMENT", ColumnType::Text),
];

/// Column names and types for `information_schema.COLUMNS`.
pub static IS_COLUMNS_COLS: &[(&str, ColumnType)] = &[
    ("TABLE_CATALOG", ColumnType::Text),
    ("TABLE_SCHEMA", ColumnType::Text),
    ("TABLE_NAME", ColumnType::Text),
    ("COLUMN_NAME", ColumnType::Text),
    ("ORDINAL_POSITION", ColumnType::BigInt),
    ("COLUMN_DEFAULT", ColumnType::Text),
    ("IS_NULLABLE", ColumnType::Text),
    ("DATA_TYPE", ColumnType::Text),
    ("CHARACTER_MAXIMUM_LENGTH", ColumnType::BigInt),
    ("CHARACTER_OCTET_LENGTH", ColumnType::BigInt),
    ("NUMERIC_PRECISION", ColumnType::BigInt),
    ("NUMERIC_SCALE", ColumnType::BigInt),
    ("DATETIME_PRECISION", ColumnType::BigInt),
    ("CHARACTER_SET_NAME", ColumnType::Text),
    ("COLLATION_NAME", ColumnType::Text),
    ("COLUMN_TYPE", ColumnType::Text),
    ("COLUMN_KEY", ColumnType::Text),
    ("EXTRA", ColumnType::Text),
    ("PRIVILEGES", ColumnType::Text),
    ("COLUMN_COMMENT", ColumnType::Text),
    ("GENERATION_EXPRESSION", ColumnType::Text),
    ("SRS_ID", ColumnType::BigInt),
];

/// Column names and types for `information_schema.KEY_COLUMN_USAGE`.
pub static IS_KEY_COLUMN_USAGE_COLS: &[(&str, ColumnType)] = &[
    ("CONSTRAINT_CATALOG", ColumnType::Text),
    ("CONSTRAINT_SCHEMA", ColumnType::Text),
    ("CONSTRAINT_NAME", ColumnType::Text),
    ("TABLE_CATALOG", ColumnType::Text),
    ("TABLE_SCHEMA", ColumnType::Text),
    ("TABLE_NAME", ColumnType::Text),
    ("COLUMN_NAME", ColumnType::Text),
    ("ORDINAL_POSITION", ColumnType::BigInt),
    ("POSITION_IN_UNIQUE_CONSTRAINT", ColumnType::BigInt),
    ("REFERENCED_TABLE_SCHEMA", ColumnType::Text),
    ("REFERENCED_TABLE_NAME", ColumnType::Text),
    ("REFERENCED_COLUMN_NAME", ColumnType::Text),
];

/// Column names and types for `information_schema.TABLE_CONSTRAINTS`.
pub static IS_TABLE_CONSTRAINTS_COLS: &[(&str, ColumnType)] = &[
    ("CONSTRAINT_CATALOG", ColumnType::Text),
    ("CONSTRAINT_SCHEMA", ColumnType::Text),
    ("CONSTRAINT_NAME", ColumnType::Text),
    ("TABLE_SCHEMA", ColumnType::Text),
    ("TABLE_NAME", ColumnType::Text),
    ("CONSTRAINT_TYPE", ColumnType::Text),
    ("ENFORCED", ColumnType::Text),
];

/// Column names and types for `information_schema.REFERENTIAL_CONSTRAINTS`.
pub static IS_REFERENTIAL_CONSTRAINTS_COLS: &[(&str, ColumnType)] = &[
    ("CONSTRAINT_CATALOG", ColumnType::Text),
    ("CONSTRAINT_SCHEMA", ColumnType::Text),
    ("CONSTRAINT_NAME", ColumnType::Text),
    ("UNIQUE_CONSTRAINT_CATALOG", ColumnType::Text),
    ("UNIQUE_CONSTRAINT_SCHEMA", ColumnType::Text),
    ("UNIQUE_CONSTRAINT_NAME", ColumnType::Text),
    ("MATCH_OPTION", ColumnType::Text),
    ("UPDATE_RULE", ColumnType::Text),
    ("DELETE_RULE", ColumnType::Text),
    ("TABLE_NAME", ColumnType::Text),
    ("REFERENCED_TABLE_NAME", ColumnType::Text),
];

/// Column names and types for `information_schema.STATISTICS`.
pub static IS_STATISTICS_COLS: &[(&str, ColumnType)] = &[
    ("TABLE_CATALOG", ColumnType::Text),
    ("TABLE_SCHEMA", ColumnType::Text),
    ("TABLE_NAME", ColumnType::Text),
    ("NON_UNIQUE", ColumnType::BigInt),
    ("INDEX_SCHEMA", ColumnType::Text),
    ("INDEX_NAME", ColumnType::Text),
    ("SEQ_IN_INDEX", ColumnType::BigInt),
    ("COLUMN_NAME", ColumnType::Text),
    ("COLLATION", ColumnType::Text),
    ("CARDINALITY", ColumnType::BigInt),
    ("SUB_PART", ColumnType::BigInt),
    ("PACKED", ColumnType::Text),
    ("NULLABLE", ColumnType::Text),
    ("INDEX_TYPE", ColumnType::Text),
    ("COMMENT", ColumnType::Text),
    ("INDEX_COMMENT", ColumnType::Text),
    ("IS_VISIBLE", ColumnType::Text),
    ("EXPRESSION", ColumnType::Text),
];

/// Column names and types for `information_schema.VIEWS`.
pub static IS_VIEWS_COLS: &[(&str, ColumnType)] = &[
    ("TABLE_CATALOG", ColumnType::Text),
    ("TABLE_SCHEMA", ColumnType::Text),
    ("TABLE_NAME", ColumnType::Text),
    ("VIEW_DEFINITION", ColumnType::Text),
    ("CHECK_OPTION", ColumnType::Text),
    ("IS_UPDATABLE", ColumnType::Text),
];

/// Column names and types for `information_schema.SCHEMATA`.
pub static IS_SCHEMATA_COLS: &[(&str, ColumnType)] = &[
    ("CATALOG_NAME", ColumnType::Text),
    ("SCHEMA_NAME", ColumnType::Text),
    ("DEFAULT_CHARACTER_SET_NAME", ColumnType::Text),
    ("DEFAULT_COLLATION_NAME", ColumnType::Text),
    ("SQL_PATH", ColumnType::Text),
];

/// Column names and types for `information_schema.SCHEDULED_JOBS` (Phase 22b.1).
pub static IS_SCHEDULED_JOBS_COLS: &[(&str, ColumnType)] = &[
    ("JOB_NAME", ColumnType::Text),
    ("SCHEDULE", ColumnType::Text),
    ("COMMAND", ColumnType::Text),
    ("DATABASE_NAME", ColumnType::Text),
    ("ENABLED", ColumnType::Text),
    ("NEXT_RUN", ColumnType::Text),
    ("LAST_RUN", ColumnType::Text),
    ("LAST_STATUS", ColumnType::Text),
];

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the column schema for the given IS virtual table name
/// (case-insensitive), or `None` if the table is unknown.
pub fn is_table_cols(table_name: &str) -> Option<&'static [(&'static str, ColumnType)]> {
    match table_name.to_lowercase().as_str() {
        "tables" => Some(IS_TABLES_COLS),
        "columns" => Some(IS_COLUMNS_COLS),
        "key_column_usage" => Some(IS_KEY_COLUMN_USAGE_COLS),
        "table_constraints" => Some(IS_TABLE_CONSTRAINTS_COLS),
        "referential_constraints" => Some(IS_REFERENTIAL_CONSTRAINTS_COLS),
        "statistics" => Some(IS_STATISTICS_COLS),
        "views" => Some(IS_VIEWS_COLS),
        "schemata" => Some(IS_SCHEMATA_COLS),
        "scheduled_jobs" => Some(IS_SCHEDULED_JOBS_COLS),
        _ => None,
    }
}

/// Returns `true` if `schema` (case-insensitive) is `"information_schema"`.
pub fn is_information_schema(schema: &str) -> bool {
    schema.eq_ignore_ascii_case("information_schema")
}

/// Builds a synthetic `Vec<CatalogColumnDef>` for an IS virtual table.
///
/// Used by the analyzer's `bound_table_ref` to create a `BoundTable` without
/// touching the real catalog. `col_offset` is the starting column index in the
/// combined row (for JOIN support).
///
/// Returns `None` if `table_name` is not a known IS table.
pub fn make_is_catalog_columns(
    table_name: &str,
    col_offset: usize,
) -> Option<Vec<CatalogColumnDef>> {
    let cols = is_table_cols(table_name)?;
    Some(
        cols.iter()
            .enumerate()
            .map(|(i, (name, col_type))| CatalogColumnDef {
                table_id: IS_TABLE_ID,
                col_idx: (col_offset + i) as u16,
                name: name.to_string(),
                col_type: *col_type,
                nullable: true,
                auto_increment: false,
                type_len: 0,
                is_fixed_len: false,
                default_expr: None,
                on_update_expr: None,
                generated_expr: None,
                collation: None,
                generated_stored: false,
                enum_type_name: None,
                array_element_type: None,
                array_ndims: None,
            })
            .collect(),
    )
}
