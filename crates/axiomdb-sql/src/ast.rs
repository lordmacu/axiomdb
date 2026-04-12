//! SQL statement AST — all statement-level types produced by the parser.
//!
//! [`Expr`] (Phase 4.17) represents expressions. This module defines [`Stmt`]
//! and all the statement structures that contain expressions.
//!
//! ## Design notes
//!
//! - No source positions — those belong in the parser's error layer.
//! - `ColumnDef` here is the *parsed* form; `axiomdb_catalog::schema::ColumnDef`
//!   is the *stored* form. The executor converts between them.
//! - `FromClause::Subquery` boxes `SelectStmt` to break the mutual recursion.

use axiomdb_types::DataType;

use crate::expr::Expr;

// ── Base types ────────────────────────────────────────────────────────────────

/// A qualified table reference with an optional alias.
///
/// Supports 1-part (`table`), 2-part (`schema.table`), and 3-part
/// (`database.schema.table`) names.
///
/// - `database` is `None` when the query omits the database prefix; the
///   executor substitutes the session's effective database.
/// - `schema` is `None` when the query omits the schema prefix; the executor
///   substitutes the session's default schema (typically `"public"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

impl TableRef {
    /// Shorthand constructor for unqualified, unaliased table references.
    pub fn simple(name: impl Into<String>) -> Self {
        Self {
            database: None,
            schema: None,
            name: name.into(),
            alias: None,
        }
    }
}

/// Sort direction for ORDER BY and index columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

impl Default for SortOrder {
    /// SQL default sort order is ascending.
    fn default() -> Self {
        Self::Asc
    }
}

/// NULL ordering for ORDER BY: `NULLS FIRST` or `NULLS LAST`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder {
    First,
    Last,
}

/// JOIN type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Cross,
    Full,
}

/// JOIN condition.
#[derive(Debug, Clone, PartialEq)]
pub enum JoinCondition {
    /// `ON expr`
    On(Expr),
    /// `USING (col1, col2, ...)`
    Using(Vec<String>),
}

/// Action taken on a referenced row when a FK constraint fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignKeyAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl Default for ForeignKeyAction {
    /// SQL default FK action is `NO ACTION`.
    fn default() -> Self {
        Self::NoAction
    }
}

// ── Column and constraint types ───────────────────────────────────────────────

/// Column definition as it appears in `CREATE TABLE` or `ALTER TABLE ADD COLUMN`.
///
/// Different from `axiomdb_catalog::schema::ColumnDef` (the disk-stored form
/// with `col_idx` and `ColumnType`). The executor converts between the two.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub constraints: Vec<ColumnConstraint>,
    /// Maximum character length for `VARCHAR(N)` / `CHAR(N)` columns.
    /// `0` means unbounded (no length constraint). Mirrors `CatalogColumnDef::type_len`.
    pub type_len: u16,
    /// `true` when the column was declared `CHAR(N)` (fixed-length).
    /// `false` for `VARCHAR(N)` and `TEXT`.
    pub is_char: bool,
}

/// Inline column constraint in a column definition.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraint {
    /// `NOT NULL`
    NotNull,
    /// `NULL` (explicit nullable)
    Null,
    /// `DEFAULT expr`
    Default(Expr),
    /// `UNIQUE`
    Unique,
    /// `PRIMARY KEY`
    PrimaryKey,
    /// `AUTO_INCREMENT` (MySQL) or `SERIAL` (PostgreSQL-compat)
    AutoIncrement,
    /// `REFERENCES table [(column)] [ON DELETE action] [ON UPDATE action]`
    References {
        table: String,
        column: Option<String>,
        on_delete: ForeignKeyAction,
        on_update: ForeignKeyAction,
    },
    /// `CHECK (expr)`
    Check(Expr),
    /// `ON UPDATE expr` — auto-refresh this column on every UPDATE when it
    /// was not explicitly assigned. Classic MySQL pattern:
    /// `updated_at TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`.
    OnUpdate(Expr),
}

