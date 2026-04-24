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

use axiomdb_catalog::TablePersistence;
use axiomdb_core::error::DbError;
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

impl SelectStmt {
    pub fn has_hash_join_hint(&self) -> bool {
        self.hints
            .iter()
            .any(|hint| matches!(hint, SelectHint::HashJoin))
    }

    pub fn parallel_hint_workers(&self) -> Option<usize> {
        self.hints.iter().find_map(|hint| match hint {
            SelectHint::Parallel { workers } => Some(*workers),
            _ => None,
        })
    }

    pub fn hinted_index_name_for_table<'a>(
        &'a self,
        table_ref: &TableRef,
        resolved_table_name: &str,
    ) -> Result<Option<&'a str>, DbError> {
        let mut matched = None;
        for hint in &self.hints {
            let SelectHint::Index { table, index } = hint else {
                continue;
            };
            let table_matches = table.eq_ignore_ascii_case(resolved_table_name)
                || table.eq_ignore_ascii_case(&table_ref.name)
                || table_ref
                    .alias
                    .as_deref()
                    .map(|alias| table.eq_ignore_ascii_case(alias))
                    .unwrap_or(false);
            if !table_matches {
                continue;
            }
            if matched.replace(index.as_str()).is_some() {
                return Err(DbError::Other(format!(
                    "multiple INDEX hints matched table '{}'",
                    resolved_table_name
                )));
            }
        }

        if matched.is_some() {
            return Ok(matched);
        }

        for hint in &self.hints {
            if let SelectHint::Index { table, .. } = hint {
                let known_name = table_ref.name.eq_ignore_ascii_case(table)
                    || resolved_table_name.eq_ignore_ascii_case(table)
                    || table_ref
                        .alias
                        .as_deref()
                        .map(|alias| alias.eq_ignore_ascii_case(table))
                        .unwrap_or(false);
                if !known_name {
                    return Err(DbError::TableNotFound {
                        name: table.clone(),
                    });
                }
            }
        }

        Ok(None)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConstraintTiming {
    #[default]
    Immediate,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConstraintDeferrability {
    pub deferrable: bool,
    pub initially: ConstraintTiming,
}

// ── Column and constraint types ───────────────────────────────────────────────

/// Storage kind for a generated column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratedColumnKind {
    /// `GENERATED ALWAYS AS (expr) STORED` — computed on write and persisted.
    Stored,
    /// `GENERATED ALWAYS AS (expr) VIRTUAL` — computed on read.
    Virtual,
}

/// Operator used by an exclusion-constraint element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionOperator {
    /// `WITH =`
    Eq,
    /// `WITH <>` / `WITH !=`
    NotEq,
    /// `WITH <`
    Lt,
    /// `WITH <=`
    LtEq,
    /// `WITH >`
    Gt,
    /// `WITH >=`
    GtEq,
    /// `WITH &&`
    Overlaps,
}

/// Target expression of one exclusion-constraint element.
#[derive(Debug, Clone, PartialEq)]
pub enum ExclusionElementTarget {
    /// Plain column reference: `slug WITH =`
    Column(String),
    /// Parenthesized expression target: `((lower(email))) WITH =`
    Expr(Expr),
}

/// One `EXCLUDE USING ... (target WITH op)` element.
#[derive(Debug, Clone, PartialEq)]
pub struct ExclusionElement {
    pub target: ExclusionElementTarget,
    pub operator: ExclusionOperator,
}

