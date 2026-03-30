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
/// `from` is `None` for `SELECT` without `FROM` (e.g. `SELECT 1`, `SELECT NOW()`).
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub distinct: bool,
    pub columns: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub joins: Vec<JoinClause>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
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
}

/// An `UPDATE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub table: TableRef,
    pub assignments: Vec<Assignment>,
    pub where_clause: Option<Expr>,
}

/// A `DELETE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub table: TableRef,
    pub where_clause: Option<Expr>,
}

// ── DDL statements ────────────────────────────────────────────────────────────

/// `CREATE TABLE`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStmt {
    pub if_not_exists: bool,
    pub table: TableRef,
    pub columns: Vec<ColumnDef>,
    pub table_constraints: Vec<TableConstraint>,
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
}

/// `SHOW DATABASES`
#[derive(Debug, Clone, PartialEq)]
pub struct ShowDatabasesStmt;

/// `SHOW COLUMNS FROM table` / `DESCRIBE table` / `DESC table`
#[derive(Debug, Clone, PartialEq)]
pub struct ShowColumnsStmt {
    pub table: TableRef,
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
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    // DDL
    CreateTable(CreateTableStmt),
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
    // Session
    Set(SetStmt),
    UseDatabase(UseDatabaseStmt),
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
            columns: vec![SelectItem::Wildcard],
            from: Some(FromClause::Table(TableRef::simple("users"))),
            joins: vec![],
            where_clause: Some(Expr::binop(BinaryOp::Gt, col(0, "age"), Expr::int(18))),
            group_by: vec![],
            having: None,
            order_by: vec![OrderByItem {
                expr: col(1, "name"),
                order: SortOrder::Asc,
                nulls: None,
            }],
            limit: Some(Expr::int(10)),
            offset: Some(Expr::int(0)),
        });
        assert!(matches!(stmt, Stmt::Select(_)));
    }

    #[test]
    fn test_select_without_from() {
        // SELECT 1  — health-check query used by ORMs
        let stmt = Stmt::Select(SelectStmt {
            distinct: false,
            columns: vec![SelectItem::Expr {
                expr: Expr::int(1),
                alias: None,
            }],
            from: None,
            joins: vec![],
            where_clause: None,
            group_by: vec![],
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
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
                },
                ColumnDef {
                    name: "email".into(),
                    data_type: DataType::Text,
                    constraints: vec![ColumnConstraint::NotNull, ColumnConstraint::Unique],
                },
                ColumnDef {
                    name: "age".into(),
                    data_type: DataType::Int,
                    constraints: vec![ColumnConstraint::Default(Expr::int(0))],
                },
            ],
            table_constraints: vec![],
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
                },
                ColumnDef {
                    name: "user_id".into(),
                    data_type: DataType::BigInt,
                    constraints: vec![ColumnConstraint::NotNull],
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
        });
        assert!(matches!(stmt, Stmt::Update(_)));
    }

    #[test]
    fn test_delete_with_where() {
        let stmt = Stmt::Delete(DeleteStmt {
            table: TableRef::simple("users"),
            where_clause: Some(Expr::IsNull {
                expr: Box::new(col(0, "email")),
                negated: false,
            }),
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
            columns: vec![SelectItem::Wildcard],
            from: Some(FromClause::Table(TableRef::simple("users"))),
            joins: vec![],
            where_clause: None,
            group_by: vec![],
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
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
        let show_tables = Stmt::ShowTables(ShowTablesStmt { schema: None });
        let show_cols = Stmt::ShowColumns(ShowColumnsStmt {
            table: TableRef::simple("users"),
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
        };
        assert_eq!(col_def.name, "balance");
    }
}