/// Table-level constraint declared after the column list.
#[derive(Debug, Clone, PartialEq)]
pub enum TableConstraint {
    PrimaryKey {
        name: Option<String>,
        columns: Vec<String>,
    },
    Unique {
        name: Option<String>,
        columns: Vec<String>,
    },
    ForeignKey {
        name: Option<String>,
        columns: Vec<String>,
        ref_table: String,
        ref_columns: Vec<String>,
        on_delete: ForeignKeyAction,
        on_update: ForeignKeyAction,
    },
    Check {
        name: Option<String>,
        expr: Expr,
    },
    /// Non-unique index defined inline in a CREATE TABLE column list.
    ///
    /// `INDEX idx_name (col1, col2)` and `KEY idx_name (col1, col2)` are MySQL
    /// extensions equivalent to a separate `CREATE INDEX` statement.
    Index {
        name: Option<String>,
        columns: Vec<String>,
    },
}

/// A column listed in `CREATE INDEX`, with optional sort direction.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexColumn {
    pub name: String,
    pub order: SortOrder,
}

/// `col = expr` assignment in an `UPDATE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: String,
    pub value: Expr,
}

// ── SELECT types ──────────────────────────────────────────────────────────────

/// An item in the `SELECT` list.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `SELECT *`
    Wildcard,
    /// `SELECT table.*`
    QualifiedWildcard(String),
    /// `SELECT expr [AS alias]`
    Expr { expr: Expr, alias: Option<String> },
}

/// The `FROM` source: a table or a subquery.
#[derive(Debug, Clone, PartialEq)]
pub enum FromClause {
    Table(TableRef),
    /// `(SELECT ...) AS alias` — boxed to break mutual recursion with SelectStmt.
    Subquery {
        query: Box<SelectStmt>,
        alias: String,
    },
}

/// A `JOIN` attached to a `SELECT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    pub join_type: JoinType,
    pub table: FromClause,
    pub condition: JoinCondition,
}

/// An item in the `ORDER BY` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub order: SortOrder,
    pub nulls: Option<NullsOrder>,
}

// ── SELECT statement ──────────────────────────────────────────────────────────

/// A `SELECT` statement.
///
/// Set operator kind for `Stmt::SetOp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

/// One tail element in a set-operation chain.
#[derive(Debug, Clone, PartialEq)]
pub struct SetOpTail {
    pub kind: SetOpKind,
    /// `true` = ALL variant (keeps duplicates per postgres semantics).
    pub all: bool,
    pub select: SelectStmt,
}

/// `from` is `None` for `SELECT` without `FROM` (e.g. `SELECT 1`, `SELECT NOW()`).
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub distinct: bool,
    /// `SQL_CALC_FOUND_ROWS` modifier: stash pre-LIMIT count for `FOUND_ROWS()` (4.5e).
    pub calc_found_rows: bool,
    pub columns: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub joins: Vec<JoinClause>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    /// `true` when `GROUP BY ... WITH ROLLUP` was specified (GAP-C.5).
    /// Produces one subtotal row per grouping level with NULL in the rolled-up keys,
    /// plus a grand-total row with NULL in every group-by column.
    pub with_rollup: bool,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    /// Row-level lock mode: `FOR UPDATE` or `LOCK IN SHARE MODE` (ignored until Phase 13.7).
    pub lock_mode: Option<LockMode>,
}

// ── DML statements ────────────────────────────────────────────────────────────

/// Source of rows for an `INSERT` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    /// `VALUES (row1), (row2), ...`
    Values(Vec<Vec<Expr>>),
    /// `INSERT INTO t SELECT ...`
    Select(Box<SelectStmt>),
    /// `INSERT INTO t DEFAULT VALUES`
    DefaultValues,
}

/// An `INSERT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub table: TableRef,
    /// Column list after the table name. `None` means all columns in schema order.
    pub columns: Option<Vec<String>>,
    pub source: InsertSource,
    /// `INSERT IGNORE` — skip rows that would cause a constraint violation.
    pub ignore: bool,
    /// `REPLACE INTO` (MySQL upsert). Before each row insert, every
    /// conflicting row on any PK/UNIQUE index is deleted — FK cascade
    /// included. `ignore` and `replace` are mutually exclusive.
    pub replace: bool,
}

/// An `UPDATE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub table: TableRef,
    /// Optional MySQL multi-table join source: `UPDATE t JOIN u ON ... SET ...`.
    pub joins: Vec<JoinClause>,
    pub assignments: Vec<Assignment>,
    pub where_clause: Option<Expr>,
    /// `UPDATE ... ORDER BY col [ASC|DESC]` — sort candidates before applying.
    pub order_by: Vec<OrderByItem>,
    /// `UPDATE ... LIMIT N` — cap the number of rows updated.
    pub limit: Option<Expr>,
}