/// Column definition as it appears in `CREATE TABLE` or `ALTER TABLE ADD COLUMN`.
///
/// Different from `axiomdb_catalog::schema::ColumnDef` (the disk-stored form
/// with `col_idx` and `ColumnType`). The executor converts between the two.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub constraints: Vec<ColumnConstraint>,
    /// Optional explicit column collation (`... COLLATE name`).
    /// `None` = inherit from table/database/session scope.
    pub collation: Option<String>,
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
        deferrability: ConstraintDeferrability,
    },
    /// `CHECK (expr)`
    Check(Expr),
    /// `ON UPDATE expr` — auto-refresh this column on every UPDATE when it
    /// was not explicitly assigned. Classic MySQL pattern:
    /// `updated_at TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`.
    OnUpdate(Expr),
    /// `GENERATED ALWAYS AS (expr) STORED|VIRTUAL`.
    Generated {
        expr: Expr,
        kind: GeneratedColumnKind,
    },
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
        deferrability: ConstraintDeferrability,
    },
    Check {
        name: Option<String>,
        expr: Expr,
    },
    /// `EXCLUDE USING btree (col WITH =, ...) [WHERE (...)]`
    Exclude {
        name: Option<String>,
        using: String,
        elements: Vec<ExclusionElement>,
        predicate: Option<Expr>,
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

/// A column listed in `CREATE INDEX`, with optional sort direction and expression.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexColumn {
    pub name: String,
    pub order: SortOrder,
    /// Optional expression for expression indexes: `CREATE INDEX ON t(LOWER(col))`
    pub expr: Option<Box<Expr>>,
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

/// The `FROM` source: a table, a subquery, or a table-valued function.
#[derive(Debug, Clone, PartialEq)]
pub enum FromClause {
    Table(TableRef),
    /// `(SELECT ...) AS alias` — boxed to break mutual recursion with SelectStmt.
    /// `lateral: true` for `LATERAL (SELECT ...)` (Phase 21.9).
    Subquery {
        query: Box<SelectStmt>,
        alias: String,
        lateral: bool,
    },
    /// Phase 21.25 — bounded `PIVOT` source. Lowered by the analyzer to
    /// `FromClause::Subquery` before execution.
    Pivot(Box<PivotClause>),
    /// `JSON_TABLE(doc, '$.path' COLUMNS (...)) [AS alias]` — SQL:2016
    /// table-valued function (Phase 11.20a, flat subset).
    JsonTable(Box<JsonTable>),
    /// Phase 11.25a — PostgreSQL-compatible JSONB set-returning functions:
    /// `jsonb_each`, `jsonb_each_text`, `jsonb_object_keys`,
    /// `jsonb_array_elements`, `jsonb_array_elements_text`.
    JsonbSrf(Box<JsonbSrf>),
    /// Phase 21.22 — `(VALUES (...)) AS alias(col1, col2, ...)` inline
    /// table constructor. Each row is evaluated at execution time
    /// against an empty scope (no outer correlation in this subphase).
    Values(Box<ValuesClause>),
    /// Phase 21.3 — materialized recursive CTE reference. Produced by
    /// the analyzer when a `FromClause::Table` matches a recursive CTE
    /// name; carries base+step SELECTs plus iteration semantics.
    RecursiveCte(Box<RecursiveCteClause>),
}

/// Phase 21.25 — bounded `FROM source PIVOT (...) [AS alias]`.
#[derive(Debug, Clone, PartialEq)]
pub struct PivotClause {
    pub source: Box<FromClause>,
    pub aggregate_name: String,
    pub aggregate_arg: Expr,
    pub pivot_expr: Expr,
    pub values: Vec<Expr>,
    pub alias: Option<String>,
}

/// Phase 21.2 — one binding in a `WITH` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CteBinding {
    pub name: String,
    /// Optional explicit column-name override: `WITH t(a, b) AS (...)`.
    pub column_names: Option<Vec<String>>,
    pub query: Box<SelectStmt>,
    /// Phase 21.3 — `true` when declared inside `WITH RECURSIVE`.
    /// Body must be `SELECT base UNION [ALL] SELECT step`.
    pub recursive: bool,
    /// Phase 21.3b — recursive step SELECT, set by the parser when a
    /// recursive CTE body has `UNION [ALL] SELECT ...` after the base.
    pub recursive_step: Option<Box<SelectStmt>>,
    /// Phase 21.3b — `true` when the base/step combiner was `UNION ALL`.
    pub recursive_union_all: bool,
}

