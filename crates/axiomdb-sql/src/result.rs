//! Query result types — the unified return value from the AxiomDB executor.
//!
//! Every SQL statement executed by the engine returns exactly one
//! [`QueryResult`]:
//!
//! - [`QueryResult::Rows`] — SELECT (and future `RETURNING` clauses): zero or
//!   more rows with typed column metadata.
//! - [`QueryResult::Affected`] — INSERT / UPDATE / DELETE: number of rows
//!   changed and an optional auto-generated ID.
//! - [`QueryResult::Empty`] — DDL (CREATE TABLE, DROP TABLE, CREATE INDEX,
//!   DROP INDEX, ALTER TABLE, TRUNCATE): no data returned.
//!
//! ## Contract with callers
//!
//! Callers (embedded API, MySQL wire protocol server, CLI REPL) pattern-match
//! on the variant. The type carries all information needed to produce the
//! correct response without accessing any internal engine state:
//!
//! - The **embedded API** uses `name` + `data_type` for typed result access.
//! - The **CLI REPL** (Phase 4.15) uses `name` for column headers and
//!   `data_type` for value formatting.
//! - The **MySQL wire protocol** (Phase 5) uses all four `ColumnMeta` fields
//!   to build `ColumnDefinition41` packets.
//!
//! ## Invariants
//!
//! - In `Rows`, `rows[i].len() == columns.len()` for every row `i`.
//! - In `Rows`, `rows[i][j]` is `Value::Null` only if `columns[j].nullable`.
//! - `last_insert_id` is `Some(id)` only for INSERT into an AUTO_INCREMENT
//!   table with at least one inserted row.

use axiomdb_types::{DataType, Value};

// ── Row ───────────────────────────────────────────────────────────────────────

/// A single output row produced by a SELECT.
///
/// One [`Value`] per output column, in the same order as
/// [`QueryResult::Rows::columns`]. May contain [`Value::Null`] for nullable
/// columns and computed expressions.
///
/// A type alias (not a newtype) so that callers can use `Vec` methods directly
/// without boilerplate `Deref` or `IntoIterator` implementations.
pub type Row = Vec<Value>;

// ── ColumnMeta ────────────────────────────────────────────────────────────────

/// Metadata for one output column in a SELECT result.
///
/// ## Field semantics
///
/// | Field | Populated by executor | Used by |
/// |---|---|---|
/// | `name` | Column name or alias | CLI headers, MySQL protocol |
/// | `data_type` | Catalog `ColumnType` → `DataType` | Wire serialization, typed access |
/// | `nullable` | Catalog `nullable` flag | Protocol flags, application logic |
/// | `table_name` | Originating table name (if known) | MySQL `ColumnDefinition41.table` |
///
/// ## Computed columns
///
/// For expressions without an originating table (`SELECT 1 + 1`,
/// `SELECT COUNT(*)`), set `table_name = None` and `nullable = true`
/// (the executor cannot guarantee non-null for arbitrary expressions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    /// Output column name.
    ///
    /// For a bare column reference (`SELECT age`): the catalog column name.
    /// For an aliased column (`SELECT age AS years`): the alias.
    /// For a computed expression (`SELECT 1 + 1`): a generated name such as
    /// `"1 + 1"` or `"?column?"`. Must be non-empty.
    pub name: String,

    /// SQL data type of this column's values.
    ///
    /// Used by the wire protocol to choose the correct MySQL type code and by
    /// the embedded API for typed access to result values.
    pub data_type: DataType,

    /// Whether this column can contain [`Value::Null`].
    ///
    /// `false` guarantees the executor will never emit `Value::Null` in this
    /// position. `true` for nullable columns, outer-join outputs, aggregate
    /// results over empty groups, and all computed expressions.
    pub nullable: bool,

    /// Name of the originating table, if known.
    ///
    /// `Some("users")` for `SELECT users.age FROM users`.
    /// `None` for computed columns (`SELECT 1 + 1`), subquery outputs, and
    /// aggregate functions.
    ///
    /// Required by the MySQL wire protocol's `ColumnDefinition41` packet
    /// (`table` field). Not providing it here would force Phase 5 to retrofit
    /// all executor call sites.
    pub table_name: Option<String>,
}

impl ColumnMeta {
    /// Constructs a [`ColumnMeta`] with all four fields.
    ///
    /// This is the primary constructor used by the executor when building
    /// result column descriptors from catalog [`ColumnDef`] records.
    ///
    /// [`ColumnDef`]: axiomdb_catalog::schema::ColumnDef
    pub fn new(
        name: impl Into<String>,
        data_type: DataType,
        nullable: bool,
        table_name: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
            table_name,
        }
    }

    /// Constructs a [`ColumnMeta`] for a computed expression column.
    ///
    /// Sets `nullable = true` and `table_name = None`, which are correct
    /// defaults for expressions that have no catalog backing.
    pub fn computed(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable: true,
            table_name: None,
        }
    }
}

