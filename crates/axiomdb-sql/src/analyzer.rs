//! Semantic analyzer — validates references and resolves column indices.
//!
//! ## What this module does
//!
//! The parser produces `Stmt` with `Expr::Column { col_idx: 0, name }` for
//! every column reference — the `col_idx` is always a placeholder. This module:
//!
//! 1. Validates every table and column name against the catalog.
//! 2. Resolves each `col_idx` to the correct position in the **combined row**
//!    produced by the FROM + JOIN clauses.
//! 3. Reports structured errors for unknown tables, unknown columns, and
//!    ambiguous unqualified column names.
//!
//! ## Combined-row layout for JOINs
//!
//! For `FROM users u JOIN orders o ON u.id = o.user_id`:
//! ```text
//! users:   [id=0, name=1, age=2, email=3]          col_offset=0
//! orders:  [id=0, user_id=1, total=2, status=3]    col_offset=4
//! Combined: [u.id=0, u.name=1, u.age=2, u.email=3,
//!            o.id=4, o.user_id=5, o.total=6, o.status=7]
//! ```
//! `col_idx = table.col_offset + column_position_within_table`

use axiomdb_catalog::{
    schema::{ColumnDef, DEFAULT_DATABASE_NAME},
    CatalogReader,
};
use axiomdb_core::{error::DbError, TransactionSnapshot};
use axiomdb_storage::StorageEngine;

use crate::{
    ast::{
        AlterTableStmt, Assignment, ColumnConstraint, CreateIndexStmt, CreateTableStmt, DeleteStmt,
        DropTableStmt, FromClause, InsertSource, InsertStmt, JoinCondition, SelectItem, SelectStmt,
        Stmt, TableRef, UpdateStmt,
    },
    expr::Expr,
    schema_cache::SchemaCache,
};

// ── Public entry point ────────────────────────────────────────────────────────

/// Semantic analysis: validate all references and resolve `col_idx`.
///
/// Takes a parsed `Stmt` (all `col_idx = 0`) and returns the same `Stmt`
/// with every `Expr::Column.col_idx` set to its correct position in the
/// combined row produced by the FROM clause.
///
/// # Errors
/// - [`DbError::TableNotFound`]    — unknown table or alias
/// - [`DbError::ColumnNotFound`]   — column not in any in-scope table
/// - [`DbError::AmbiguousColumn`]  — unqualified name in multiple tables
pub fn analyze(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
) -> Result<Stmt, DbError> {
    analyze_with_defaults(stmt, storage, snapshot, DEFAULT_DATABASE_NAME, "public")
}

/// Cached variant of [`analyze`] — skips catalog heap scans for tables already
/// seen in this session.
///
/// Pass the same `cache` across repeated calls to the same tables (e.g. bulk
/// INSERT loops). The caller is responsible for calling [`SchemaCache::invalidate`]
/// after any DDL statement that changes the schema.
///
/// First call per table: normal catalog scan + populates cache.
/// Subsequent calls: cache hit, zero catalog I/O.
pub fn analyze_cached(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    cache: &mut SchemaCache,
) -> Result<Stmt, DbError> {
    analyze_cached_with_defaults(
        stmt,
        storage,
        snapshot,
        DEFAULT_DATABASE_NAME,
        "public",
        cache,
    )
}