/// Phase 21.3 — materialized recursive CTE reference in FROM. The
/// analyzer rewrites `FromClause::Table(cte_name)` into this variant
/// when the CTE is flagged recursive. The executor runs the PG-parity
/// iteration loop (base → working table → step → dedup → repeat).
#[derive(Debug, Clone, PartialEq)]
pub struct RecursiveCteClause {
    pub alias: String,
    pub column_names: Option<Vec<String>>,
    pub base: Box<SelectStmt>,
    pub step: Box<SelectStmt>,
    /// `true` = UNION ALL (keep dups). `false` = UNION (dedup).
    pub union_all: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValuesClause {
    /// Each inner `Vec<Expr>` is one row; all rows must have the same
    /// column count.
    pub rows: Vec<Vec<Expr>>,
    /// Required alias — both PG and MySQL reject inline VALUES without
    /// an alias.
    pub alias: String,
    /// Optional explicit column name list — `VALUES (...) AS t(c1, c2)`.
    /// When None, defaults to `column1`, `column2`, ... (PG parity).
    pub column_names: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonbSrfKind {
    Each,
    EachText,
    ObjectKeys,
    ArrayElements,
    ArrayElementsText,
}

impl JsonbSrfKind {
    pub fn from_fn_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "jsonb_each" => Some(Self::Each),
            "jsonb_each_text" => Some(Self::EachText),
            "jsonb_object_keys" => Some(Self::ObjectKeys),
            "jsonb_array_elements" => Some(Self::ArrayElements),
            "jsonb_array_elements_text" => Some(Self::ArrayElementsText),
            _ => None,
        }
    }

    pub fn fn_name(self) -> &'static str {
        match self {
            Self::Each => "jsonb_each",
            Self::EachText => "jsonb_each_text",
            Self::ObjectKeys => "jsonb_object_keys",
            Self::ArrayElements => "jsonb_array_elements",
            Self::ArrayElementsText => "jsonb_array_elements_text",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JsonbSrf {
    pub kind: JsonbSrfKind,
    pub doc: Expr,
    pub alias: Option<String>,
}

/// `JSON_TABLE(doc, row_path [PASSING ...] COLUMNS (...))` — SQL:2016
/// table-valued function.
///
/// Phase 11.20a introduced the flat form; 11.20b/c extended to NESTED PATH;
/// 11.20d1 added PASSING bindings (visible in every path evaluation) plus
/// per-column WRAPPER / QUOTES clauses (on `JsonTableColumn::Regular`).
#[derive(Debug, Clone, PartialEq)]
pub struct JsonTable {
    /// Document expression — must evaluate to JSONB/JSON/TEXT at exec time.
    pub doc: Expr,
    /// Row path as a SQL string literal, e.g. `'$[*]'`.
    pub row_path: String,
    /// `PASSING expr AS name` bindings; empty when the clause is absent.
    /// Bindings are visible to the row path AND every column / NESTED path.
    pub passing: Vec<(Expr, String)>,
    /// Column declarations in the `COLUMNS(...)` list.
    pub columns: Vec<JsonTableColumn>,
    /// Alias after `AS` (defaults to a synthetic name when omitted).
    pub alias: Option<String>,
}

/// One column inside a JSON_TABLE's `COLUMNS(...)` list.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonTableColumn {
    /// `name TYPE PATH 'jsonpath' [WRAPPER] [QUOTES] [ON EMPTY] [ON ERROR]`.
    Regular {
        name: String,
        ty: DataType,
        path: String,
        /// Phase 11.20d1: default `SqlJsonWrapper::Without`.
        wrapper: crate::expr::SqlJsonWrapper,
        /// Phase 11.20d1: default `SqlJsonQuotes::Keep`.
        quotes: crate::expr::SqlJsonQuotes,
        /// Default behavior: `SqlJsonOnBehavior::Null` (SQL:2016 spec default).
        on_empty: crate::expr::SqlJsonOnBehavior,
        on_error: crate::expr::SqlJsonOnBehavior,
    },
    /// `name FOR ORDINALITY` — 1-based BIGINT counter.
    Ordinality { name: String },
    /// `name TYPE EXISTS PATH 'jsonpath' [<bool|UNKNOWN|ERROR> ON ERROR]`.
    Exists {
        name: String,
        ty: DataType,
        path: String,
        /// Default behavior: `SqlJsonOnBehavior::FalseLit` (PG parity).
        on_error: crate::expr::SqlJsonOnBehavior,
    },
    /// `NESTED [PATH] 'jsonpath' COLUMNS ( ... )` — Phase 11.20b.
    ///
    /// The nested list shares the same `JsonTableColumn` shape, allowing
    /// the grammar to recurse arbitrarily; runtime depth is limited to 1
    /// in 11.20b (depth ≥ 2 and multi-sibling NESTED → 11.20c).
    Nested {
        path: String,
        columns: Vec<JsonTableColumn>,
    },
}

/// A `JOIN` attached to a `SELECT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    pub join_type: JoinType,
    pub table: FromClause,
    pub condition: JoinCondition,
    /// Phase 21.18 — `true` when the surface form was `NATURAL JOIN`. The
    /// analyzer computes the shared-column list and rewrites `condition`
    /// to `Using(...)`, clearing this flag; downstream code only ever
    /// sees the resolved form.
    pub natural: bool,
}