/// A `DELETE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub table: TableRef,
    /// Optional MySQL target before FROM: `DELETE t FROM t JOIN u ON ...`.
    pub target: Option<String>,
    /// Optional MySQL multi-table join source after FROM.
    pub joins: Vec<JoinClause>,
    pub where_clause: Option<Expr>,
    /// `DELETE ... ORDER BY col [ASC|DESC]` — sort candidates before deleting.
    pub order_by: Vec<OrderByItem>,
    /// `DELETE ... LIMIT N` — cap the number of rows deleted.
    pub limit: Option<Expr>,
}

/// Row-level lock mode for `SELECT ... FOR UPDATE` / `LOCK IN SHARE MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// `FOR UPDATE` — exclusive write lock (ignored until Phase 13.7).
    ForUpdate,
    /// `LOCK IN SHARE MODE` — shared read lock (ignored until Phase 13.7).
    ShareMode,
}

/// `CREATE TABLE new_table LIKE source_table` — copy schema without data.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableLikeStmt {
    pub if_not_exists: bool,
    pub new_table: TableRef,
    pub source_table: TableRef,
}

/// `CREATE TABLE new_table AS SELECT ...` — derive schema from query result.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableAsSelectStmt {
    pub new_table: TableRef,
    pub select: SelectStmt,
}

// ── DDL statements ────────────────────────────────────────────────────────────

/// `CREATE TABLE`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStmt {
    pub if_not_exists: bool,
    pub table: TableRef,
    pub columns: Vec<ColumnDef>,
    pub table_constraints: Vec<TableConstraint>,
    /// `CREATE TABLE ... IMMUTABLE` (Phase 13.9) — rejects any UPDATE/DELETE.
    pub immutable: bool,
}

/// Index access method (Phase 11.1b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexType {
    /// B-Tree (default) — point lookups, range scans, uniqueness enforcement.
    #[default]
    BTree,
    /// BRIN (Block Range INdex) — per-block-range min/max summaries.
    /// 100× smaller than B-Tree for naturally ordered columns (timestamps, IDs).
    /// PostgreSQL-compatible: `CREATE INDEX ... USING brin`.
    Brin,
    /// Trigram — 3-character n-gram inverted index for substring search.
    /// `WHERE col LIKE '%pattern%'` uses trigram index to narrow candidates.
    /// Built-in (PostgreSQL requires pg_trgm extension).
    Trigram,
    /// Full Text Search — inverted index with BM25 ranking (Phase 11.6).
    /// `WHERE MATCH(col, 'query terms')` searches with relevance scoring.
    /// Built-in (PostgreSQL requires tsearch, MySQL uses FULLTEXT).
    FullText,
    /// GIN — Generalized Inverted Index for JSONB containment (Phase 11.17).
    /// `CREATE INDEX ... USING gin (col)` on a JSONB column.
    /// Enables `col @> '{"key":"val"}'` with O(log n) index lookup.
    Gin,
}

/// `CREATE [UNIQUE] INDEX`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexStmt {
    pub if_not_exists: bool,
    pub unique: bool,
    pub name: String,
    pub table: TableRef,
    pub columns: Vec<IndexColumn>,
    /// Optional WHERE predicate for partial indexes (Phase 6.7).
    /// `None` = full index (covers all rows).
    pub predicate: Option<crate::expr::Expr>,
    /// Target leaf-page fill factor (Phase 6.8). `None` → default 90.
    /// Valid range: 10–100.
    pub fillfactor: Option<u8>,
    /// INCLUDE columns for covering indexes (Phase 6.13). `vec![]` = no included cols.
    pub include_columns: Vec<String>,
    /// Index access method (Phase 11.1b). Default: BTree.
    pub index_type: IndexType,
    /// BRIN: heap pages per range. Default 128. Ignored for B-Tree.
    pub pages_per_range: Option<u32>,
}

/// `DROP TABLE`
#[derive(Debug, Clone, PartialEq)]
pub struct DropTableStmt {
    pub if_exists: bool,
    /// Multiple tables can be dropped in one statement: `DROP TABLE a, b, c`.
    pub tables: Vec<TableRef>,
    pub cascade: bool,
}