pub fn analyze_with_defaults(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<Stmt, DbError> {
    analyze_stmt(stmt, storage, snapshot, default_database, default_schema)
}

pub fn analyze_cached_with_defaults(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    cache: &mut SchemaCache,
) -> Result<Stmt, DbError> {
    analyze_stmt_cached(
        stmt,
        storage,
        snapshot,
        default_database,
        default_schema,
        cache,
    )
}

// ── BindContext ───────────────────────────────────────────────────────────────

/// Resolution context built from the FROM and JOIN clauses of a SELECT.
struct BindContext {
    tables: Vec<BoundTable>,
}

struct BoundTable {
    /// Alias from `FROM users AS u`, or `None` if no alias.
    alias: Option<String>,
    /// Real table name in the catalog.
    name: String,
    /// Columns in declaration order (from `CatalogReader::list_columns`).
    columns: Vec<ColumnDef>,
    /// Start position of this table's columns in the combined row.
    col_offset: usize,
}

impl BindContext {
    fn empty() -> Self {
        Self { tables: vec![] }
    }

    /// Find a table by its alias or name.
    fn find_table(&self, qualifier: &str) -> Option<&BoundTable> {
        self.tables
            .iter()
            .find(|t| t.alias.as_deref() == Some(qualifier) || t.name == qualifier)
    }

    /// Find all (table_idx, col_idx_global, col_name) for an unqualified column.
    fn find_column_all(&self, col_name: &str) -> Vec<(usize, usize, &str)> {
        let mut found = Vec::new();
        for (ti, table) in self.tables.iter().enumerate() {
            for (ci, col) in table.columns.iter().enumerate() {
                if col.name == col_name {
                    found.push((ti, table.col_offset + ci, col.name.as_str()));
                }
            }
        }
        found
    }

    /// Resolve a qualified `table.column` or unqualified `column` reference.
    fn resolve_column(&self, name: &str) -> Result<usize, DbError> {
        let (qualifier, field) = split_name(name);

        if let Some(q) = qualifier {
            // Qualified: find the specific table
            let table = self.find_table(q).ok_or_else(|| DbError::TableNotFound {
                name: format!("{q}.{field}"),
            })?;
            let (pos, _) = find_col_in_table(table, field, q)?;
            Ok(table.col_offset + pos)
        } else {
            // Unqualified: search all tables
            let matches = self.find_column_all(field);
            match matches.len() {
                0 => {
                    let available = self
                        .tables
                        .iter()
                        .flat_map(|t| t.columns.iter().map(|c| c.name.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let table_names = self
                        .tables
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Err(DbError::ColumnNotFound {
                        name: field.to_string(),
                        table: format!("\"{}\" (available: {})", table_names, available),
                    })
                }
                1 => Ok(matches[0].1),
                _ => {
                    let table_list = matches
                        .iter()
                        .map(|(ti, _, _)| self.tables[*ti].name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Err(DbError::AmbiguousColumn {
                        name: field.to_string(),
                        tables: table_list,
                    })
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Split "table.column" into ("table", "column") or (None, "column").
fn split_name(name: &str) -> (Option<&str>, &str) {
    match name.find('.') {
        Some(i) => (Some(&name[..i]), &name[i + 1..]),
        None => (None, name),
    }
}

/// Find a column by name within a single BoundTable.
fn find_col_in_table<'a>(
    table: &'a BoundTable,
    col_name: &str,
    context: &str,
) -> Result<(usize, &'a ColumnDef), DbError> {
    table
        .columns
        .iter()
        .enumerate()
        .find(|(_, c)| c.name == col_name)
        .ok_or_else(|| {
            let available = table
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            DbError::ColumnNotFound {
                name: col_name.to_string(),
                table: format!("\"{context}\" (available: {available})"),
            }
        })
}

/// Levenshtein distance — O(|a|*|b|), both strings ≤ 64 chars (Phase 4.3d).
/// Used for typo hints; kept for Phase 4.18b typo suggestion feature.
#[allow(dead_code)]
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    (0..=m).for_each(|i| dp[i][0] = i);
    (0..=n).for_each(|j| dp[0][j] = j);
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1]
            } else {
                1 + dp[i - 1][j].min(dp[i][j - 1]).min(dp[i - 1][j - 1])
            };
        }
    }
    dp[m][n]
}

// ── BindContext construction ──────────────────────────────────────────────────

fn build_context(
    from: &Option<FromClause>,
    joins: &[crate::ast::JoinClause],
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<BindContext, DbError> {
    let mut ctx = BindContext::empty();
    let mut col_offset = 0usize;

    if let Some(from_clause) = from {
        let bound = bound_from_clause(
            from_clause,
            storage,
            snapshot,
            default_database,
            default_schema,
            &mut col_offset,
        )?;
        ctx.tables.extend(bound);
    }

    for join in joins {
        let bound = bound_from_clause(
            &join.table,
            storage,
            snapshot,
            default_database,
            default_schema,
            &mut col_offset,
        )?;
        ctx.tables.extend(bound);
    }

    Ok(ctx)
}

fn bound_from_clause(
    from: &FromClause,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    col_offset: &mut usize,
) -> Result<Vec<BoundTable>, DbError> {
    match from {
        FromClause::Table(table_ref) => {
            let bound = bound_table_ref(
                table_ref,
                storage,
                snapshot,
                default_database,
                default_schema,
                col_offset,
            )?;
            Ok(vec![bound])
        }
        FromClause::Subquery { query, alias } => {
            // Analyze the inner SELECT recursively.
            // The virtual table's columns = the SELECT list items.
            let analyzed_query = analyze_select(
                *query.clone(),
                storage,
                snapshot,
                default_database,
                default_schema,
            )?;
            let virtual_cols = virtual_columns_from_select(&analyzed_query);
            let n = virtual_cols.len();
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias.clone(),
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
    }
}

fn bound_table_ref(
    table_ref: &TableRef,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    col_offset: &mut usize,
) -> Result<BoundTable, DbError> {
    let database = table_ref.database.as_deref().unwrap_or(default_database);
    let schema = table_ref.schema.as_deref().unwrap_or(default_schema);
    let mut reader = CatalogReader::new(storage, snapshot)?;

    // If the user explicitly specified a database, verify it exists first.
    if table_ref.database.is_some() && !reader.database_exists(database)? {
        return Err(DbError::DatabaseNotFound {
            name: database.to_string(),
        });
    }

    // When the user didn't specify an explicit schema, try the default schema
    // first, then fall back to "public" if different (search_path fallback).
    let table_def = if table_ref.schema.is_none() {
        let mut found = reader.get_table_in_database(database, schema, &table_ref.name)?;
        if found.is_none() && schema != "public" {
            found = reader.get_table_in_database(database, "public", &table_ref.name)?;
        }
        found
    } else {
        reader.get_table_in_database(database, schema, &table_ref.name)?
    }
    .ok_or_else(|| DbError::TableNotFound {
        name: table_ref.name.clone(),
    })?;

    let columns = reader.list_columns(table_def.id)?;
    let n = columns.len();
    let bound = BoundTable {
        alias: table_ref.alias.clone(),
        name: table_ref.name.clone(),
        columns,
        col_offset: *col_offset,
    };
    *col_offset += n;
    Ok(bound)
}

/// Resolves a table for DML statements, trying the default schema first and
/// falling back to "public" for unqualified names when the search path has
/// a non-public first entry.
fn resolve_dml_table(
    reader: &mut CatalogReader,
    table_ref: &TableRef,
    default_database: &str,
    default_schema: &str,
) -> Result<axiomdb_catalog::TableDef, DbError> {
    let database = table_ref.database.as_deref().unwrap_or(default_database);
    let schema = table_ref.schema.as_deref().unwrap_or(default_schema);

    if table_ref.schema.is_none() {
        // Unqualified: try search_path order (default_schema, then public fallback)
        if let Some(td) = reader.get_table_in_database(database, schema, &table_ref.name)? {
            return Ok(td);
        }
        if schema != "public" {
            if let Some(td) = reader.get_table_in_database(database, "public", &table_ref.name)? {
                return Ok(td);
            }
        }
        Err(DbError::TableNotFound {
            name: table_ref.name.clone(),
        })
    } else {
        reader
            .get_table_in_database(database, schema, &table_ref.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: table_ref.name.clone(),
            })
    }
}

/// Build virtual ColumnDef list from the SELECT list of an analyzed subquery.
fn virtual_columns_from_select(select: &SelectStmt) -> Vec<ColumnDef> {
    use axiomdb_catalog::schema::{ColumnType, TableId};
    select
        .columns
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let name = match item {
                SelectItem::Expr { alias: Some(a), .. } => a.clone(),
                SelectItem::Expr {
                    expr: Expr::Column { name, .. },
                    ..
                } => {
                    // Use the unqualified column name if no alias
                    split_name(name).1.to_string()
                }
                SelectItem::Expr { .. } => format!("col{i}"),
                SelectItem::Wildcard => format!("*{i}"),
                SelectItem::QualifiedWildcard(t) => format!("{t}.*{i}"),
            };
            ColumnDef {
                table_id: 0 as TableId,
                col_idx: i as u16,
                name,
                col_type: ColumnType::Text, // type unknown without full type inference
                nullable: true,
                auto_increment: false,
            }
        })
        .collect()
}

// ── Expression resolution ─────────────────────────────────────────────────────

/// Storage + snapshot context threaded through `resolve_expr_full` so that
/// subquery arms can call `analyze_select_with_outer`.
///
/// `None` is used on pure paths (UPDATE/DELETE/INSERT) where subqueries are not
/// expected; subquery arms return `DbError::NotImplemented` when `state` is `None`.
struct AnalyzeState<'a> {
    storage: &'a dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &'a str,
    default_schema: &'a str,
}

/// Thin wrapper — resolves `expr` in `ctx` with no outer scopes and no state.
///
/// Sufficient for all non-subquery expressions (UPDATE SET, DELETE WHERE, etc.).
fn resolve_expr(expr: Expr, ctx: &BindContext) -> Result<Expr, DbError> {
    resolve_expr_full(expr, ctx, &[], None)
}

/// Resolves column references in `expr` against `ctx` (inner scope) and
/// `outer_scopes` (enclosing query scopes, outermost last).
///
/// `state` must be `Some(...)` for any query that may contain subquery expressions
/// (`Expr::Subquery`, `Expr::InSubquery`, `Expr::Exists`). When `state` is `None`
/// those arms return `DbError::NotImplemented`.
///
/// When a column is found in an outer scope but not `ctx`, it is emitted as
/// `Expr::OuterColumn` — a correlated reference the executor substitutes with the
/// outer row before executing the inner query.
///
/// Only depth-1 correlation is supported (Phase 4.11). Deeper nesting returns
/// `DbError::NotImplemented`.
fn resolve_expr_full(
    expr: Expr,
    ctx: &BindContext,
    outer_scopes: &[&BindContext],
    state: Option<&AnalyzeState<'_>>,
) -> Result<Expr, DbError> {
    match expr {
        Expr::Literal(v) => Ok(Expr::Literal(v)),

        Expr::Column { col_idx: _, name } => {
            // 1. Try the inner (current) scope first.
            if !ctx.tables.is_empty() {
                if let Ok(idx) = ctx.resolve_column(&name) {
                    return Ok(Expr::Column { col_idx: idx, name });
                }
            }
            // 2. Try outer scopes — emit OuterColumn if found.
            for outer_ctx in outer_scopes {
                if let Ok(idx) = outer_ctx.resolve_column(&name) {
                    return Ok(Expr::OuterColumn { col_idx: idx, name });
                }
            }
            // 3. Not found anywhere.
            if ctx.tables.is_empty() && outer_scopes.is_empty() {
                return Err(DbError::ColumnNotFound {
                    name: name.clone(),
                    table: "no tables in scope (missing FROM clause)".into(),
                });
            }
            // Delegate to ctx.resolve_column for the best error message.
            ctx.resolve_column(&name).map(|_| unreachable!())?;
            unreachable!()
        }

        Expr::OuterColumn { .. } => Ok(expr), // already resolved — pass through

        // Prepared statement parameter: type is determined at execute time.
        // No column resolution needed — pass through unchanged.
        Expr::Param { .. } => Ok(expr),

        Expr::UnaryOp { op, operand } => Ok(Expr::UnaryOp {
            op,
            operand: Box::new(resolve_expr_full(*operand, ctx, outer_scopes, state)?),
        }),

        Expr::BinaryOp { op, left, right } => Ok(Expr::BinaryOp {
            op,
            left: Box::new(resolve_expr_full(*left, ctx, outer_scopes, state)?),
            right: Box::new(resolve_expr_full(*right, ctx, outer_scopes, state)?),
        }),

        Expr::IsNull { expr, negated } => Ok(Expr::IsNull {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            negated,
        }),

        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Ok(Expr::Between {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            low: Box::new(resolve_expr_full(*low, ctx, outer_scopes, state)?),
            high: Box::new(resolve_expr_full(*high, ctx, outer_scopes, state)?),
            negated,
        }),

        Expr::Like {
            expr,
            pattern,
            negated,
        } => Ok(Expr::Like {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            pattern: Box::new(resolve_expr_full(*pattern, ctx, outer_scopes, state)?),
            negated,
        }),

        Expr::In {
            expr,
            list,
            negated,
        } => {
            let expr = Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?);
            let list = list
                .into_iter()
                .map(|e| resolve_expr_full(e, ctx, outer_scopes, state))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::In {
                expr,
                list,
                negated,
            })
        }

        Expr::Function { name, args } => {
            let args = args
                .into_iter()
                .map(|a| resolve_expr_full(a, ctx, outer_scopes, state))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Function { name, args })
        }

        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            let operand = operand
                .map(|e| resolve_expr_full(*e, ctx, outer_scopes, state).map(Box::new))
                .transpose()?;
            let when_thens = when_thens
                .into_iter()
                .map(|(w, t)| {
                    Ok((
                        resolve_expr_full(w, ctx, outer_scopes, state)?,
                        resolve_expr_full(t, ctx, outer_scopes, state)?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let else_result = else_result
                .map(|e| resolve_expr_full(*e, ctx, outer_scopes, state).map(Box::new))
                .transpose()?;
            Ok(Expr::Case {
                operand,
                when_thens,
                else_result,
            })
        }

        Expr::Cast { expr, target } => Ok(Expr::Cast {
            expr: Box::new(resolve_expr_full(*expr, ctx, outer_scopes, state)?),
            target,
        }),

        Expr::GroupConcat {
            expr,
            distinct,
            order_by,
            separator,
        } => {
            let expr = resolve_expr_full(*expr, ctx, outer_scopes, state)?;
            let order_by = order_by
                .into_iter()
                .map(|(e, dir)| Ok((resolve_expr_full(e, ctx, outer_scopes, state)?, dir)))
                .collect::<Result<Vec<_>, DbError>>()?;
            Ok(Expr::GroupConcat {
                expr: Box::new(expr),
                distinct,
                order_by,
                separator,
            })
        }

        // ── Subquery variants ────────────────────────────────────────────────
        //
        // The inner SELECT is analyzed with `ctx` pushed as an outer scope so
        // that column references to the enclosing query become `OuterColumn`.
        // Depth-1 correlated subqueries are fully supported.
        // Depth > 1 correlated subqueries fail at executor time with a clear
        // "unsubstituted OuterColumn" error (Phase 6 will fix this).
        // Uncorrelated subqueries at any depth work correctly.
        Expr::Subquery(inner) => {
            let st = state.ok_or_else(|| DbError::NotImplemented {
                feature: "subqueries require an analyze context (not available here)".into(),
            })?;
            let mut extended: Vec<&BindContext> = outer_scopes.to_vec();
            extended.push(ctx);
            let analyzed = analyze_select_with_outer(
                *inner,
                st.storage,
                st.snapshot,
                st.default_database,
                st.default_schema,
                &extended,
            )?;
            Ok(Expr::Subquery(Box::new(analyzed)))
        }

        Expr::InSubquery {
            expr,
            query,
            negated,
        } => {
            let st = state.ok_or_else(|| DbError::NotImplemented {
                feature: "subqueries require an analyze context (not available here)".into(),
            })?;
            let expr = Box::new(resolve_expr_full(*expr, ctx, outer_scopes, Some(st))?);
            let mut extended: Vec<&BindContext> = outer_scopes.to_vec();
            extended.push(ctx);
            let analyzed = analyze_select_with_outer(
                *query,
                st.storage,
                st.snapshot,
                st.default_database,
                st.default_schema,
                &extended,
            )?;
            Ok(Expr::InSubquery {
                expr,
                query: Box::new(analyzed),
                negated,
            })
        }

        Expr::Exists { query, negated } => {
            let st = state.ok_or_else(|| DbError::NotImplemented {
                feature: "subqueries require an analyze context (not available here)".into(),
            })?;
            let mut extended: Vec<&BindContext> = outer_scopes.to_vec();
            extended.push(ctx);
            let analyzed = analyze_select_with_outer(
                *query,
                st.storage,
                st.snapshot,
                st.default_database,
                st.default_schema,
                &extended,
            )?;
            Ok(Expr::Exists {
                query: Box::new(analyzed),
                negated,
            })
        }
    }
}

/// Convenience wrapper for `Option<Expr>` with no outer scopes and no state.
fn resolve_opt_expr(expr: Option<Expr>, ctx: &BindContext) -> Result<Option<Expr>, DbError> {
    expr.map(|e| resolve_expr(e, ctx)).transpose()
}

/// Convenience wrapper for `Option<Expr>` with full state threading.
fn resolve_opt_expr_full(
    expr: Option<Expr>,
    ctx: &BindContext,
    outer_scopes: &[&BindContext],
    state: Option<&AnalyzeState<'_>>,
) -> Result<Option<Expr>, DbError> {
    expr.map(|e| resolve_expr_full(e, ctx, outer_scopes, state))
        .transpose()
}

// ── Statement analysis ────────────────────────────────────────────────────────

fn analyze_stmt(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<Stmt, DbError> {
    match stmt {
        Stmt::Select(s) => {
            analyze_select(s, storage, snapshot, default_database, default_schema).map(Stmt::Select)
        }
        Stmt::Insert(s) => {
            analyze_insert(s, storage, snapshot, default_database, default_schema).map(Stmt::Insert)
        }
        Stmt::Update(s) => {
            analyze_update(s, storage, snapshot, default_database, default_schema).map(Stmt::Update)
        }
        Stmt::Delete(s) => {
            analyze_delete(s, storage, snapshot, default_database, default_schema).map(Stmt::Delete)
        }
        Stmt::CreateTable(s) => {
            analyze_create_table(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::CreateTable)
        }
        Stmt::DropTable(s) => {
            analyze_drop_table(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::DropTable)
        }
        Stmt::CreateIndex(s) => {
            analyze_create_index(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::CreateIndex)
        }
        Stmt::AlterTable(s) => {
            analyze_alter_table(s, storage, snapshot, default_database, default_schema)
                .map(Stmt::AlterTable)
        }
        // Statements that need no semantic analysis for Phase 4.18:
        other => Ok(other),
    }
}

// ── Cached analysis dispatcher ────────────────────────────────────────────────

fn analyze_stmt_cached(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    cache: &mut SchemaCache,
) -> Result<Stmt, DbError> {
    match stmt {
        // INSERT is the hot path — use the cached variant
        Stmt::Insert(s) => analyze_insert_cached(
            s,
            storage,
            snapshot,
            default_database,
            default_schema,
            cache,
        )
        .map(Stmt::Insert),
        // DDL invalidates the cache
        Stmt::CreateTable(_) | Stmt::DropTable(_) | Stmt::AlterTable(_) => {
            cache.invalidate();
            analyze_stmt(stmt, storage, snapshot, default_database, default_schema)
        }
        // Everything else: fall back to uncached
        other => analyze_stmt(other, storage, snapshot, default_database, default_schema),
    }
}

fn analyze_insert_cached(
    mut s: InsertStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    cache: &mut SchemaCache,
) -> Result<InsertStmt, DbError> {
    let database = s.table.database.as_deref().unwrap_or(default_database);
    let schema = s.table.schema.as_deref().unwrap_or(default_schema);

    // Try cache first — avoids HeapChain::scan_visible × 2 on repeated inserts
    let (table_def, columns): (axiomdb_catalog::TableDef, Vec<axiomdb_catalog::ColumnDef>) =
        if let Some(td) = cache.get_table(database, schema, &s.table.name) {
            let cols = cache.get_columns(td.id).cloned().unwrap_or_default();
            (td.clone(), cols)
        } else {
            // Cache miss: resolve with search_path fallback
            let mut reader = CatalogReader::new(storage, snapshot)?;
            let td = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
            let cols = reader.list_columns(td.id)?;
            cache.insert(database, schema, &s.table.name, td.clone(), cols.clone());
            (td, cols)
        };
    let _ = table_def; // used only to populate cache; executor reads from catalog directly

    // Validate named column list if provided
    if let Some(ref col_names) = s.columns {
        for col_name in col_names {
            if !columns.iter().any(|c| &c.name == col_name) {
                let available = columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(DbError::ColumnNotFound {
                    name: col_name.clone(),
                    table: format!("\"{}\" (available: {})", s.table.name, available),
                });
            }
        }
    }

    // Analyze SELECT source if present
    if let InsertSource::Select(ref select) = s.source {
        let analyzed = analyze_select(
            *select.clone(),
            storage,
            snapshot,
            default_database,
            default_schema,
        )?;
        s.source = InsertSource::Select(Box::new(analyzed));
    }

    Ok(s)
}

// ── SELECT ────────────────────────────────────────────────────────────────────

/// Public entry for analyzing a SELECT with no outer scopes.
///
/// Delegates to [`analyze_select_with_outer`] with an empty outer-scope slice.
fn analyze_select(
    s: SelectStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<SelectStmt, DbError> {
    analyze_select_with_outer(s, storage, snapshot, default_database, default_schema, &[])
}

/// Analyze a SELECT statement, threading `outer_scopes` through every expression
/// so that correlated column references produce `Expr::OuterColumn` nodes.
///
/// Called recursively for subqueries: when a subquery is encountered inside
/// `resolve_expr_full`, the current `BindContext` is appended to `outer_scopes`
/// and this function is invoked on the inner `SelectStmt`.
fn analyze_select_with_outer(
    mut s: SelectStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    outer_scopes: &[&BindContext],
) -> Result<SelectStmt, DbError> {
    // Build resolution context from FROM and JOINs.
    let ctx = build_context(
        &s.from,
        &s.joins,
        storage,
        snapshot,
        default_database,
        default_schema,
    )?;

    // If FROM is a derived table (subquery in FROM), `build_context` analyzed
    // the inner query to extract virtual column names, but did NOT store the
    // analyzed version back into `s.from`. Fix that here so the executor
    // receives the analyzed inner query with correct `col_idx` values.
    if let Some(FromClause::Subquery { query, alias }) = s.from {
        let analyzed_inner = analyze_select_with_outer(
            *query,
            storage,
            snapshot,
            default_database,
            default_schema,
            outer_scopes,
        )?;
        s.from = Some(FromClause::Subquery {
            query: Box::new(analyzed_inner),
            alias,
        });
    }

    // AnalyzeState is needed so that subquery arms inside expressions can
    // recurse back into analyze_select_with_outer.
    let state = AnalyzeState {
        storage,
        snapshot,
        default_database,
        default_schema,
    };

    // Resolve JOIN conditions.
    let mut resolved_joins = Vec::with_capacity(s.joins.len());
    for mut join in s.joins {
        join.condition = match join.condition {
            JoinCondition::On(expr) => {
                JoinCondition::On(resolve_expr_full(expr, &ctx, outer_scopes, Some(&state))?)
            }
            JoinCondition::Using(cols) => {
                // Detailed column-by-column validation deferred (Phase 4.22).
                JoinCondition::Using(cols)
            }
        };
        resolved_joins.push(join);
    }
    s.joins = resolved_joins;

    // Resolve WHERE, GROUP BY, HAVING, ORDER BY, LIMIT, OFFSET.
    s.where_clause = resolve_opt_expr_full(s.where_clause, &ctx, outer_scopes, Some(&state))?;
    s.group_by = s
        .group_by
        .into_iter()
        .map(|e| resolve_expr_full(e, &ctx, outer_scopes, Some(&state)))
        .collect::<Result<_, _>>()?;
    s.having = resolve_opt_expr_full(s.having, &ctx, outer_scopes, Some(&state))?;

    // Resolve ORDER BY.
    let mut resolved_order = Vec::with_capacity(s.order_by.len());
    for mut item in s.order_by {
        item.expr = resolve_expr_full(item.expr, &ctx, outer_scopes, Some(&state))?;
        resolved_order.push(item);
    }
    s.order_by = resolved_order;

    s.limit = resolve_opt_expr_full(s.limit, &ctx, outer_scopes, Some(&state))?;
    s.offset = resolve_opt_expr_full(s.offset, &ctx, outer_scopes, Some(&state))?;

    // Resolve SELECT list.
    let mut resolved_cols = Vec::with_capacity(s.columns.len());
    for item in s.columns {
        let resolved = match item {
            SelectItem::Wildcard => SelectItem::Wildcard,
            SelectItem::QualifiedWildcard(ref table_name) => {
                // Validate the table/alias is in scope.
                if !ctx.tables.is_empty() && ctx.find_table(table_name).is_none() {
                    return Err(DbError::TableNotFound {
                        name: format!("{table_name}.*"),
                    });
                }
                item
            }
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: resolve_expr_full(expr, &ctx, outer_scopes, Some(&state))?,
                alias,
            },
        };
        resolved_cols.push(resolved);
    }
    s.columns = resolved_cols;

    Ok(s)
}

// ── INSERT ────────────────────────────────────────────────────────────────────

fn analyze_insert(
    mut s: InsertStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<InsertStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot)?;
    let table_def = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
    let columns = reader.list_columns(table_def.id)?;

    // Validate named column list if provided.
    if let Some(ref col_names) = s.columns {
        for col_name in col_names {
            if !columns.iter().any(|c| &c.name == col_name) {
                let available = columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(DbError::ColumnNotFound {
                    name: col_name.clone(),
                    table: format!("\"{}\" (available: {})", s.table.name, available),
                });
            }
        }
    }

    // Analyze SELECT source if present.
    if let InsertSource::Select(ref select) = s.source {
        let analyzed = analyze_select(
            *select.clone(),
            storage,
            snapshot,
            default_database,
            default_schema,
        )?;
        s.source = InsertSource::Select(Box::new(analyzed));
    }

    Ok(s)
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn analyze_update(
    mut s: UpdateStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<UpdateStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot)?;
    let table_def = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
    let columns = reader.list_columns(table_def.id)?;

    // Build single-table context.
    let bound = BoundTable {
        alias: s.table.alias.clone(),
        name: s.table.name.clone(),
        columns: columns.clone(),
        col_offset: 0,
    };
    let ctx = BindContext {
        tables: vec![bound],
    };

    // Validate and resolve SET assignments.
    let mut resolved = Vec::with_capacity(s.assignments.len());
    for Assignment { column, value } in s.assignments {
        if !columns.iter().any(|c| c.name == column) {
            let available = columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DbError::ColumnNotFound {
                name: column.clone(),
                table: format!("\"{}\" (available: {})", s.table.name, available),
            });
        }
        let value = resolve_expr(value, &ctx)?;
        resolved.push(Assignment { column, value });
    }
    s.assignments = resolved;

    s.where_clause = resolve_opt_expr(s.where_clause, &ctx)?;
    Ok(s)
}