/// An item in the `ORDER BY` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub order: SortOrder,
    pub nulls: Option<NullsOrder>,
}

/// Bounded SQL window specification for ranking functions.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderByItem>,
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

/// GROUP BY clause representation.
///
/// Replaces the former `group_by: Vec<Expr>` + `with_rollup: bool` fields on `SelectStmt`.
/// Introduced in Phase 21.21 (GROUPING SETS / ROLLUP / CUBE).
#[derive(Debug, Clone, PartialEq)]
pub enum GroupByClause {
    /// No GROUP BY.
    None,
    /// Plain `GROUP BY expr, ...`
    Simple(Vec<Expr>),
    /// MySQL `GROUP BY expr, ... WITH ROLLUP` (GAP-C.5).
    /// Produces one subtotal row per grouping level with NULL in the rolled-up keys,
    /// plus a grand-total row with NULL in every group-by column.
    WithRollup(Vec<Expr>),
    /// SQL:1999 standard `GROUP BY ROLLUP(...)` / `CUBE(...)` / `GROUPING SETS(...)`.
    ///
    /// `universe`: deduplicated list of all expressions referenced across all sets,
    ///             in first-appearance order.
    /// `sets`:     each inner `Vec<usize>` is one grouping set, given as indices into
    ///             `universe`. An empty inner vec represents the grand-total set `()`.
    Sets {
        universe: Vec<Expr>,
        sets: Vec<Vec<usize>>,
    },
}

impl GroupByClause {
    /// Returns the flat list of group-by expressions.
    /// - `None`  → empty slice
    /// - `Simple` / `WithRollup` → the expression list
    /// - `Sets`  → the universe list
    pub fn exprs(&self) -> &[Expr] {
        match self {
            Self::None => &[],
            Self::Simple(v) | Self::WithRollup(v) => v,
            Self::Sets { universe, .. } => universe,
        }
    }

    /// Mutable access to the expression list for in-place resolution.
    /// Panics on `None` — callers must guard with `is_empty()` first.
    pub fn exprs_mut(&mut self) -> &mut Vec<Expr> {
        match self {
            Self::None => panic!("exprs_mut called on GroupByClause::None"),
            Self::Simple(v) | Self::WithRollup(v) => v,
            Self::Sets { universe, .. } => universe,
        }
    }

    /// True when there are no group-by expressions (includes `None` and empty vecs).
    pub fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Simple(v) | Self::WithRollup(v) => v.is_empty(),
            Self::Sets { universe, .. } => universe.is_empty(),
        }
    }

    /// True for the `WithRollup` variant.
    pub fn is_with_rollup(&self) -> bool {
        matches!(self, Self::WithRollup(_))
    }

    /// True for the `Sets` variant (SQL standard ROLLUP/CUBE/GROUPING SETS).
    pub fn is_sets(&self) -> bool {
        matches!(self, Self::Sets { .. })
    }

    /// Returns the sets list for the `Sets` variant; empty slice otherwise.
    pub fn grouping_sets(&self) -> &[Vec<usize>] {
        match self {
            Self::Sets { sets, .. } => sets,
            _ => &[],
        }
    }
}

/// Phase 21.11 — bounded optimizer hints attached to one SELECT statement.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectHint {
    Index { table: String, index: String },
    HashJoin,
    Parallel { workers: usize },
}