/// `DROP INDEX`
#[derive(Debug, Clone, PartialEq)]
pub struct DropIndexStmt {
    pub if_exists: bool,
    pub name: String,
    /// MySQL requires `ON table`: `DROP INDEX idx ON users`.
    pub table: Option<TableRef>,
}

/// `TRUNCATE TABLE`
#[derive(Debug, Clone, PartialEq)]
pub struct TruncateTableStmt {
    pub table: TableRef,
}

/// `ANALYZE [TABLE table_name [(column_name)]]` — refresh per-column statistics (Phase 6.12).
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeStmt {
    /// `None` = analyze all tables in the current schema.
    /// `Some(name)` = analyze a specific table.
    pub table: Option<String>,
    /// `None` = all indexed columns.
    /// `Some(name)` = a specific column only.
    pub column: Option<String>,
}

/// `VACUUM [table_name]` — remove dead rows and dead index entries (Phase 7.11).
#[derive(Debug, Clone, PartialEq)]
pub struct VacuumStmt {
    /// `None` = vacuum all tables in the current database.
    pub table: Option<TableRef>,
}

/// `ALTER TABLE` operation.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableOp {
    AddColumn(ColumnDef),
    DropColumn {
        name: String,
        if_exists: bool,
    },
    RenameColumn {
        old_name: String,
        new_name: String,
    },
    /// `RENAME TO new_name`
    RenameTable(String),
    AddConstraint(TableConstraint),
    DropConstraint {
        name: String,
        if_exists: bool,
    },
    /// MySQL `MODIFY COLUMN col new_type [constraints]`
    ModifyColumn(ColumnDef),
    /// `REBUILD` — convert heap table to clustered format (Phase 39.19).
    Rebuild,
    /// `RENAME INDEX old TO new` (4.22g)
    RenameIndex {
        old_name: String,
        new_name: String,
    },
    /// `CONVERT TO CHARACTER SET charset [COLLATE collation]` (4.22i) — accepted, no-op.
    ConvertCharset,
    /// `ADD [UNIQUE] [INDEX|KEY] [name] (col [, col]*)` (4.22h)
    AddIndex {
        unique: bool,
        name: Option<String>,
        columns: Vec<String>,
    },
    /// `DROP INDEX name` within ALTER TABLE (4.22h)
    DropIndex {
        name: String,
    },
    /// `CHANGE [COLUMN] old_name new_col_def` (4.22h) — rename + retype in one op.
    ChangeColumn {
        old_name: String,
        new_def: ColumnDef,
    },
    /// `AUTO_INCREMENT = N` (4.22h) — reset the auto-increment counter.
    /// Not yet persisted (counter storage in 4.18e); accepted as a no-op.
    SetAutoIncrement(u64),
    /// `ENGINE = name` (4.22h) — accepted, ignored.
    SetEngine,
}

/// `ALTER TABLE`
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTableStmt {
    pub table: TableRef,
    pub operations: Vec<AlterTableOp>,
}

// ── Utility statements ────────────────────────────────────────────────────────

/// `SHOW TABLES [FROM schema]`
#[derive(Debug, Clone, PartialEq)]
pub struct ShowTablesStmt {
    pub schema: Option<String>,
    /// `SHOW FULL TABLES` — adds `Table_type` column (BASE TABLE | VIEW).
    pub full: bool,
}

/// `SHOW DATABASES`
#[derive(Debug, Clone, PartialEq)]
pub struct ShowDatabasesStmt;

/// `SHOW COLUMNS FROM table` / `DESCRIBE table` / `DESC table`
#[derive(Debug, Clone, PartialEq)]
pub struct ShowColumnsStmt {
    pub table: TableRef,
    /// `SHOW FULL COLUMNS` — adds Collation, Privileges, Comment columns.
    pub full: bool,
}

/// `SHOW TABLE STATUS [FROM db] [LIKE pattern]`
#[derive(Debug, Clone, PartialEq)]
pub struct ShowTableStatusStmt {
    pub schema: Option<String>,
    pub like_pattern: Option<String>,
}

