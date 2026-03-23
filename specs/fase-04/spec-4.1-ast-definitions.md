# Spec: 4.1 — AST Definitions

## What to build (not how)

The complete SQL statement AST: all types that the parser (4.2–4.4) produces
and the executor (4.5) consumes. The AST captures semantic structure, not
concrete syntax — no source positions, no raw tokens.

`Expr` is already defined in Phase 4.17 (`axiomdb-sql/src/expr.rs`). This
phase defines `Stmt` and all the statement-level types that contain `Expr`.

---

## Naming: `ColumnDef` in AST vs catalog

`axiomdb-catalog::schema::ColumnDef` is the **stored** form (col_idx,
ColumnType, nullable). The AST `ColumnDef` is the **parsed** form (name,
DataType, constraints). Both use the same name in different crates — no
conflict. The executor converts between them.

---

## Types

All types live in `axiomdb-sql/src/ast.rs`.

### Base types

```rust
/// Qualified table reference with optional alias.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub schema: Option<String>,   // None = use default schema ("public")
    pub name: String,
    pub alias: Option<String>,    // AS alias
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder { Asc, Desc }

impl Default for SortOrder { fn default() -> Self { Self::Asc } }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder { First, Last }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType { Inner, Left, Right, Cross, Full }

#[derive(Debug, Clone, PartialEq)]
pub enum JoinCondition {
    On(Expr),
    Using(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignKeyAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl Default for ForeignKeyAction {
    fn default() -> Self { Self::NoAction }
}
```

### Column and constraint types

```rust
/// A column definition as it appears in CREATE TABLE or ALTER TABLE ADD COLUMN.
/// Different from `axiomdb_catalog::schema::ColumnDef` (the stored form).
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub constraints: Vec<ColumnConstraint>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraint {
    NotNull,
    Null,                         // explicit NULL (nullable)
    Default(Expr),
    Unique,
    PrimaryKey,
    AutoIncrement,               // MySQL AUTO_INCREMENT / SERIAL
    References {
        table: String,
        column: Option<String>,  // None = reference PK
        on_delete: ForeignKeyAction,
        on_update: ForeignKeyAction,
    },
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

/// A column listed in CREATE INDEX, with optional sort direction.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexColumn {
    pub name: String,
    pub order: SortOrder,
}

/// Assignment in an UPDATE statement: `col = expr`.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: String,
    pub value: Expr,
}

/// An item in the SELECT list.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,                                      // SELECT *
    QualifiedWildcard(String),                     // SELECT table.*
    Expr { expr: Expr, alias: Option<String> },   // SELECT expr [AS alias]
}

/// The FROM clause: a table or a subquery.
#[derive(Debug, Clone, PartialEq)]
pub enum FromClause {
    Table(TableRef),
    Subquery { query: Box<SelectStmt>, alias: String },
}

/// A JOIN clause attached to a SELECT.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    pub join_type: JoinType,
    pub table: FromClause,
    pub condition: JoinCondition,
}

/// An item in the ORDER BY clause.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub order: SortOrder,
    pub nulls: Option<NullsOrder>,
}
```

### SELECT

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub distinct: bool,
    pub columns: Vec<SelectItem>,
    pub from: Option<FromClause>,       // None → SELECT without FROM (4.5a)
    pub joins: Vec<JoinClause>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}
```

### DML

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub table: TableRef,
    pub columns: Option<Vec<String>>,    // None → all columns in schema order
    pub source: InsertSource,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),              // (1,'a'), (2,'b')
    Select(Box<SelectStmt>),             // INSERT INTO t SELECT ...
    DefaultValues,                        // INSERT INTO t DEFAULT VALUES
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub table: TableRef,
    pub assignments: Vec<Assignment>,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub table: TableRef,
    pub where_clause: Option<Expr>,
}
```