/// `from` is `None` for `SELECT` without `FROM` (e.g. `SELECT 1`, `SELECT NOW()`).
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    /// Phase 21.2 — `WITH name AS (SELECT ...), ...` non-recursive CTEs
    /// bound for the scope of this SELECT. Empty = no WITH clause.
    /// The analyzer substitutes references into `FromClause::Subquery`
    /// and clears the list, so executors never see non-empty entries.
    pub with_ctes: Vec<CteBinding>,
    pub distinct: bool,
    /// Phase 21.12 — `SELECT DISTINCT ON (e1, e2, …) …` key expressions.
    /// Non-empty implies `distinct == false`. Evaluated against pre-projection rows
    /// (same scope as ORDER BY), so source columns not in the SELECT list are accessible.
    pub distinct_on: Vec<Expr>,
    /// Phase 21.11 — optimizer comment hints from `SELECT /*+ ... */`.
    pub hints: Vec<SelectHint>,
    /// `SQL_CALC_FOUND_ROWS` modifier: stash pre-LIMIT count for `FOUND_ROWS()` (4.5e).
    pub calc_found_rows: bool,
    pub columns: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub joins: Vec<JoinClause>,
    pub where_clause: Option<Expr>,
    /// GROUP BY clause — see [`GroupByClause`] for all variants.
    pub group_by: GroupByClause,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    /// Row-level lock mode: `FOR UPDATE` or `LOCK IN SHARE MODE` (ignored until Phase 13.7).
    pub lock_mode: Option<LockMode>,
    /// Phase 21.9b — UNION/INTERSECT/EXCEPT tails when this SelectStmt is the
    /// first branch of a set operation embedded inside a subquery or FROM clause.
    /// Empty for plain SELECT statements (the common case).
    /// Top-level set operations still use `Stmt::SetOp`; this field handles
    /// `(SELECT ... UNION ALL SELECT ...) AS alias` inside FROM/JOIN.
    pub set_op_rest: Vec<SetOpTail>,
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

/// PostgreSQL `INSERT ... ON CONFLICT` action.
#[derive(Debug, Clone, PartialEq)]
pub enum OnConflictAction {
    /// `DO NOTHING`
    DoNothing,
    /// `DO UPDATE SET ... [WHERE ...]`
    DoUpdate {
        assignments: Vec<Assignment>,
        where_clause: Option<Expr>,
    },
}

/// PostgreSQL `INSERT ... ON CONFLICT` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflictClause {
    /// Column-list conflict target. Empty only for target-less `DO NOTHING`.
    pub target_columns: Vec<String>,
    pub action: OnConflictAction,
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
    /// Phase 21.4 — `RETURNING ...` projection. Empty = no RETURNING.
    #[allow(dead_code)]
    pub returning: Vec<SelectItem>,
    /// `INSERT ... ON DUPLICATE KEY UPDATE col = expr, ...` (MySQL ODKU).
    /// When the incoming row collides with a PK/UNIQUE index, the
    /// conflicting row is updated with the given assignments instead
    /// of erroring. `VALUES(col)` in the RHS refers to the proposed
    /// (would-have-been-inserted) row via `Expr::InsertValue`.
    pub on_duplicate_update: Option<Vec<Assignment>>,
    /// PostgreSQL `INSERT ... ON CONFLICT ...`.
    pub on_conflict: Option<OnConflictClause>,
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
    /// Phase 21.4 — `RETURNING ...` projection. Empty = no RETURNING.
    #[allow(dead_code)]
    pub returning: Vec<SelectItem>,
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
    /// Phase 21.4 — `RETURNING ...` projection. Empty = no RETURNING.
    #[allow(dead_code)]
    pub returning: Vec<SelectItem>,
}

/// SQL-standard MERGE match class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeActionCondition {
    /// `WHEN MATCHED`
    Matched,
    /// `WHEN NOT MATCHED [BY TARGET]`
    NotMatched,
}

/// SQL-standard MERGE action.
#[derive(Debug, Clone, PartialEq)]
pub enum MergeActionKind {
    /// `UPDATE SET col = expr, ...`
    Update(Vec<Assignment>),
    /// `DELETE`
    Delete,
    /// `INSERT [(col, ...)] VALUES (expr, ...)`
    Insert {
        columns: Option<Vec<String>>,
        values: Vec<Expr>,
    },
    /// `DO NOTHING`
    DoNothing,
}

/// One `WHEN ... THEN ...` branch in a MERGE statement.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeAction {
    pub condition: MergeActionCondition,
    /// Optional `AND <expr>` guard after MATCHED / NOT MATCHED.
    pub guard: Option<Expr>,
    pub kind: MergeActionKind,
}