/// `SHOW INDEX FROM table` / `SHOW INDEXES FROM table` / `SHOW KEYS FROM table`
#[derive(Debug, Clone, PartialEq)]
pub struct ShowIndexStmt {
    pub table: TableRef,
}

/// `SHOW CREATE TABLE table` — reconstruct the DDL for a table.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowCreateTableStmt {
    pub table: TableRef,
}

/// `RENAME TABLE old TO new [, old2 TO new2 ...]`
#[derive(Debug, Clone, PartialEq)]
pub struct RenameTableStmt {
    /// One or more (old_name, new_name) pairs.
    pub pairs: Vec<(String, String)>,
}

/// Value assigned in a `SET` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    Expr(Expr),
    Default,
}

/// `SET variable = value`
#[derive(Debug, Clone, PartialEq)]
pub struct SetStmt {
    pub variable: String,
    pub value: SetValue,
}

/// `CREATE DATABASE name`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateDatabaseStmt {
    pub name: String,
}

/// `DROP DATABASE [IF EXISTS] name`
#[derive(Debug, Clone, PartialEq)]
pub struct DropDatabaseStmt {
    pub if_exists: bool,
    pub name: String,
}

/// `USE name`
#[derive(Debug, Clone, PartialEq)]
pub struct UseDatabaseStmt {
    pub name: String,
}

/// `CREATE SCHEMA [IF NOT EXISTS] name` (Phase 22b.4)
#[derive(Debug, Clone, PartialEq)]
pub struct CreateSchemaStmt {
    pub name: String,
    pub if_not_exists: bool,
}

// ── Stmt ──────────────────────────────────────────────────────────────────────