// ── QueryResult ───────────────────────────────────────────────────────────────

/// The unified return type from the AxiomDB executor for any SQL statement.
///
/// Pattern-match on the variant to handle each statement class:
///
/// ```rust
/// # use axiomdb_sql::result::{QueryResult, ColumnMeta};
/// # use axiomdb_types::{DataType, Value};
/// # let result = QueryResult::Empty;
/// match result {
///     QueryResult::Rows { columns, rows } => {
///         println!("{} columns, {} rows", columns.len(), rows.len());
///     }
///     QueryResult::Affected { count, last_insert_id } => {
///         println!("{count} rows affected");
///         if let Some(id) = last_insert_id {
///             println!("last insert id = {id}");
///         }
///     }
///     QueryResult::Empty => {
///         println!("OK");
///     }
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// Result of a SELECT statement (or future `RETURNING` clause).
    ///
    /// `columns` describes the output schema; `rows` contains the data.
    /// An empty SELECT (no matching rows) has `rows = vec![]` but still
    /// carries `columns` — use [`QueryResult::empty_rows`] to construct it.
    ///
    /// **Invariant:** `rows[i].len() == columns.len()` for all `i`.
    Rows {
        columns: Vec<ColumnMeta>,
        rows: Vec<Row>,
    },

    /// Result of INSERT, UPDATE, or DELETE.
    ///
    /// `count` is the number of heap tuples written, updated, or deleted.
    /// Does not include cascaded foreign key actions (Phase 13).
    ///
    /// `last_insert_id` is `Some(id)` only when:
    /// - The statement was an INSERT, AND
    /// - The target table has an AUTO_INCREMENT or SERIAL column, AND
    /// - At least one row was inserted.
    ///
    /// Matches MySQL's `LAST_INSERT_ID()` semantics: for a multi-row INSERT,
    /// it holds the ID of the **first** inserted row (not the last).
    Affected {
        count: u64,
        last_insert_id: Option<u64>,
    },

    /// Result of DDL statements: CREATE TABLE, DROP TABLE, CREATE INDEX,
    /// DROP INDEX, ALTER TABLE, TRUNCATE.
    ///
    /// No data is returned. The statement either succeeded (Ok(Empty)) or
    /// failed (Err(DbError)).
    Empty,
}

impl QueryResult {
    /// Constructs a `Rows` result with the given columns and no rows.
    ///
    /// Used when a SELECT executes successfully but matches no tuples.
    /// The column descriptors are still present so callers can render
    /// headers even for empty results.
    pub fn empty_rows(columns: Vec<ColumnMeta>) -> Self {
        Self::Rows {
            columns,
            rows: vec![],
        }
    }

    /// Constructs an `Affected` result for DML without an auto-increment ID.
    ///
    /// Use this for UPDATE, DELETE, and INSERT into tables without
    /// AUTO_INCREMENT / SERIAL columns.
    pub fn affected(count: u64) -> Self {
        Self::Affected {
            count,
            last_insert_id: None,
        }
    }

    /// Constructs an `Affected` result for INSERT with an auto-generated ID.
    ///
    /// `last_insert_id` is the first auto-generated ID for the batch
    /// (MySQL `LAST_INSERT_ID()` semantics).
    pub fn affected_with_id(count: u64, last_insert_id: u64) -> Self {
        Self::Affected {
            count,
            last_insert_id: Some(last_insert_id),
        }
    }

    /// Returns the number of result rows for a `Rows` result.
    ///
    /// Returns `None` for `Affected` and `Empty` results.
    pub fn row_count(&self) -> Option<usize> {
        match self {
            Self::Rows { rows, .. } => Some(rows.len()),
            _ => None,
        }
    }