/// SQL-standard `MERGE INTO target USING source ON expr WHEN ... THEN ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeStmt {
    pub target: TableRef,
    pub source: FromClause,
    pub on: Expr,
    pub actions: Vec<MergeAction>,
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
    pub persistence: TablePersistence,
}

/// `CREATE TABLE new_table AS SELECT ...` — derive schema from query result.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableAsSelectStmt {
    pub new_table: TableRef,
    pub select: SelectStmt,
    pub persistence: TablePersistence,
}

/// `CREATE MATERIALIZED VIEW name AS SELECT ...` — store query + materialized rows.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateMaterializedViewStmt {
    pub if_not_exists: bool,
    pub view: TableRef,
    pub select: SelectStmt,
    pub query_sql: String,
}

// ── DDL statements ────────────────────────────────────────────────────────────

/// `CREATE TABLE`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStmt {
    pub if_not_exists: bool,
    pub table: TableRef,
    pub columns: Vec<ColumnDef>,
    pub table_constraints: Vec<TableConstraint>,
    /// Optional table default collation (`CREATE TABLE ... ) COLLATE name`).
    pub collation: Option<String>,
    /// `CREATE TABLE ... IMMUTABLE` (Phase 13.9) — rejects any UPDATE/DELETE.
    pub immutable: bool,
    pub persistence: TablePersistence,
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

/// `DROP MATERIALIZED VIEW`
#[derive(Debug, Clone, PartialEq)]
pub struct DropMaterializedViewStmt {
    pub if_exists: bool,
    pub views: Vec<TableRef>,
    pub cascade: bool,
}

/// `REFRESH MATERIALIZED VIEW`
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshMaterializedViewStmt {
    pub view: TableRef,
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
    /// Optional database default collation.
    pub collation: Option<String>,
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

/// `DECLARE name CURSOR FOR <query>`
#[derive(Debug, Clone, PartialEq)]
pub struct DeclareCursorStmt {
    pub name: String,
    pub query: Box<Stmt>,
}

/// Row-window selector for `FETCH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchCount {
    Next,
    Forward(u64),
    All,
}

/// `FETCH ... FROM name`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchCursorStmt {
    pub name: String,
    pub count: FetchCount,
}

/// `LISTEN channel`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenStmt {
    pub channel: String,
}

/// `UNLISTEN channel` or `UNLISTEN *`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlistenStmt {
    /// `None` means `UNLISTEN *`.
    pub channel: Option<String>,
}

/// `NOTIFY channel [, payload]`
#[derive(Debug, Clone, PartialEq)]
pub struct NotifyStmt {
    pub channel: String,
    pub payload: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
}

/// `CREATE TRIGGER name AFTER event ON table FOR EACH STATEMENT AS SELECT ...`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTriggerStmt {
    pub name: String,
    pub event: TriggerEvent,
    pub table: TableRef,
    pub body_sql: String,
}

/// `DROP TRIGGER name ON table`
#[derive(Debug, Clone, PartialEq)]
pub struct DropTriggerStmt {
    pub name: String,
    pub table: TableRef,
}

/// `SHOW CREATE TRIGGER name ON table`
#[derive(Debug, Clone, PartialEq)]
pub struct ShowCreateTriggerStmt {
    pub name: String,
    pub table: TableRef,
}