// ── DELETE ────────────────────────────────────────────────────────────────────

fn analyze_delete(
    mut s: DeleteStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<DeleteStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot)?;
    let table_def = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
    let columns = reader.list_columns(table_def.id)?;
    let bound = BoundTable {
        alias: s.table.alias.clone(),
        name: s.table.name.clone(),
        columns,
        col_offset: 0,
    };
    let ctx = BindContext {
        tables: vec![bound],
    };

    s.where_clause = resolve_opt_expr(s.where_clause, &ctx)?;
    Ok(s)
}

// ── CREATE TABLE ──────────────────────────────────────────────────────────────

fn analyze_create_table(
    s: CreateTableStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<CreateTableStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot)?;

    // Validate FK REFERENCES targets.
    for col_def in &s.columns {
        for constraint in &col_def.constraints {
            if let ColumnConstraint::References {
                table: ref_table,
                column: ref_col,
                ..
            } = constraint
            {
                let schema = default_schema;
                let exists = reader
                    .get_table_in_database(default_database, schema, ref_table)?
                    .is_some()
                    || reader
                        .get_table_in_database(default_database, "public", ref_table)?
                        .is_some();
                if !exists {
                    return Err(DbError::TableNotFound {
                        name: ref_table.clone(),
                    });
                }
                // If a specific column is referenced, validate it exists.
                if let Some(col_name) = ref_col {
                    let ref_table_def = reader
                        .get_table_in_database(default_database, default_schema, ref_table)?
                        .or_else(|| {
                            reader
                                .get_table_in_database(default_database, "public", ref_table)
                                .ok()
                                .flatten()
                        });
                    if let Some(ref_def) = ref_table_def {
                        let ref_cols = reader.list_columns(ref_def.id)?;
                        if !ref_cols.iter().any(|c| &c.name == col_name) {
                            return Err(DbError::ColumnNotFound {
                                name: col_name.clone(),
                                table: ref_table.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(s)
}

// ── DROP TABLE ────────────────────────────────────────────────────────────────

fn analyze_drop_table(
    s: DropTableStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<DropTableStmt, DbError> {
    if s.if_exists {
        return Ok(s); // IF EXISTS: no validation needed
    }

    let mut reader = CatalogReader::new(storage, snapshot)?;
    for table_ref in &s.tables {
        let database = table_ref.database.as_deref().unwrap_or(default_database);
        let schema = table_ref.schema.as_deref().unwrap_or(default_schema);
        if table_ref.database.is_some() && !reader.database_exists(database)? {
            return Err(DbError::DatabaseNotFound {
                name: database.to_string(),
            });
        }
        let exists = reader
            .get_table_in_database(database, schema, &table_ref.name)?
            .is_some();
        if !exists {
            return Err(DbError::TableNotFound {
                name: table_ref.name.clone(),
            });
        }
    }

    Ok(s)
}

// ── CREATE INDEX ──────────────────────────────────────────────────────────────

fn analyze_create_index(
    s: CreateIndexStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<CreateIndexStmt, DbError> {
    let schema = s.table.schema.as_deref().unwrap_or(default_schema);
    let mut reader = CatalogReader::new(storage, snapshot)?;

    let table_def = reader
        .get_table_in_database(default_database, schema, &s.table.name)?
        .ok_or_else(|| DbError::TableNotFound {
            name: s.table.name.clone(),
        })?;

    let columns = reader.list_columns(table_def.id)?;

    for idx_col in &s.columns {
        if !columns.iter().any(|c| c.name == idx_col.name) {
            let available = columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DbError::ColumnNotFound {
                name: idx_col.name.clone(),
                table: format!("\"{}\" (available: {})", s.table.name, available),
            });
        }
    }

    Ok(s)
}

// ── ALTER TABLE ───────────────────────────────────────────────────────────────

fn analyze_alter_table(
    s: AlterTableStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<AlterTableStmt, DbError> {
    let schema = s.table.schema.as_deref().unwrap_or(default_schema);
    let mut reader = CatalogReader::new(storage, snapshot)?;

    // Validate the target table exists.
    reader
        .get_table_in_database(default_database, schema, &s.table.name)?
        .ok_or_else(|| DbError::TableNotFound {
            name: s.table.name.clone(),
        })?;

    // Individual operations validated at execution time (Phase 4.22).
    // For now just validate the table exists.
    let _ = s.operations; // suppress unused warning
    Ok(s)
}