/// A complete SQL statement as produced by the parser.
///
/// Some variants (e.g. `Select`) hold large structs while transaction control
/// variants (`Begin`, `Commit`, `Rollback`) are unit variants. The size
/// difference is intentional for an AST — we prefer ergonomic construction
/// over memory uniformity. Values are typically heap-allocated by the parser.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Stmt {
    // DML
    Select(SelectStmt),
    /// `SELECT ... (UNION|INTERSECT|EXCEPT) [ALL] SELECT ...` — set operation chain.
    ///
    /// Left-associative application of `rest` against `first`. Each tail
    /// carries its own operator kind and ALL flag. INTERSECT binds tighter
    /// than UNION/EXCEPT per SQL std — enforced during parsing by grouping
    /// INTERSECT chains into nested SetOp trees via an intermediate SELECT
    /// wrapper if needed (current impl folds left-to-right; mix precedence
    /// is MySQL-compatible).
    SetOp {
        first: SelectStmt,
        rest: Vec<SetOpTail>,
    },
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    /// `CALL proc(args)` — MySQL stored procedure call; executes as Noop (Phase 17+).
    Call {
        name: String,
        args: Vec<Expr>,
    },
    /// `DO expr` — MySQL expression discard; executes as Noop.
    Do {
        expr: Expr,
    },
    // DDL
    CreateTable(CreateTableStmt),
    /// `CREATE TABLE new LIKE src` — copy schema without data.
    CreateTableLike(CreateTableLikeStmt),
    /// `CREATE TABLE new AS SELECT ...` — derive schema + populate from query.
    CreateTableAsSelect(CreateTableAsSelectStmt),
    CreateDatabase(CreateDatabaseStmt),
    CreateSchema(CreateSchemaStmt),
    CreateIndex(CreateIndexStmt),
    DropTable(DropTableStmt),
    DropDatabase(DropDatabaseStmt),
    DropIndex(DropIndexStmt),
    TruncateTable(TruncateTableStmt),
    AlterTable(AlterTableStmt),
    /// `ANALYZE [TABLE name [(col)]]` — refresh per-column statistics (Phase 6.12).
    Analyze(AnalyzeStmt),
    // Introspection
    ShowTables(ShowTablesStmt),
    ShowDatabases(ShowDatabasesStmt),
    ShowColumns(ShowColumnsStmt),
    ShowIndex(ShowIndexStmt),
    /// `SHOW CREATE TABLE t` — reconstruct DDL (4.20b).
    ShowCreateTable(ShowCreateTableStmt),
    /// `SHOW TABLE STATUS [FROM db] [LIKE pattern]` (5.9f).
    ShowTableStatus(ShowTableStatusStmt),
    /// `SHOW ENGINES` — static engine list (5.9g).
    ShowEngines,
    /// `SHOW CHARSET` / `SHOW CHARACTER SET` — static charset list (5.9g).
    ShowCharset,
    /// `SHOW COLLATION` — static collation list (5.9g).
    ShowCollation,
    /// `SHOW VARIABLES` — static variable dump (intercepted by wire handler).
    ShowVariables,
    /// `SHOW STATUS` — intercepted by wire handler.
    ShowStatus,
    /// `SHOW WARNINGS [LIMIT n]` — returns per-session warning list (5.9e).
    ShowWarnings {
        limit: Option<u64>,
    },
    /// `SHOW ERRORS [LIMIT n]` — returns per-session error list (5.9e).
    ShowErrors {
        limit: Option<u64>,
    },
    /// `RENAME TABLE a TO b [, c TO d ...]` (4.3h).
    RenameTable(RenameTableStmt),
    // Transaction control
    Begin,
    Commit,
    Rollback,
    /// `SAVEPOINT name` — create a named savepoint within an explicit transaction.
    Savepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] name` — undo changes back to the named savepoint.
    RollbackToSavepoint(String),
    /// `RELEASE [SAVEPOINT] name` — destroy the named savepoint (changes persist).
    ReleaseSavepoint(String),
    // Maintenance
    /// `VACUUM [table_name]` — remove dead rows and dead index entries (Phase 7.11).
    Vacuum(VacuumStmt),
    /// `EXPLAIN SELECT ...` — show the chosen query plan (Phase 8.4).
    Explain(Box<Stmt>),
    // Session
    Set(SetStmt),
    UseDatabase(UseDatabaseStmt),
    /// A parsed but semantically empty statement.
    ///
    /// Used for MySQL compatibility statements that must parse cleanly but
    /// require no action: `LOCK TABLES`, `UNLOCK TABLES`, `FLUSH TABLES`,
    /// `KILL [QUERY|CONNECTION] id`, etc.
    Noop,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::{BinaryOp, UnaryOp};
    use axiomdb_types::Value;

    // Convenience: column reference by index
    fn col(idx: usize, name: &str) -> Expr {
        Expr::Column {
            col_idx: idx,
            name: name.into(),
        }
    }

    #[test]
    fn test_select_star_from_table_with_where_and_order() {
        let stmt = Stmt::Select(SelectStmt {
            distinct: false,
            calc_found_rows: false,
            columns: vec![SelectItem::Wildcard],
            from: Some(FromClause::Table(TableRef::simple("users"))),
            joins: vec![],
            where_clause: Some(Expr::binop(BinaryOp::Gt, col(0, "age"), Expr::int(18))),
            group_by: vec![],
            with_rollup: false,
            having: None,
            order_by: vec![OrderByItem {
                expr: col(1, "name"),
                order: SortOrder::Asc,
                nulls: None,
            }],
            limit: Some(Expr::int(10)),
            offset: Some(Expr::int(0)),
            lock_mode: None,
        });
        assert!(matches!(stmt, Stmt::Select(_)));
    }

    #[test]
    fn test_select_without_from() {
        // SELECT 1  — health-check query used by ORMs
        let stmt = Stmt::Select(SelectStmt {
            distinct: false,
            calc_found_rows: false,
            columns: vec![SelectItem::Expr {
                expr: Expr::int(1),
                alias: None,
            }],
            from: None,
            joins: vec![],
            where_clause: None,
            group_by: vec![],
            with_rollup: false,
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
        });
        if let Stmt::Select(s) = stmt {
            assert!(s.from.is_none());
        } else {
            panic!("expected Stmt::Select");
        }
    }

    #[test]
    fn test_create_table_with_pk_and_unique() {
        let stmt = Stmt::CreateTable(CreateTableStmt {
            if_not_exists: true,
            table: TableRef {
                database: None,
                schema: Some("public".into()),
                name: "users".into(),
                alias: None,
            },
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: DataType::BigInt,
                    constraints: vec![
                        ColumnConstraint::PrimaryKey,
                        ColumnConstraint::AutoIncrement,
                    ],
                    type_len: 0,
                    is_char: false,
                },
                ColumnDef {
                    name: "email".into(),
                    data_type: DataType::Text,
                    constraints: vec![ColumnConstraint::NotNull, ColumnConstraint::Unique],
                    type_len: 0,
                    is_char: false,
                },
                ColumnDef {
                    name: "age".into(),
                    data_type: DataType::Int,
                    constraints: vec![ColumnConstraint::Default(Expr::int(0))],
                    type_len: 0,
                    is_char: false,
                },
            ],
            table_constraints: vec![],
            immutable: false,
        });
        assert!(matches!(stmt, Stmt::CreateTable(_)));
    }

    #[test]
    fn test_create_table_with_table_constraints() {
        let stmt = Stmt::CreateTable(CreateTableStmt {
            if_not_exists: false,
            table: TableRef::simple("orders"),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: DataType::BigInt,
                    constraints: vec![ColumnConstraint::NotNull],
                    type_len: 0,
                    is_char: false,
                },
                ColumnDef {
                    name: "user_id".into(),
                    data_type: DataType::BigInt,
                    constraints: vec![ColumnConstraint::NotNull],
                    type_len: 0,
                    is_char: false,
                },
            ],
            table_constraints: vec![
                TableConstraint::PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                },
                TableConstraint::ForeignKey {
                    name: Some("fk_orders_user".into()),
                    columns: vec!["user_id".into()],
                    ref_table: "users".into(),
                    ref_columns: vec!["id".into()],
                    on_delete: ForeignKeyAction::Cascade,
                    on_update: ForeignKeyAction::NoAction,
                },
            ],
            immutable: false,
        });
        if let Stmt::CreateTable(ct) = stmt {
            assert_eq!(ct.table_constraints.len(), 2);
        } else {
            panic!("expected Stmt::CreateTable");
        }
    }

    #[test]
    fn test_insert_multiple_rows() {
        let stmt = Stmt::Insert(InsertStmt {
            table: TableRef::simple("users"),
            columns: Some(vec!["id".into(), "name".into()]),
            source: InsertSource::Values(vec![
                vec![Expr::int(1), Expr::text("Alice")],
                vec![Expr::int(2), Expr::text("Bob")],
            ]),
            ignore: false,
            replace: false,
        });
        if let Stmt::Insert(ins) = stmt {
            if let InsertSource::Values(rows) = &ins.source {
                assert_eq!(rows.len(), 2);
            } else {
                panic!("expected Values");
            }
        } else {
            panic!("expected Stmt::Insert");
        }
    }

    #[test]
    fn test_update_with_where() {
        let stmt = Stmt::Update(UpdateStmt {
            table: TableRef::simple("users"),
            assignments: vec![
                Assignment {
                    column: "name".into(),
                    value: Expr::text("Charlie"),
                },
                Assignment {
                    column: "age".into(),
                    value: Expr::binop(BinaryOp::Add, col(0, "age"), Expr::int(1)),
                },
            ],
            where_clause: Some(Expr::binop(BinaryOp::Eq, col(1, "id"), Expr::int(42))),
            joins: vec![],
            order_by: vec![],
            limit: None,
        });
        assert!(matches!(stmt, Stmt::Update(_)));
    }

    #[test]
    fn test_delete_with_where() {
        let stmt = Stmt::Delete(DeleteStmt {
            table: TableRef::simple("users"),
            target: None,
            joins: vec![],
            where_clause: Some(Expr::IsNull {
                expr: Box::new(col(0, "email")),
                negated: false,
            }),
            order_by: vec![],
            limit: None,
        });
        assert!(matches!(stmt, Stmt::Delete(_)));
    }

    #[test]
    fn test_join_clause_construction() {
        let join = JoinClause {
            join_type: JoinType::Left,
            table: FromClause::Table(TableRef {
                database: None,
                schema: None,
                name: "orders".into(),
                alias: Some("o".into()),
            }),
            condition: JoinCondition::On(Expr::binop(
                BinaryOp::Eq,
                col(0, "u.id"),
                col(1, "o.user_id"),
            )),
        };
        assert_eq!(join.join_type, JoinType::Left);
    }

    #[test]
    fn test_subquery_from_clause() {
        let subquery = SelectStmt {
            distinct: false,
            calc_found_rows: false,
            columns: vec![SelectItem::Wildcard],
            from: Some(FromClause::Table(TableRef::simple("users"))),
            joins: vec![],
            where_clause: None,
            group_by: vec![],
            with_rollup: false,
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
        };
        let from = FromClause::Subquery {
            query: Box::new(subquery),
            alias: "sub".into(),
        };
        assert!(matches!(from, FromClause::Subquery { .. }));
    }

    #[test]
    fn test_transaction_stmts() {
        assert!(matches!(Stmt::Begin, Stmt::Begin));
        assert!(matches!(Stmt::Commit, Stmt::Commit));
        assert!(matches!(Stmt::Rollback, Stmt::Rollback));
    }

    #[test]
    fn test_alter_table_multiple_ops() {
        let stmt = Stmt::AlterTable(AlterTableStmt {
            table: TableRef::simple("users"),
            operations: vec![
                AlterTableOp::AddColumn(ColumnDef {
                    name: "phone".into(),
                    data_type: DataType::Text,
                    constraints: vec![ColumnConstraint::Null],
                    type_len: 0,
                    is_char: false,
                }),
                AlterTableOp::DropColumn {
                    name: "legacy_col".into(),
                    if_exists: true,
                },
                AlterTableOp::RenameColumn {
                    old_name: "fname".into(),
                    new_name: "first_name".into(),
                },
            ],
        });
        if let Stmt::AlterTable(at) = stmt {
            assert_eq!(at.operations.len(), 3);
        } else {
            panic!("expected Stmt::AlterTable");
        }
    }

    #[test]
    fn test_drop_table_multiple() {
        let stmt = Stmt::DropTable(DropTableStmt {
            if_exists: true,
            tables: vec![TableRef::simple("a"), TableRef::simple("b")],
            cascade: false,
        });
        if let Stmt::DropTable(dt) = stmt {
            assert_eq!(dt.tables.len(), 2);
        } else {
            panic!("expected Stmt::DropTable");
        }
    }

    #[test]
    fn test_create_index() {
        let stmt = Stmt::CreateIndex(CreateIndexStmt {
            if_not_exists: false,
            unique: true,
            name: "users_email_idx".into(),
            table: TableRef::simple("users"),
            columns: vec![IndexColumn {
                name: "email".into(),
                order: SortOrder::Asc,
            }],
            predicate: None,
            fillfactor: None,
            include_columns: vec![],
            index_type: IndexType::BTree,
            pages_per_range: None,
        });
        assert!(matches!(stmt, Stmt::CreateIndex(_)));
    }

    #[test]
    fn test_set_stmt() {
        let stmt = Stmt::Set(SetStmt {
            variable: "autocommit".into(),
            value: SetValue::Expr(Expr::Literal(Value::Int(0))),
        });
        assert!(matches!(stmt, Stmt::Set(_)));
    }

    #[test]
    fn test_show_tables_and_columns() {
        let show_tables = Stmt::ShowTables(ShowTablesStmt {
            schema: None,
            full: false,
        });
        let show_cols = Stmt::ShowColumns(ShowColumnsStmt {
            table: TableRef::simple("users"),
            full: false,
        });
        assert!(matches!(show_tables, Stmt::ShowTables(_)));
        assert!(matches!(show_cols, Stmt::ShowColumns(_)));
    }

    #[test]
    fn test_nulls_order_in_order_by() {
        let item = OrderByItem {
            expr: col(0, "price"),
            order: SortOrder::Desc,
            nulls: Some(NullsOrder::Last),
        };
        assert_eq!(item.order, SortOrder::Desc);
        assert_eq!(item.nulls, Some(NullsOrder::Last));
    }

    #[test]
    fn test_sort_order_default_is_asc() {
        assert_eq!(SortOrder::default(), SortOrder::Asc);
    }

    #[test]
    fn test_fk_action_default_is_no_action() {
        assert_eq!(ForeignKeyAction::default(), ForeignKeyAction::NoAction);
    }

    #[test]
    fn test_unary_neg_in_default_expr() {
        // DEFAULT -1 in a column constraint
        let col_def = ColumnDef {
            name: "balance".into(),
            data_type: DataType::Int,
            constraints: vec![ColumnConstraint::Default(Expr::UnaryOp {
                op: UnaryOp::Neg,
                operand: Box::new(Expr::int(1)),
            })],
            type_len: 0,
            is_char: false,
        };
        assert_eq!(col_def.name, "balance");
    }
}