/// `CLOSE name` or `CLOSE ALL`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseCursorStmt {
    One(String),
    All,
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
    Merge(MergeStmt),
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
    /// `CREATE MATERIALIZED VIEW name AS SELECT ...`.
    CreateMaterializedView(CreateMaterializedViewStmt),
    CreateTrigger(CreateTriggerStmt),
    CreateDatabase(CreateDatabaseStmt),
    CreateSchema(CreateSchemaStmt),
    CreateIndex(CreateIndexStmt),
    DropTable(DropTableStmt),
    DropMaterializedView(DropMaterializedViewStmt),
    DropDatabase(DropDatabaseStmt),
    DropIndex(DropIndexStmt),
    DropTrigger(DropTriggerStmt),
    RefreshMaterializedView(RefreshMaterializedViewStmt),
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
    ShowCreateTrigger(ShowCreateTriggerStmt),
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
    /// `SHOW NOTIFICATIONS` — drains queued LISTEN / NOTIFY events for this session.
    ShowNotifications,
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
    Listen(ListenStmt),
    Unlisten(UnlistenStmt),
    Notify(NotifyStmt),
    DeclareCursor(DeclareCursorStmt),
    FetchCursor(FetchCursorStmt),
    CloseCursor(CloseCursorStmt),
    /// `SAVEPOINT name` — create a named savepoint within an explicit transaction.
    Savepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] name` — undo changes back to the named savepoint.
    RollbackToSavepoint(String),
    /// `RELEASE [SAVEPOINT] name` — destroy the named savepoint (changes persist).
    ReleaseSavepoint(String),
    // Maintenance
    /// `CHECKPOINT` — flush storage pages and record a durable checkpoint LSN.
    Checkpoint,
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
            with_ctes: Vec::new(),
            distinct: false,
            distinct_on: vec![],
            hints: vec![],
            calc_found_rows: false,
            columns: vec![SelectItem::Wildcard],
            from: Some(FromClause::Table(TableRef::simple("users"))),
            joins: vec![],
            where_clause: Some(Expr::binop(BinaryOp::Gt, col(0, "age"), Expr::int(18))),
            group_by: GroupByClause::None,
            having: None,
            order_by: vec![OrderByItem {
                expr: col(1, "name"),
                order: SortOrder::Asc,
                nulls: None,
            }],
            limit: Some(Expr::int(10)),
            offset: Some(Expr::int(0)),
            lock_mode: None,
            set_op_rest: vec![],
        });
        assert!(matches!(stmt, Stmt::Select(_)));
    }

    #[test]
    fn test_select_without_from() {
        // SELECT 1  — health-check query used by ORMs
        let stmt = Stmt::Select(SelectStmt {
            with_ctes: Vec::new(),
            distinct: false,
            distinct_on: vec![],
            hints: vec![],
            calc_found_rows: false,
            columns: vec![SelectItem::Expr {
                expr: Expr::int(1),
                alias: None,
            }],
            from: None,
            joins: vec![],
            where_clause: None,
            group_by: GroupByClause::None,
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            set_op_rest: vec![],
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
                    collation: None,
                    type_len: 0,
                    is_char: false,
                },
                ColumnDef {
                    name: "email".into(),
                    data_type: DataType::Text,
                    constraints: vec![ColumnConstraint::NotNull, ColumnConstraint::Unique],
                    collation: None,
                    type_len: 0,
                    is_char: false,
                },
                ColumnDef {
                    name: "age".into(),
                    data_type: DataType::Int,
                    constraints: vec![ColumnConstraint::Default(Expr::int(0))],
                    collation: None,
                    type_len: 0,
                    is_char: false,
                },
            ],
            table_constraints: vec![],
            collation: None,
            immutable: false,
            persistence: TablePersistence::Permanent,
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
                    collation: None,
                    type_len: 0,
                    is_char: false,
                },
                ColumnDef {
                    name: "user_id".into(),
                    data_type: DataType::BigInt,
                    constraints: vec![ColumnConstraint::NotNull],
                    collation: None,
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
                    deferrability: ConstraintDeferrability::default(),
                },
            ],
            collation: None,
            immutable: false,
            persistence: TablePersistence::Permanent,
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
            on_duplicate_update: None,
            on_conflict: None,
            returning: Vec::new(),
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
            returning: Vec::new(),
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
            returning: Vec::new(),
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
            natural: false,
        };
        assert_eq!(join.join_type, JoinType::Left);
    }

    #[test]
    fn test_subquery_from_clause() {
        let subquery = SelectStmt {
            with_ctes: Vec::new(),
            distinct: false,
            distinct_on: vec![],
            hints: vec![],
            calc_found_rows: false,
            columns: vec![SelectItem::Wildcard],
            from: Some(FromClause::Table(TableRef::simple("users"))),
            joins: vec![],
            where_clause: None,
            group_by: GroupByClause::None,
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            set_op_rest: vec![],
        };
        let from = FromClause::Subquery {
            query: Box::new(subquery),
            alias: "sub".into(),
            lateral: false,
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
                    collation: None,
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
                expr: None,
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
            collation: None,
            type_len: 0,
            is_char: false,
        };
        assert_eq!(col_def.name, "balance");
    }
}