### DDL

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStmt {
    pub if_not_exists: bool,
    pub table: TableRef,
    pub columns: Vec<ColumnDef>,
    pub table_constraints: Vec<TableConstraint>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexStmt {
    pub if_not_exists: bool,
    pub unique: bool,
    pub name: String,
    pub table: TableRef,
    pub columns: Vec<IndexColumn>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropTableStmt {
    pub if_exists: bool,
    pub tables: Vec<TableRef>,          // DROP TABLE a, b, c
    pub cascade: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropIndexStmt {
    pub if_exists: bool,
    pub name: String,
    pub table: Option<TableRef>,        // MySQL: DROP INDEX name ON table
}

#[derive(Debug, Clone, PartialEq)]
pub struct TruncateTableStmt {
    pub table: TableRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterTableStmt {
    pub table: TableRef,
    pub operations: Vec<AlterTableOp>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableOp {
    AddColumn(ColumnDef),
    DropColumn { name: String, if_exists: bool },
    RenameColumn { old_name: String, new_name: String },
    RenameTable(String),
    AddConstraint(TableConstraint),
    DropConstraint { name: String },
    ModifyColumn(ColumnDef),            // MySQL MODIFY COLUMN
}
```

### Utility statements

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct ShowTablesStmt {
    pub schema: Option<String>,
}

/// SHOW COLUMNS FROM t  /  DESCRIBE t  /  DESC t
#[derive(Debug, Clone, PartialEq)]
pub struct ShowColumnsStmt {
    pub table: TableRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetStmt {
    pub variable: String,
    pub value: SetValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    Expr(Expr),
    Default,
}
```

### Stmt — the top-level union

```rust
/// A complete SQL statement as produced by the parser.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    // DML
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    // DDL
    CreateTable(CreateTableStmt),
    CreateIndex(CreateIndexStmt),
    DropTable(DropTableStmt),
    DropIndex(DropIndexStmt),
    TruncateTable(TruncateTableStmt),
    AlterTable(AlterTableStmt),
    // Introspection
    ShowTables(ShowTablesStmt),
    ShowColumns(ShowColumnsStmt),
    // Transaction
    Begin,
    Commit,
    Rollback,
    // Session
    Set(SetStmt),
}
```

---

## Acceptance criteria

- [ ] `TableRef` has `schema: Option<String>`, `name: String`, `alias: Option<String>`
- [ ] `SortOrder` defaults to `Asc`
- [ ] `ForeignKeyAction` defaults to `NoAction`
- [ ] `ColumnDef` (AST) has `name`, `data_type: DataType`, `constraints: Vec<ColumnConstraint>`
- [ ] `ColumnConstraint` has all 8 variants including `References` with `on_delete`/`on_update`
- [ ] `TableConstraint` has PrimaryKey, Unique, ForeignKey, Check
- [ ] `SelectStmt` has all 10 fields including `joins: Vec<JoinClause>` and `having: Option<Expr>`
- [ ] `SelectItem` has Wildcard, QualifiedWildcard, Expr variants
- [ ] `FromClause` supports Table and Subquery (boxed SelectStmt)
- [ ] `InsertSource` has Values, Select, DefaultValues
- [ ] `AlterTableOp` has all 7 variants including ModifyColumn
- [ ] `Stmt` has all 16 variants
- [ ] All types derive `Debug`, `Clone`, `PartialEq`
- [ ] `SortOrder` and `ForeignKeyAction` derive `Copy`
- [ ] Construction test: a realistic SELECT with WHERE, JOIN, ORDER BY compiles
- [ ] Construction test: a CREATE TABLE with constraints compiles
- [ ] No `unwrap()` in `src/` (trivially satisfied — pure data types)

---

## ⚠️ DEFERRED

- Source spans (byte offsets for error reporting) — Phase 4.2b (parser error messages)
- `CASE WHEN` in Expr — Phase 4.24 (adds `Expr::Case` variant)
- `RETURNING` clause in INSERT/UPDATE/DELETE — Phase 13.x
- `WITH` (CTE) — Phase 4.x
- `UNION` / `INTERSECT` / `EXCEPT` — Phase 4.x
- `WINDOW` clause — Phase 13.2
- `LOCK IN SHARE MODE` / `FOR UPDATE` — Phase 7.10
- `EXPLAIN` — Phase 12.2

---

## Out of scope

- Parsing SQL text into AST — Phase 4.2–4.4
- Semantic validation of the AST — Phase 4.18
- Executing the AST — Phase 4.5

---

## Dependencies

- `axiomdb-sql`: same crate, `ast.rs` alongside `expr.rs`
- `axiomdb-types`: `DataType` (used in `ColumnDef.data_type`)
- `axiomdb-sql/src/expr.rs`: `Expr` (used throughout)
- No new crate dependencies needed