    /// Returns the number of output columns for a `Rows` result.
    ///
    /// Returns `None` for `Affected` and `Empty` results.
    pub fn column_count(&self) -> Option<usize> {
        match self {
            Self::Rows { columns, .. } => Some(columns.len()),
            _ => None,
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ColumnMeta ───────────────────────────────────────────────────────────

    #[test]
    fn test_column_meta_new_stores_fields() {
        let meta = ColumnMeta::new("age", DataType::Int, false, Some("users".into()));
        assert_eq!(meta.name, "age");
        assert_eq!(meta.data_type, DataType::Int);
        assert!(!meta.nullable);
        assert_eq!(meta.table_name, Some("users".into()));
    }

    #[test]
    fn test_column_meta_new_no_table() {
        let meta = ColumnMeta::new("score", DataType::Real, true, None);
        assert_eq!(meta.table_name, None);
        assert!(meta.nullable);
    }

    #[test]
    fn test_column_meta_computed() {
        let meta = ColumnMeta::computed("1 + 1", DataType::Int);
        assert_eq!(meta.name, "1 + 1");
        assert_eq!(meta.data_type, DataType::Int);
        assert!(meta.nullable);
        assert_eq!(meta.table_name, None);
    }

    #[test]
    fn test_column_meta_clone_eq() {
        let meta = ColumnMeta::new("id", DataType::BigInt, false, Some("orders".into()));
        assert_eq!(meta.clone(), meta);
    }

    // ── QueryResult::Rows ────────────────────────────────────────────────────

    #[test]
    fn test_empty_rows_has_columns_no_data() {
        let cols = vec![ColumnMeta::computed("x", DataType::Int)];
        let result = QueryResult::empty_rows(cols.clone());
        assert_eq!(result.row_count(), Some(0));
        assert_eq!(result.column_count(), Some(1));
        if let QueryResult::Rows { columns, rows } = &result {
            assert_eq!(columns, &cols);
            assert!(rows.is_empty());
        } else {
            panic!("expected Rows variant");
        }
    }

    #[test]
    fn test_rows_with_data() {
        let result = QueryResult::Rows {
            columns: vec![
                ColumnMeta::new("id", DataType::BigInt, false, Some("users".into())),
                ColumnMeta::new("email", DataType::Text, false, Some("users".into())),
            ],
            rows: vec![
                vec![Value::BigInt(1), Value::Text("alice@example.com".into())],
                vec![Value::BigInt(2), Value::Text("bob@example.com".into())],
                vec![Value::BigInt(3), Value::Text("carol@example.com".into())],
            ],
        };
        assert_eq!(result.row_count(), Some(3));
        assert_eq!(result.column_count(), Some(2));
    }

    #[test]
    fn test_rows_clone_eq() {
        let result = QueryResult::Rows {
            columns: vec![ColumnMeta::computed("n", DataType::Int)],
            rows: vec![vec![Value::Int(42)]],
        };
        assert_eq!(result.clone(), result);
    }

    // ── QueryResult::Affected ────────────────────────────────────────────────

    #[test]
    fn test_affected_no_id() {
        let result = QueryResult::affected(5);
        assert_eq!(
            result,
            QueryResult::Affected {
                count: 5,
                last_insert_id: None
            }
        );
        assert_eq!(result.row_count(), None);
        assert_eq!(result.column_count(), None);
    }

    #[test]
    fn test_affected_with_id() {
        let result = QueryResult::affected_with_id(1, 42);
        assert_eq!(
            result,
            QueryResult::Affected {
                count: 1,
                last_insert_id: Some(42)
            }
        );
    }

    #[test]
    fn test_affected_zero_rows() {
        // UPDATE that matches nothing: count = 0, no ID.
        let result = QueryResult::affected(0);
        assert_eq!(
            result,
            QueryResult::Affected {
                count: 0,
                last_insert_id: None
            }
        );
    }

    // ── QueryResult::Empty ───────────────────────────────────────────────────

    #[test]
    fn test_empty_variant() {
        let result = QueryResult::Empty;
        assert_eq!(result.row_count(), None);
        assert_eq!(result.column_count(), None);
        assert_eq!(result, QueryResult::Empty);
    }

    // ── Accessors ────────────────────────────────────────────────────────────

    #[test]
    fn test_row_count_rows() {
        let r = QueryResult::Rows {
            columns: vec![ColumnMeta::computed("x", DataType::Int)],
            rows: vec![
                vec![Value::Int(1)],
                vec![Value::Int(2)],
                vec![Value::Int(3)],
            ],
        };
        assert_eq!(r.row_count(), Some(3));
    }

    #[test]
    fn test_row_count_affected_is_none() {
        assert_eq!(QueryResult::affected(10).row_count(), None);
    }

    #[test]
    fn test_row_count_empty_is_none() {
        assert_eq!(QueryResult::Empty.row_count(), None);
    }

    #[test]
    fn test_column_count_rows() {
        let r = QueryResult::Rows {
            columns: vec![
                ColumnMeta::computed("a", DataType::Int),
                ColumnMeta::computed("b", DataType::Text),
            ],
            rows: vec![],
        };
        assert_eq!(r.column_count(), Some(2));
    }

    #[test]
    fn test_column_count_non_rows_is_none() {
        assert_eq!(QueryResult::affected(5).column_count(), None);
        assert_eq!(QueryResult::Empty.column_count(), None);
    }

    // ── Debug smoke tests ────────────────────────────────────────────────────

    #[test]
    fn test_debug_all_variants_no_panic() {
        let variants: Vec<QueryResult> = vec![
            QueryResult::Rows {
                columns: vec![ColumnMeta::computed("x", DataType::Int)],
                rows: vec![vec![Value::Int(1)]],
            },
            QueryResult::affected(3),
            QueryResult::affected_with_id(1, 99),
            QueryResult::Empty,
        ];
        for v in &variants {
            let _ = format!("{v:?}");
        }
    }
}
