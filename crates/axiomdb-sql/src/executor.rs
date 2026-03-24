//! Basic SQL executor — interprets an analyzed [`Stmt`] and produces a [`QueryResult`].
//!
//! ## Entry point
//!
//! [`execute`] is the single public function. It accepts an analyzed statement
//! (all `col_idx` resolved by the semantic analyzer) and drives it to completion,
//! returning a [`QueryResult`].
//!
//! ## Autocommit
//!
//! If no transaction is active when `execute` is called, the statement is
//! automatically wrapped in an implicit `BEGIN / COMMIT` via
//! [`TxnManager::autocommit`]. Transaction control statements (`BEGIN`,
//! `COMMIT`, `ROLLBACK`) bypass autocommit and operate on the `TxnManager`
//! directly.
//!
//! ## Snapshot selection
//!
//! All reads inside a statement use [`TxnManager::active_snapshot`] so that
//! writes made earlier in the same transaction are visible (read-your-own-writes).
//! This is always valid because:
//! - In autocommit mode, `autocommit()` calls `begin()` before invoking the handler.
//! - In explicit transaction mode, `begin()` was already called by the user.
//!
//! ## Phase 4.5 scope
//!
//! Supported: SELECT (with optional WHERE + projection), SELECT without FROM,
//! INSERT VALUES, UPDATE, DELETE, CREATE TABLE, DROP TABLE, CREATE INDEX,
//! DROP INDEX, BEGIN / COMMIT / ROLLBACK, SET (stub).
//!
//! Not yet supported (returns [`DbError::NotImplemented`]):
//! JOIN, GROUP BY, ORDER BY, LIMIT, DISTINCT, subqueries in FROM, INSERT SELECT,
//! TRUNCATE, ALTER TABLE, SHOW TABLES / DESCRIBE.

use std::collections::HashMap;

use axiomdb_catalog::{
    schema::{
        ColumnDef as CatalogColumnDef, ColumnType, IndexColumnDef, IndexDef,
        SortOrder as CatalogSortOrder, TableDef,
    },
    CatalogReader, CatalogWriter, ResolvedTable, SchemaResolver,
};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
use axiomdb_index::BTree;
use axiomdb_storage::{heap_chain::HeapChain, Page, PageType, StorageEngine};
use axiomdb_types::{DataType, Value};
use axiomdb_wal::TxnManager;

use crate::{
    ast::{
        AlterTableOp, AlterTableStmt, ColumnConstraint, CreateIndexStmt, CreateTableStmt,
        DeleteStmt, DropIndexStmt, DropTableStmt, FromClause, InsertSource, InsertStmt, JoinClause,
        JoinCondition, JoinType, NullsOrder, OrderByItem, SelectItem, SelectStmt, SortOrder, Stmt,
        UpdateStmt,
    },
    eval::{eval, eval_with, is_truthy, SubqueryRunner},
    expr::{BinaryOp, Expr},
    result::{ColumnMeta, QueryResult, Row},
    session::SessionContext,
    table::TableEngine,
};

// ── AUTO_INCREMENT sequence state ─────────────────────────────────────────────

use std::cell::{Cell, RefCell};
use std::collections::HashMap as StdHashMap;

thread_local! {
    /// Per-table AUTO_INCREMENT sequence counter (TableId → next value to assign).
    /// Initialized lazily: on first auto-insert, the executor scans the table to
    /// find MAX(auto_col) and seeds the counter from MAX+1.
    static AUTO_INC_SEQ: RefCell<StdHashMap<u32, u64>> = RefCell::new(StdHashMap::new());

    /// The last auto-generated ID produced by this thread.
    /// Read by `LAST_INSERT_ID()` / `lastval()` in the expression evaluator.
    static THREAD_LAST_INSERT_ID: Cell<u64> = const { Cell::new(0) };
}

/// Returns the value of `LAST_INSERT_ID()` for the current thread.
/// Exported so `eval.rs` can call it from `eval_function`.
pub(crate) fn last_insert_id_value() -> u64 {
    THREAD_LAST_INSERT_ID.with(|v| v.get())
}

// ── Subquery execution support ────────────────────────────────────────────────

/// Walks a `SelectStmt` AST and substitutes every `Expr::OuterColumn { col_idx }`
/// with `Expr::Literal(outer_row[col_idx])`, producing a fully self-contained
/// statement ready for inner execution.
///
/// Called once per outer row for correlated subqueries. Uncorrelated subqueries
/// contain no `OuterColumn` nodes — `substitute_outer` is a no-op for them.
fn substitute_outer(mut stmt: SelectStmt, outer_row: &[Value]) -> SelectStmt {
    stmt.where_clause = stmt.where_clause.map(|e| subst_expr(e, outer_row));
    stmt.columns = stmt
        .columns
        .into_iter()
        .map(|item| match item {
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: subst_expr(expr, outer_row),
                alias,
            },
            other => other,
        })
        .collect();
    stmt.having = stmt.having.map(|e| subst_expr(e, outer_row));
    stmt.group_by = stmt
        .group_by
        .into_iter()
        .map(|e| subst_expr(e, outer_row))
        .collect();
    stmt.order_by = stmt
        .order_by
        .into_iter()
        .map(|mut item| {
            item.expr = subst_expr(item.expr, outer_row);
            item
        })
        .collect();
    stmt.joins = stmt
        .joins
        .into_iter()
        .map(|mut join| {
            use crate::ast::JoinCondition;
            join.condition = match join.condition {
                JoinCondition::On(e) => JoinCondition::On(subst_expr(e, outer_row)),
                other => other,
            };
            join
        })
        .collect();
    stmt
}

/// Recursively replaces `OuterColumn` nodes with `Literal` values from `outer_row`.
fn subst_expr(expr: Expr, outer_row: &[Value]) -> Expr {
    match expr {
        Expr::OuterColumn { col_idx, .. } => {
            Expr::Literal(outer_row.get(col_idx).cloned().unwrap_or(Value::Null))
        }
        // Compound nodes — recurse.
        Expr::UnaryOp { op, operand } => Expr::UnaryOp {
            op,
            operand: Box::new(subst_expr(*operand, outer_row)),
        },
        Expr::BinaryOp { op, left, right } => Expr::BinaryOp {
            op,
            left: Box::new(subst_expr(*left, outer_row)),
            right: Box::new(subst_expr(*right, outer_row)),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(subst_expr(*expr, outer_row)),
            negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(subst_expr(*expr, outer_row)),
            low: Box::new(subst_expr(*low, outer_row)),
            high: Box::new(subst_expr(*high, outer_row)),
            negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(subst_expr(*expr, outer_row)),
            pattern: Box::new(subst_expr(*pattern, outer_row)),
            negated,
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: Box::new(subst_expr(*expr, outer_row)),
            list: list.into_iter().map(|e| subst_expr(e, outer_row)).collect(),
            negated,
        },
        Expr::Function { name, args } => Expr::Function {
            name,
            args: args.into_iter().map(|a| subst_expr(a, outer_row)).collect(),
        },
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => Expr::Case {
            operand: operand.map(|e| Box::new(subst_expr(*e, outer_row))),
            when_thens: when_thens
                .into_iter()
                .map(|(w, t)| (subst_expr(w, outer_row), subst_expr(t, outer_row)))
                .collect(),
            else_result: else_result.map(|e| Box::new(subst_expr(*e, outer_row))),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(subst_expr(*expr, outer_row)),
            target,
        },
        Expr::Subquery(inner) => Expr::Subquery(Box::new(substitute_outer(*inner, outer_row))),
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(subst_expr(*expr, outer_row)),
            query: Box::new(substitute_outer(*query, outer_row)),
            negated,
        },
        Expr::Exists { query, negated } => Expr::Exists {
            query: Box::new(substitute_outer(*query, outer_row)),
            negated,
        },
        // Leaf nodes — no substitution needed.
        other => other,
    }
}

/// [`SubqueryRunner`] that executes inner queries through the executor,
/// substituting outer-row references before running.
///
/// Holds mutable refs to `storage`, `txn`, and `ctx`, plus the current
/// outer row for `substitute_outer`. Created fresh for each outer row.
struct ExecSubqueryRunner<'a> {
    storage: &'a mut dyn StorageEngine,
    txn: &'a mut TxnManager,
    ctx: &'a mut SessionContext,
    outer_row: &'a [Value],
}

impl<'a> SubqueryRunner for ExecSubqueryRunner<'a> {
    fn run(&mut self, stmt: &SelectStmt) -> Result<QueryResult, DbError> {
        let bound = substitute_outer(stmt.clone(), self.outer_row);
        execute_select_ctx(bound, self.storage, self.txn, self.ctx)
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Executes a single analyzed SQL statement.
///
/// If no transaction is currently active, the statement is automatically wrapped
/// in an implicit `BEGIN / COMMIT` (autocommit mode). On error in autocommit mode,
/// the transaction is automatically rolled back.
///
/// If a transaction is already active, the executor participates in it without
/// committing — the caller is responsible for `COMMIT` or `ROLLBACK`.
///
/// Transaction control statements (`BEGIN`, `COMMIT`, `ROLLBACK`) operate directly
/// on `txn` regardless of autocommit state.
///
/// # Errors
///
/// Any [`DbError`] from storage, WAL, catalog, type coercion, or expression
/// evaluation. [`DbError::NotImplemented`] is returned for SQL features deferred
/// to later phases (JOIN, GROUP BY, ORDER BY, LIMIT, DISTINCT, subqueries).
pub fn execute(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    if txn.active_txn_id().is_some() {
        // Inside an explicit transaction — execute without auto-committing.
        dispatch(stmt, storage, txn)
    } else {
        // Autocommit: BEGIN + execute + COMMIT, with automatic ROLLBACK on error.
        // We cannot use txn.autocommit() here because its closure signature
        // (FnOnce(&mut TxnManager)) does not provide storage, but dispatch() needs it.
        match stmt {
            Stmt::Begin => {
                txn.begin()?;
                Ok(QueryResult::Empty)
            }
            Stmt::Commit => Err(DbError::NoActiveTransaction),
            Stmt::Rollback => Err(DbError::NoActiveTransaction),
            other => {
                txn.begin()?;
                match dispatch(other, storage, txn) {
                    Ok(result) => {
                        txn.commit()?;
                        Ok(result)
                    }
                    Err(e) => {
                        // Best-effort rollback — ignore rollback error to surface the original.
                        let _ = txn.rollback(storage);
                        Err(e)
                    }
                }
            }
        }
    }
}

// ── execute_with_ctx ──────────────────────────────────────────────────────────

/// Like [`execute`] but uses a persistent [`SessionContext`] for schema caching.
///
/// Prefer this over `execute` for workloads that run many statements on the
/// same connection. The `SessionContext` caches `ResolvedTable` values across
/// calls, eliminating the O(n) catalog heap scan on every DML statement.
///
/// DDL statements (`CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`)
/// automatically invalidate the full schema cache before executing, so the
/// next lookup sees the updated catalog.
pub fn execute_with_ctx(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    if txn.active_txn_id().is_some() {
        dispatch_ctx(stmt, storage, txn, ctx)
    } else {
        match stmt {
            Stmt::Begin => {
                txn.begin()?;
                Ok(QueryResult::Empty)
            }
            Stmt::Commit => Err(DbError::NoActiveTransaction),
            Stmt::Rollback => Err(DbError::NoActiveTransaction),
            other => {
                txn.begin()?;
                match dispatch_ctx(other, storage, txn, ctx) {
                    Ok(result) => {
                        txn.commit()?;
                        Ok(result)
                    }
                    Err(e) => {
                        let _ = txn.rollback(storage);
                        Err(e)
                    }
                }
            }
        }
    }
}

/// Routes a statement to its handler using a `SessionContext` for schema caching.
fn dispatch_ctx(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    match stmt {
        Stmt::Select(s) => execute_select_ctx(s, storage, txn, ctx),
        Stmt::Insert(s) => execute_insert_ctx(s, storage, txn, ctx),
        Stmt::Update(s) => execute_update_ctx(s, storage, txn, ctx),
        Stmt::Delete(s) => execute_delete_ctx(s, storage, txn, ctx),
        // DDL: invalidate cache, then delegate to existing handlers.
        Stmt::CreateTable(s) => {
            ctx.invalidate_all();
            execute_create_table(s, storage, txn)
        }
        Stmt::DropTable(s) => {
            ctx.invalidate_all();
            execute_drop_table(s, storage, txn)
        }
        Stmt::CreateIndex(s) => {
            ctx.invalidate_all();
            execute_create_index(s, storage, txn)
        }
        Stmt::DropIndex(s) => {
            ctx.invalidate_all();
            execute_drop_index(s, storage, txn)
        }
        Stmt::AlterTable(s) => {
            ctx.invalidate_all();
            execute_alter_table(s, storage, txn)
        }
        // Everything else: delegate to existing dispatch.
        other => dispatch(other, storage, txn),
    }
}

/// Resolves a table, using the session cache to avoid repeated catalog scans.
///
/// On cache miss, calls `make_resolver` and stores the result in `ctx`.
fn resolve_table_cached(
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
    schema: Option<&str>,
    table_name: &str,
) -> Result<ResolvedTable, DbError> {
    let schema_str = schema.unwrap_or("public");
    if let Some(cached) = ctx.get_table(schema_str, table_name) {
        return Ok(cached.clone());
    }
    let mut resolver = make_resolver(storage, txn)?;
    let resolved = resolver.resolve_table(schema, table_name)?;
    ctx.cache_table(schema_str, table_name, resolved.clone());
    Ok(resolved)
}

// ── ctx-aware DML handlers ────────────────────────────────────────────────────

fn execute_select_ctx(
    mut stmt: SelectStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // SELECT without FROM: no table resolution needed.
    if stmt.from.is_none() {
        return execute_select(stmt, storage, txn);
    }

    // Subquery in FROM: no caching path yet — delegate.
    if matches!(stmt.from, Some(FromClause::Subquery { .. })) {
        return execute_select(stmt, storage, txn);
    }

    let from_table_ref = match stmt.from.take() {
        Some(FromClause::Table(tref)) => tref,
        _ => unreachable!("already handled None and Subquery above"),
    };

    if stmt.joins.is_empty() {
        // Single-table path — use cache.
        let resolved = resolve_table_cached(
            storage,
            txn,
            ctx,
            from_table_ref.schema.as_deref(),
            &from_table_ref.name,
        )?;

        let snap = txn.active_snapshot()?;

        // Build column mask for lazy decode: only decode columns referenced in
        // SELECT list, WHERE, ORDER BY, GROUP BY, HAVING. SELECT * passes None.
        let n_cols = resolved.columns.len();
        let has_wildcard = stmt.columns.iter().any(|item| {
            matches!(
                item,
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_)
            )
        });
        let column_mask: Option<Vec<bool>> = if has_wildcard || n_cols == 0 {
            None
        } else {
            let mut expr_ptrs: Vec<&Expr> = Vec::new();
            for item in &stmt.columns {
                if let SelectItem::Expr { expr, .. } = item {
                    expr_ptrs.push(expr);
                }
            }
            if let Some(ref wc) = stmt.where_clause {
                expr_ptrs.push(wc);
            }
            for ob in &stmt.order_by {
                expr_ptrs.push(&ob.expr);
            }
            for gb in &stmt.group_by {
                expr_ptrs.push(gb);
            }
            if let Some(ref hav) = stmt.having {
                expr_ptrs.push(hav);
            }
            let mask = build_column_mask(n_cols, &expr_ptrs);
            if mask.iter().all(|&b| b) {
                None
            } else {
                Some(mask)
            }
        };

        // scan_table returns owned Vec — storage is free after this call.
        let raw_rows = TableEngine::scan_table(
            storage,
            &resolved.def,
            &resolved.columns,
            snap,
            column_mask.as_deref(),
        )?;

        let mut combined_rows: Vec<Row> = Vec::new();
        for (_rid, values) in raw_rows {
            if let Some(ref wc) = stmt.where_clause {
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    ctx,
                    outer_row: &values,
                };
                if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
                    continue;
                }
            }
            combined_rows.push(values);
        }

        if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
            return execute_select_grouped(stmt, combined_rows);
        }

        combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

        let out_cols = build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
        let mut rows = combined_rows
            .iter()
            .map(|v| {
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    ctx,
                    outer_row: v,
                };
                project_row_with(&stmt.columns, v, &mut runner)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if stmt.distinct {
            rows = apply_distinct(rows);
        }
        rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

        Ok(QueryResult::Rows {
            columns: out_cols,
            rows,
        })
    } else {
        // Multi-table JOIN path — use cache for each table.
        execute_select_with_joins_ctx(stmt, from_table_ref, storage, txn, ctx)
    }
}

fn execute_select_with_joins_ctx(
    stmt: SelectStmt,
    from_ref: crate::ast::TableRef,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let mut all_resolved: Vec<axiomdb_catalog::ResolvedTable> = Vec::new();
    let mut col_offsets: Vec<usize> = Vec::new();
    let mut running_offset = 0usize;

    {
        let from_t = resolve_table_cached(
            storage,
            txn,
            ctx,
            from_ref.schema.as_deref(),
            &from_ref.name,
        )?;
        col_offsets.push(running_offset);
        running_offset += from_t.columns.len();
        all_resolved.push(from_t);

        for join in &stmt.joins {
            match &join.table {
                FromClause::Table(tref) => {
                    let jt = resolve_table_cached(
                        storage,
                        txn,
                        ctx,
                        tref.schema.as_deref(),
                        &tref.name,
                    )?;
                    col_offsets.push(running_offset);
                    running_offset += jt.columns.len();
                    all_resolved.push(jt);
                }
                FromClause::Subquery { .. } => {
                    return Err(DbError::NotImplemented {
                        feature: "subquery in JOIN — Phase 4.11".into(),
                    })
                }
            }
        }
    }

    let snap = txn.active_snapshot()?;
    let mut scanned: Vec<Vec<Row>> = Vec::with_capacity(all_resolved.len());
    for t in &all_resolved {
        let rows = TableEngine::scan_table(storage, &t.def, &t.columns, snap, None)?;
        scanned.push(rows.into_iter().map(|(_, r)| r).collect());
    }

    let mut combined_rows: Vec<Row> = scanned[0].clone();
    let mut left_col_count = all_resolved[0].columns.len();

    let mut left_schema: Vec<(String, usize)> = all_resolved[0]
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.clone(), i))
        .collect();

    for (i, join) in stmt.joins.iter().enumerate() {
        if join.join_type == JoinType::Full {
            return Err(DbError::NotImplemented {
                feature: "FULL OUTER JOIN — Phase 4.8+".into(),
            });
        }

        let right_idx = i + 1;
        let right_col_count = all_resolved[right_idx].columns.len();
        let right_col_offset = col_offsets[right_idx];

        combined_rows = apply_join(
            combined_rows,
            &scanned[right_idx],
            left_col_count,
            right_col_count,
            join.join_type,
            &join.condition,
            &left_schema,
            right_col_offset,
            &all_resolved[right_idx].columns,
        )?;

        for (j, col) in all_resolved[right_idx].columns.iter().enumerate() {
            left_schema.push((col.name.clone(), right_col_offset + j));
        }
        left_col_count += right_col_count;
    }

    if let Some(ref wc) = stmt.where_clause {
        let mut filtered = Vec::with_capacity(combined_rows.len());
        for row in combined_rows {
            if is_truthy(&eval(wc, &row)?) {
                filtered.push(row);
            }
        }
        combined_rows = filtered;
    }

    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        return execute_select_grouped(stmt, combined_rows);
    }

    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

    let out_cols = build_join_column_meta(&stmt.columns, &all_resolved, &stmt.joins)?;

    let mut rows = combined_rows
        .iter()
        .map(|r| project_row(&stmt.columns, r))
        .collect::<Result<Vec<_>, _>>()?;

    if stmt.distinct {
        rows = apply_distinct(rows);
    }
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

fn execute_insert_ctx(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let resolved = resolve_table_cached(
        storage,
        txn,
        ctx,
        stmt.table.schema.as_deref(),
        &stmt.table.name,
    )?;

    let schema_cols = &resolved.columns;

    let col_positions: Vec<usize> = match &stmt.columns {
        None => (0..schema_cols.len()).collect(),
        Some(named_cols) => {
            let mut map = vec![usize::MAX; schema_cols.len()];
            for (val_pos, col_name) in named_cols.iter().enumerate() {
                let schema_pos = schema_cols
                    .iter()
                    .position(|c| &c.name == col_name)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: resolved.def.table_name.clone(),
                    })?;
                map[schema_pos] = val_pos;
            }
            map
        }
    };

    let mut count = 0u64;

    // Find the AUTO_INCREMENT column (at most one per table).
    let auto_inc_col: Option<usize> = schema_cols.iter().position(|c| c.auto_increment);
    let mut first_generated: Option<u64> = None;

    fn next_auto_inc_ctx(
        storage: &mut dyn StorageEngine,
        txn: &TxnManager,
        table_def: &axiomdb_catalog::schema::TableDef,
        schema_cols: &[axiomdb_catalog::schema::ColumnDef],
        col_idx: usize,
    ) -> Result<u64, DbError> {
        let table_id = table_def.id;
        let cached = AUTO_INC_SEQ.with(|seq| seq.borrow().get(&table_id).copied());
        if let Some(next) = cached {
            AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
            return Ok(next);
        }
        let snap = txn.active_snapshot()?;
        let rows = TableEngine::scan_table(storage, table_def, schema_cols, snap, None)?;
        let max_existing: u64 = rows
            .iter()
            .filter_map(|(_, vals)| vals.get(col_idx))
            .filter_map(|v| match v {
                Value::Int(n) => Some(*n as u64),
                Value::BigInt(n) => Some(*n as u64),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let next = max_existing + 1;
        AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
        Ok(next)
    }

    match stmt.source {
        InsertSource::Values(rows) => {
            for value_exprs in rows {
                let provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;

                let mut full_values: Vec<Value> = col_positions
                    .iter()
                    .map(|&idx| {
                        if idx == usize::MAX {
                            Value::Null
                        } else {
                            provided.get(idx).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();

                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id =
                            next_auto_inc_ctx(storage, txn, &resolved.def, schema_cols, ai_col)?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }

                TableEngine::insert_row(storage, txn, &resolved.def, schema_cols, full_values)?;
                count += 1;
            }
        }
        InsertSource::Select(select_stmt) => {
            let select_rows = match execute_select_ctx(*select_stmt, storage, txn, ctx)? {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(DbError::Other(format!(
                        "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                    )))
                }
            };
            for row_values in select_rows {
                let mut full_values: Vec<Value> = col_positions
                    .iter()
                    .map(|&idx| {
                        if idx == usize::MAX {
                            Value::Null
                        } else {
                            row_values.get(idx).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();
                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id =
                            next_auto_inc_ctx(storage, txn, &resolved.def, schema_cols, ai_col)?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }
                TableEngine::insert_row(storage, txn, &resolved.def, schema_cols, full_values)?;
                count += 1;
            }
        }
        InsertSource::DefaultValues => {
            return Err(DbError::NotImplemented {
                feature: "DEFAULT VALUES — Phase 4.3c".into(),
            })
        }
    }

    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        return Ok(QueryResult::affected_with_id(count, id));
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

fn execute_update_ctx(
    stmt: UpdateStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let resolved = resolve_table_cached(
        storage,
        txn,
        ctx,
        stmt.table.schema.as_deref(),
        &stmt.table.name,
    )?;

    let schema_cols = resolved.columns.clone();

    let assignments: Vec<(usize, Expr)> = stmt
        .assignments
        .into_iter()
        .map(|a| {
            let pos = schema_cols
                .iter()
                .position(|c| c.name == a.column)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: a.column.clone(),
                    table: resolved.def.table_name.clone(),
                })?;
            Ok((pos, a.value))
        })
        .collect::<Result<_, DbError>>()?;

    let snap = txn.active_snapshot()?;
    // UPDATE always needs all columns: unchanged columns carry over as-is to
    // the new row. Lazy decode (column_mask) does not help here.
    let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?;

    // Collect all matching (rid, new_values) pairs before touching the heap.
    // Inspired by OceanBase ObDASUpdIterator: accumulate (old, new) pairs,
    // then flush as one delete_batch + insert_batch pass — O(P) page I/O
    // instead of O(3N) for N per-row update_row() calls.
    let mut to_update: Vec<(RecordId, Vec<Value>)> = Vec::new();
    for (rid, current_values) in rows {
        if let Some(ref wc) = stmt.where_clause {
            if !is_truthy(&eval(wc, &current_values)?) {
                continue;
            }
        }
        let mut new_values = current_values.clone();
        for (col_pos, val_expr) in &assignments {
            new_values[*col_pos] = eval(val_expr, &current_values)?;
        }
        to_update.push((rid, new_values));
    }

    let count = to_update.len() as u64;

    match to_update.len() {
        0 => {}
        1 => {
            // Single-row path: avoid Vec allocation overhead of batch.
            let (rid, new_values) = to_update.pop().unwrap();
            TableEngine::update_row(storage, txn, &resolved.def, &schema_cols, rid, new_values)?;
        }
        _ => {
            // Multi-row batch: delete_batch + insert_batch — O(P) page I/O.
            TableEngine::update_rows_batch(storage, txn, &resolved.def, &schema_cols, to_update)?;
        }
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

fn execute_delete_ctx(
    stmt: DeleteStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let resolved = resolve_table_cached(
        storage,
        txn,
        ctx,
        stmt.table.schema.as_deref(),
        &stmt.table.name,
    )?;

    let snap = txn.active_snapshot()?;

    if stmt.where_clause.is_none() {
        // No-WHERE fast path: collect only (page_id, slot_id) pairs — skip full
        // row decode entirely (MariaDB ha_delete_all_rows pattern).
        let raw_rids = HeapChain::scan_rids_visible(storage, resolved.def.data_root_page_id, snap)?;
        let count = raw_rids.len() as u64;

        // Physical heap update: mark all slots dead with ONE heap-chain pass.
        HeapChain::delete_batch(
            storage,
            resolved.def.data_root_page_id,
            &raw_rids,
            snap.current_txn_id,
        )?;

        // WAL: ONE Truncate entry instead of N Delete entries — 10,000× fewer WAL writes.
        txn.record_truncate(resolved.def.id, resolved.def.data_root_page_id)?;

        return Ok(QueryResult::Affected {
            count,
            last_insert_id: None,
        });
    }

    // WHERE clause path: need decoded row values to evaluate the predicate.
    // Build a column mask so only WHERE-referenced columns are decoded.
    let schema_cols = resolved.columns.clone();
    let n_cols = schema_cols.len();
    let column_mask: Option<Vec<bool>> = stmt
        .where_clause
        .as_ref()
        .map(|wc| {
            let mask = build_column_mask(n_cols, &[wc]);
            if mask.iter().all(|&b| b) {
                vec![]
            } else {
                mask
            }
        })
        .filter(|m| !m.is_empty());
    let rows = TableEngine::scan_table(
        storage,
        &resolved.def,
        &schema_cols,
        snap,
        column_mask.as_deref(),
    )?;
    let to_delete: Vec<RecordId> = rows
        .into_iter()
        .filter_map(|(rid, values)| match &stmt.where_clause {
            None => unreachable!(),
            Some(wc) => match eval(wc, &values) {
                Ok(v) if is_truthy(&v) => Some(Ok(rid)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            },
        })
        .collect::<Result<_, DbError>>()?;

    // Batch-delete: each heap page read+written once instead of 3× per row.
    let count = TableEngine::delete_rows_batch(storage, txn, &resolved.def, &to_delete)?;

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── Column mask for lazy decode ───────────────────────────────────────────────

/// Builds a boolean mask over `n_cols` columns. `mask[i]` is `true` if column
/// `i` is referenced by any expression in `exprs`. Used by `execute_select_ctx`
/// and `execute_delete_ctx` to tell `scan_table` which columns to decode.
///
/// Conservative: any [`SelectItem::Wildcard`] or [`SelectItem::QualifiedWildcard`]
/// in the query's SELECT list will cause the caller to pass `None` instead (full
/// decode), so this function is only called when the select list is fully
/// resolved to column expressions.
fn build_column_mask(n_cols: usize, exprs: &[&Expr]) -> Vec<bool> {
    let mut mask = vec![false; n_cols];
    for expr in exprs {
        collect_column_refs(expr, &mut mask);
    }
    mask
}

/// Walks `expr` and marks every referenced local column index in `mask`.
///
/// Does **not** recurse into subquery bodies (`Subquery`, `InSubquery`,
/// `Exists`) — those reference an inner scope with a different row layout.
/// [`OuterColumn`] references point to an enclosing scope, not this row.
fn collect_column_refs(expr: &Expr, mask: &mut Vec<bool>) {
    match expr {
        Expr::Column { col_idx, .. } => {
            if *col_idx < mask.len() {
                mask[*col_idx] = true;
            }
        }
        Expr::Literal(_) | Expr::OuterColumn { .. } | Expr::Param { .. } => {}
        Expr::UnaryOp { operand, .. } => collect_column_refs(operand, mask),
        Expr::BinaryOp { left, right, .. } => {
            collect_column_refs(left, mask);
            collect_column_refs(right, mask);
        }
        Expr::IsNull { expr, .. } => collect_column_refs(expr, mask),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_column_refs(expr, mask);
            collect_column_refs(low, mask);
            collect_column_refs(high, mask);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_column_refs(expr, mask);
            collect_column_refs(pattern, mask);
        }
        Expr::In { expr, list, .. } => {
            collect_column_refs(expr, mask);
            for e in list {
                collect_column_refs(e, mask);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_column_refs(a, mask);
            }
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                collect_column_refs(op, mask);
            }
            for (w, t) in when_thens {
                collect_column_refs(w, mask);
                collect_column_refs(t, mask);
            }
            if let Some(e) = else_result {
                collect_column_refs(e, mask);
            }
        }
        Expr::Cast { expr, .. } => collect_column_refs(expr, mask),
        // InSubquery: recurse only on the outer expression, not the inner query.
        Expr::InSubquery { expr, .. } => collect_column_refs(expr, mask),
        // Subquery and Exists reference inner scopes — do not recurse.
        Expr::Subquery(_) | Expr::Exists { .. } => {}
    }
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

/// Routes a statement to its handler. Called both inside `autocommit` and
/// directly when an explicit transaction is already active.
fn dispatch(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    match stmt {
        // DML
        Stmt::Select(s) => execute_select(s, storage, txn),
        Stmt::Insert(s) => execute_insert(s, storage, txn),
        Stmt::Update(s) => execute_update(s, storage, txn),
        Stmt::Delete(s) => execute_delete(s, storage, txn),
        // DDL
        Stmt::CreateTable(s) => execute_create_table(s, storage, txn),
        Stmt::DropTable(s) => execute_drop_table(s, storage, txn),
        Stmt::CreateIndex(s) => execute_create_index(s, storage, txn),
        Stmt::DropIndex(s) => execute_drop_index(s, storage, txn),
        // Transaction control (when already inside a txn)
        Stmt::Begin => Err(DbError::TransactionAlreadyActive {
            txn_id: txn.active_txn_id().unwrap_or(0),
        }),
        Stmt::Commit => {
            txn.commit()?;
            Ok(QueryResult::Empty)
        }
        Stmt::Rollback => {
            txn.rollback(storage)?;
            Ok(QueryResult::Empty)
        }
        // Session variables — stub for Phase 5
        Stmt::Set(_) => Ok(QueryResult::Empty),
        // Deferred statements
        Stmt::TruncateTable(s) => execute_truncate(s, storage, txn),
        Stmt::AlterTable(s) => execute_alter_table(s, storage, txn),
        Stmt::ShowTables(s) => execute_show_tables(s, storage, txn),
        Stmt::ShowColumns(s) => execute_show_columns(s, storage, txn),
    }
}

// ── SELECT ───────────────────────────────────────────────────────────────────

fn execute_select(
    mut stmt: SelectStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    // Dispatch based on FROM clause type and whether JOINs are present.
    if stmt.from.is_none() {
        // ── SELECT without FROM ───────────────────────────────────────────────
        // Subqueries in the SELECT list (EXISTS, IN subquery, scalar subquery)
        // require a runner; we use a temporary SessionContext.
        let mut temp_ctx = SessionContext::new();
        let mut runner = ExecSubqueryRunner {
            storage,
            txn,
            ctx: &mut temp_ctx,
            outer_row: &[],
        };
        let mut out_row: Row = Vec::new();
        let mut out_cols: Vec<ColumnMeta> = Vec::new();
        for item in &stmt.columns {
            match item {
                SelectItem::Expr { expr, alias } => {
                    let v = eval_with(expr, &[], &mut runner)?;
                    let name = alias
                        .clone()
                        .unwrap_or_else(|| expr_column_name(expr, None));
                    let dt = datatype_of_value(&v);
                    out_cols.push(ColumnMeta::computed(name, dt));
                    out_row.push(v);
                }
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    return Err(DbError::NotImplemented {
                        feature: "SELECT * without FROM".into(),
                    });
                }
            }
        }
        let rows = if stmt.distinct {
            apply_distinct(vec![out_row])
        } else {
            vec![out_row]
        };
        return Ok(QueryResult::Rows {
            columns: out_cols,
            rows,
        });
    }

    // FROM is present — handle derived table (subquery in FROM) or real table.
    if matches!(stmt.from, Some(FromClause::Subquery { .. })) {
        return execute_select_derived(stmt, storage, txn);
    }

    // Extract the FROM table reference.
    let from_table_ref = match stmt.from.take() {
        Some(FromClause::Table(tref)) => tref,
        _ => unreachable!("already handled None and Subquery above"),
    };

    if stmt.joins.is_empty() {
        // ── Single-table path (no JOIN) ───────────────────────────────────────
        let resolved = {
            let mut resolver = make_resolver(storage, txn)?;
            resolver.resolve_table(from_table_ref.schema.as_deref(), &from_table_ref.name)?
        };

        let snap = txn.active_snapshot()?;

        // ── Query planner: pick the best access method ────────────────────
        // Use indexes already loaded by resolve_table (cached by SchemaCache).
        let access_method = crate::planner::plan_select(
            stmt.where_clause.as_ref(),
            &resolved.indexes,
            &resolved.columns,
        );

        // ── Fetch rows via the chosen access method ───────────────────────
        let raw_rows: Vec<(RecordId, Vec<Value>)> = match &access_method {
            crate::planner::AccessMethod::Scan => {
                // Full sequential scan — existing behavior.
                TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap, None)?
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key } => {
                // Point lookup: B-Tree → single RecordId → heap read.
                match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                    None => vec![],
                    Some(rid) => {
                        match TableEngine::read_row(storage, &resolved.columns, rid)? {
                            None => vec![], // row was deleted
                            Some(values) => vec![(rid, values)],
                        }
                    }
                }
            }
            crate::planner::AccessMethod::IndexRange { index_def, lo, hi } => {
                // Range scan: iterate B-Tree entries → heap reads.
                let pairs = BTree::range_in(
                    storage,
                    index_def.root_page_id,
                    lo.as_deref(),
                    hi.as_deref(),
                )?;
                let mut result = Vec::with_capacity(pairs.len());
                for (rid, _key) in pairs {
                    if let Some(values) = TableEngine::read_row(storage, &resolved.columns, rid)? {
                        result.push((rid, values));
                    }
                }
                result
            }
        };

        let mut combined_rows: Vec<Row> = Vec::new();
        for (_rid, values) in raw_rows {
            if let Some(ref wc) = stmt.where_clause {
                let mut temp_ctx = SessionContext::new();
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    ctx: &mut temp_ctx,
                    outer_row: &values,
                };
                if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
                    continue;
                }
            }
            combined_rows.push(values);
        }

        if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
            return execute_select_grouped(stmt, combined_rows);
        }

        combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

        let out_cols = build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
        let mut rows = combined_rows
            .iter()
            .map(|v| {
                let mut temp_ctx = SessionContext::new();
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    ctx: &mut temp_ctx,
                    outer_row: v,
                };
                project_row_with(&stmt.columns, v, &mut runner)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if stmt.distinct {
            rows = apply_distinct(rows);
        }
        rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

        Ok(QueryResult::Rows {
            columns: out_cols,
            rows,
        })
    } else {
        // ── Multi-table JOIN path ─────────────────────────────────────────────
        execute_select_with_joins(stmt, from_table_ref, storage, txn)
    }
}

/// Executes a SELECT whose FROM clause is a derived table: `FROM (SELECT ...) AS alias`.
///
/// The inner query is executed to produce a materialized set of rows, which are
/// then treated as a virtual table for the outer query's WHERE / GROUP BY / ORDER BY.
fn execute_select_derived(
    mut stmt: SelectStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let (inner_query, _alias) = match stmt.from.take() {
        Some(FromClause::Subquery { query, alias }) => (*query, alias),
        _ => unreachable!("execute_select_derived called with non-subquery FROM"),
    };

    // Execute the inner query to materialize the derived table.
    let mut temp_ctx = SessionContext::new();
    let inner_result = execute_select_ctx(inner_query, storage, txn, &mut temp_ctx)?;
    let (derived_cols, derived_rows) = match inner_result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        _ => {
            return Err(DbError::Internal {
                message: "derived table inner query did not return rows".into(),
            })
        }
    };

    // Apply outer WHERE.
    let mut combined_rows: Vec<Row> = Vec::new();
    for values in derived_rows {
        if let Some(ref wc) = stmt.where_clause {
            let mut temp_ctx2 = SessionContext::new();
            let mut runner = ExecSubqueryRunner {
                storage,
                txn,
                ctx: &mut temp_ctx2,
                outer_row: &values,
            };
            if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
                continue;
            }
        }
        combined_rows.push(values);
    }

    // GROUP BY / aggregation.
    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        return execute_select_grouped(stmt, combined_rows);
    }

    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

    // Build output columns from SELECT list against derived column metadata.
    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = combined_rows
        .iter()
        .map(|v| project_row(&stmt.columns, v))
        .collect::<Result<Vec<_>, _>>()?;

    if stmt.distinct {
        rows = apply_distinct(rows);
    }
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

// ── JOIN execution ───────────────────────────────────────────────────────────

/// Executes a SELECT with one or more JOINs using nested-loop strategy.
///
/// All tables are pre-scanned once. The combined row is built progressively:
/// - Stage 0: rows from the FROM table
/// - Stage i: `apply_join(stage_{i-1}, scan(JOIN[i].table), ...)`
///
/// WHERE is applied to the fully combined row after all joins.
fn execute_select_with_joins(
    stmt: SelectStmt,
    from_ref: crate::ast::TableRef,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    // Resolve all tables (FROM + each JOIN table) and compute col_offsets.
    let mut all_resolved: Vec<axiomdb_catalog::ResolvedTable> = Vec::new();
    let mut col_offsets: Vec<usize> = Vec::new(); // col_offset[i] = start of table i in combined row
    let mut running_offset = 0usize;

    {
        let mut resolver = make_resolver(storage, txn)?;
        let from_t = resolver.resolve_table(from_ref.schema.as_deref(), &from_ref.name)?;
        col_offsets.push(running_offset);
        running_offset += from_t.columns.len();
        all_resolved.push(from_t);

        for join in &stmt.joins {
            match &join.table {
                FromClause::Table(tref) => {
                    let jt = resolver.resolve_table(tref.schema.as_deref(), &tref.name)?;
                    col_offsets.push(running_offset);
                    running_offset += jt.columns.len();
                    all_resolved.push(jt);
                }
                FromClause::Subquery { .. } => {
                    return Err(DbError::NotImplemented {
                        feature: "subquery in JOIN — Phase 4.11".into(),
                    })
                }
            }
        }
    } // resolver dropped — storage immutable borrow released

    // Pre-scan all tables once (consistent snapshot for all).
    let snap = txn.active_snapshot()?;
    let mut scanned: Vec<Vec<Row>> = Vec::with_capacity(all_resolved.len());
    for t in &all_resolved {
        let rows = TableEngine::scan_table(storage, &t.def, &t.columns, snap, None)?;
        scanned.push(rows.into_iter().map(|(_, r)| r).collect());
    }

    // Progressive nested-loop join.
    let mut combined_rows: Vec<Row> = scanned[0].clone();
    let mut left_col_count = all_resolved[0].columns.len();

    // left_schema tracks (col_name, global_col_idx) for all accumulated left columns.
    // Used by USING conditions to locate column positions by name.
    let mut left_schema: Vec<(String, usize)> = all_resolved[0]
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.clone(), i))
        .collect();

    for (i, join) in stmt.joins.iter().enumerate() {
        if join.join_type == JoinType::Full {
            return Err(DbError::NotImplemented {
                feature: "FULL OUTER JOIN — Phase 4.8+".into(),
            });
        }

        let right_idx = i + 1;
        let right_col_count = all_resolved[right_idx].columns.len();
        let right_col_offset = col_offsets[right_idx];

        combined_rows = apply_join(
            combined_rows,
            &scanned[right_idx],
            left_col_count,
            right_col_count,
            join.join_type,
            &join.condition,
            &left_schema,
            right_col_offset,
            &all_resolved[right_idx].columns,
        )?;

        // Extend left_schema with the right table's columns at their global positions.
        for (j, col) in all_resolved[right_idx].columns.iter().enumerate() {
            left_schema.push((col.name.clone(), right_col_offset + j));
        }
        left_col_count += right_col_count;
    }

    // Apply WHERE against the full combined row.
    if let Some(ref wc) = stmt.where_clause {
        let mut filtered = Vec::with_capacity(combined_rows.len());
        for row in combined_rows {
            if is_truthy(&eval(wc, &row)?) {
                filtered.push(row);
            }
        }
        combined_rows = filtered;
    }

    // Branch: aggregation (GROUP BY / aggregate functions) or direct projection.
    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        return execute_select_grouped(stmt, combined_rows);
    }

    // Sort source rows before projection.
    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

    // Build output ColumnMeta.
    let out_cols = build_join_column_meta(&stmt.columns, &all_resolved, &stmt.joins)?;

    // Project SELECT list.
    let mut rows = combined_rows
        .iter()
        .map(|r| project_row(&stmt.columns, r))
        .collect::<Result<Vec<_>, _>>()?;

    // DISTINCT deduplication (after projection, before LIMIT).
    if stmt.distinct {
        rows = apply_distinct(rows);
    }
    // LIMIT/OFFSET applied after deduplication.
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Nested-loop join of `left_rows` against `right_rows`.
///
/// Each output row is `left_row ++ right_row`. For OUTER joins, unmatched
/// rows are padded with `Value::Null` on the non-driving side.
///
/// `right_col_offset` is the position of the right table's first column in
/// the combined row. Used only for `JoinCondition::Using`.
#[allow(clippy::too_many_arguments)] // 9 params needed: join context has inherent complexity
fn apply_join(
    left_rows: Vec<Row>,
    right_rows: &[Row],
    left_col_count: usize,
    right_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    left_schema: &[(String, usize)], // for USING: (col_name, global_col_idx) for left side
    right_col_offset: usize,
    right_columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<Vec<Row>, DbError> {
    match join_type {
        JoinType::Inner | JoinType::Cross => {
            let mut result = Vec::new();
            for left in &left_rows {
                for right in right_rows {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                    }
                }
            }
            Ok(result)
        }

        JoinType::Left => {
            let null_right: Row = vec![Value::Null; right_col_count];
            let mut result = Vec::new();
            for left in &left_rows {
                let mut matched = false;
                for right in right_rows {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched = true;
                    }
                }
                if !matched {
                    result.push(concat_rows(left, &null_right));
                }
            }
            Ok(result)
        }

        JoinType::Right => {
            let null_left: Row = vec![Value::Null; left_col_count];
            let mut matched_right = vec![false; right_rows.len()];
            let mut result = Vec::new();

            for left in &left_rows {
                for (i, right) in right_rows.iter().enumerate() {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched_right[i] = true;
                    }
                }
            }
            // Emit unmatched right rows with NULLs on the left side.
            for (i, right) in right_rows.iter().enumerate() {
                if !matched_right[i] {
                    result.push(concat_rows(&null_left, right));
                }
            }
            Ok(result)
        }

        JoinType::Full => Err(DbError::NotImplemented {
            feature: "FULL OUTER JOIN — Phase 4.8+".into(),
        }),
    }
}

/// Evaluates a join condition against a combined row.
///
/// - `On(expr)`: evaluates the expression directly (`col_idx` already resolved by analyzer).
/// - `Using(names)`: for each name, finds its index in the left schema and in the right
///   table, then checks equality. NULL = NULL is UNKNOWN (returns false per SQL semantics).
///
/// `left_schema` is a `(column_name, global_col_idx)` list for every column in the
/// accumulated left side of this join stage.
fn eval_join_cond(
    cond: &JoinCondition,
    combined: &[Value],
    left_schema: &[(String, usize)],
    right_col_offset: usize,
    right_columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<bool, DbError> {
    match cond {
        JoinCondition::On(expr) => Ok(is_truthy(&eval(expr, combined)?)),

        JoinCondition::Using(names) => {
            for col_name in names {
                // Find col_idx in the accumulated left schema.
                let left_idx = left_schema
                    .iter()
                    .find(|(name, _)| name == col_name)
                    .map(|(_, idx)| *idx)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: "left side (USING)".into(),
                    })?;

                // Find col_idx in the right table.
                let right_pos = right_columns
                    .iter()
                    .position(|c| &c.name == col_name)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: "right table (USING)".into(),
                    })?;
                let right_idx = right_col_offset + right_pos;

                let left_val = combined
                    .get(left_idx)
                    .ok_or(DbError::ColumnIndexOutOfBounds {
                        idx: left_idx,
                        len: combined.len(),
                    })?;
                let right_val = combined
                    .get(right_idx)
                    .ok_or(DbError::ColumnIndexOutOfBounds {
                        idx: right_idx,
                        len: combined.len(),
                    })?;

                // NULL = NULL is UNKNOWN in SQL 3-valued logic — no match.
                if matches!(left_val, Value::Null) || matches!(right_val, Value::Null) {
                    return Ok(false);
                }
                if left_val != right_val {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}

/// Concatenates two row slices into a new combined row.
#[inline]
fn concat_rows(left: &[Value], right: &[Value]) -> Row {
    let mut combined = Vec::with_capacity(left.len() + right.len());
    combined.extend_from_slice(left);
    combined.extend_from_slice(right);
    combined
}

/// Builds `ColumnMeta` for the output of a JOIN query.
fn build_join_column_meta(
    items: &[SelectItem],
    all_tables: &[axiomdb_catalog::ResolvedTable],
    joins: &[JoinClause],
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();

    for item in items {
        match item {
            SelectItem::Wildcard => {
                // Expand all columns from all tables in order.
                for (t_idx, table) in all_tables.iter().enumerate() {
                    let outer_nullable = is_outer_nullable(t_idx, joins);
                    for col in &table.columns {
                        out.push(ColumnMeta {
                            name: col.name.clone(),
                            data_type: column_type_to_datatype(col.col_type),
                            nullable: col.nullable || outer_nullable,
                            table_name: Some(table.def.table_name.clone()),
                        });
                    }
                }
            }

            SelectItem::QualifiedWildcard(qualifier) => {
                // Expand only the columns from the matching table.
                let t_idx = all_tables
                    .iter()
                    .position(|t| t.def.table_name == *qualifier || t.def.schema_name == *qualifier)
                    .ok_or_else(|| DbError::TableNotFound {
                        name: qualifier.clone(),
                    })?;
                let table = &all_tables[t_idx];
                let outer_nullable = is_outer_nullable(t_idx, joins);
                for col in &table.columns {
                    out.push(ColumnMeta {
                        name: col.name.clone(),
                        data_type: column_type_to_datatype(col.col_type),
                        nullable: col.nullable || outer_nullable,
                        table_name: Some(table.def.table_name.clone()),
                    });
                }
            }

            SelectItem::Expr { expr, alias } => {
                let name = expr_column_name(expr, alias.as_deref());
                // Infer type: plain column reference uses catalog type; others use Text fallback.
                let (dt, nullable) = infer_expr_type_join(expr, all_tables, joins);
                out.push(ColumnMeta {
                    name,
                    data_type: dt,
                    nullable,
                    table_name: None,
                });
            }
        }
    }
    Ok(out)
}

/// Returns true if the table at `t_idx` can have NULLs due to its position
/// in the join chain.
///
/// - Table 0 (FROM table): nullable if the first join is RIGHT.
/// - Table `i` (i > 0, the i-th JOIN table): nullable if join[i-1] is LEFT.
fn is_outer_nullable(t_idx: usize, joins: &[JoinClause]) -> bool {
    if t_idx == 0 {
        // FROM table is nullable if the first join is RIGHT.
        joins
            .first()
            .is_some_and(|j| j.join_type == JoinType::Right)
    } else {
        // JOIN[t_idx-1] table is nullable if that join is LEFT.
        joins
            .get(t_idx - 1)
            .is_some_and(|j| j.join_type == JoinType::Left)
    }
}

/// Infers (DataType, nullable) for an expression in a JOIN context.
fn infer_expr_type_join(
    expr: &Expr,
    all_tables: &[axiomdb_catalog::ResolvedTable],
    joins: &[JoinClause],
) -> (DataType, bool) {
    if let Expr::Column { col_idx, .. } = expr {
        // Find which table owns this col_idx and what the column type is.
        let mut offset = 0;
        for (t_idx, table) in all_tables.iter().enumerate() {
            let end = offset + table.columns.len();
            if *col_idx < end {
                let local_pos = col_idx - offset;
                if let Some(col) = table.columns.get(local_pos) {
                    let nullable = col.nullable || is_outer_nullable(t_idx, joins);
                    return (column_type_to_datatype(col.col_type), nullable);
                }
            }
            offset = end;
        }
    }
    (DataType::Text, true) // safe fallback for computed expressions
}

// ── GROUP BY / AGGREGATE execution ───────────────────────────────────────────

// ── Aggregate detection ───────────────────────────────────────────────────────

/// Returns `true` if `name` is a known aggregate function.
fn is_aggregate(name: &str) -> bool {
    matches!(name, "count" | "sum" | "min" | "max" | "avg")
}

/// Returns `true` if `expr` or any sub-expression is an aggregate call.
fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function { name, .. } if is_aggregate(name.as_str()) => true,
        Expr::BinaryOp { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::UnaryOp { operand, .. } => contains_aggregate(operand),
        Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::In { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Function { args, .. } => args.iter().any(contains_aggregate),
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || when_thens
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_result.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Cast { expr, .. } => contains_aggregate(expr),
        Expr::Literal(_) | Expr::Column { .. } | Expr::OuterColumn { .. } | Expr::Param { .. } => {
            false
        }
        // Subquery internals are analyzed independently; aggregates inside them
        // do not count as aggregates of the outer query.
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => false,
    }
}

/// Returns `true` if the SELECT list or HAVING clause contain any aggregate call.
fn has_aggregates(items: &[SelectItem], having: &Option<Expr>) -> bool {
    let in_select = items.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => contains_aggregate(expr),
        _ => false,
    });
    let in_having = having.as_ref().is_some_and(contains_aggregate);
    in_select || in_having
}

// ── Aggregate descriptor ──────────────────────────────────────────────────────

/// Descriptor for one aggregate expression in the query.
///
/// Collected from the SELECT list and HAVING clause before the scan loop.
/// Deduplicated: if `COUNT(*)` appears in both SELECT and HAVING, only one
/// `AggExpr` is created and both share the same accumulator index.
#[derive(Debug, Clone)]
struct AggExpr {
    /// Lowercase function name: "count", "sum", "min", "max", "avg".
    name: String,
    /// The argument expression. `None` for `COUNT(*)`.
    arg: Option<Expr>,
    /// Position in `GroupState::accumulators`. Preserved for diagnostics.
    #[allow(dead_code)]
    agg_idx: usize,
}

impl AggExpr {
    /// Returns `true` if this descriptor matches the given function call.
    fn matches(&self, name: &str, args: &[Expr]) -> bool {
        if self.name != name {
            return false;
        }
        match (&self.arg, args.first()) {
            // Both COUNT(*): arg = None, args is empty
            (None, None) => args.is_empty(),
            // Both have an argument — compare by col_idx if both are Column refs
            (Some(Expr::Column { col_idx: a, .. }), Some(Expr::Column { col_idx: b, .. })) => {
                a == b
            }
            // One has an arg, the other doesn't
            _ => false,
        }
    }
}

/// Walks `expr` and registers any aggregate function calls into `result`.
fn collect_agg_exprs_from(expr: &Expr, result: &mut Vec<AggExpr>) {
    match expr {
        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let arg = args.first().cloned();
            // Deduplicate: only add if not already registered.
            let already = result.iter().any(|ae| ae.matches(name.as_str(), args));
            if !already {
                let idx = result.len();
                result.push(AggExpr {
                    name: name.clone(),
                    arg,
                    agg_idx: idx,
                });
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_agg_exprs_from(left, result);
            collect_agg_exprs_from(right, result);
        }
        Expr::UnaryOp { operand, .. } => collect_agg_exprs_from(operand, result),
        Expr::IsNull { expr, .. } => collect_agg_exprs_from(expr, result),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_agg_exprs_from(expr, result);
            collect_agg_exprs_from(low, result);
            collect_agg_exprs_from(high, result);
        }
        Expr::In { expr, list, .. } => {
            collect_agg_exprs_from(expr, result);
            for e in list {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_agg_exprs_from(a, result);
            }
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_agg_exprs_from(op, result);
            }
            for (w, t) in when_thens {
                collect_agg_exprs_from(w, result);
                collect_agg_exprs_from(t, result);
            }
            if let Some(e) = else_result {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Cast { expr, .. } => collect_agg_exprs_from(expr, result),
        Expr::Literal(_)
        | Expr::Column { .. }
        | Expr::Like { .. }
        | Expr::OuterColumn { .. }
        | Expr::Param { .. } => {}
        // Aggregates inside a subquery belong to the inner query, not the outer.
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {}
    }
}

/// Builds the deduplicated list of aggregate expressions from SELECT + HAVING.
fn collect_agg_exprs(items: &[SelectItem], having: &Option<Expr>) -> Vec<AggExpr> {
    let mut result = Vec::new();
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_agg_exprs_from(expr, &mut result);
        }
    }
    if let Some(h) = having {
        collect_agg_exprs_from(h, &mut result);
    }
    result
}

// ── Accumulator ───────────────────────────────────────────────────────────────

/// Per-group state for a single aggregate expression.
#[derive(Debug)]
enum AggAccumulator {
    /// `COUNT(*)` — increments for every row.
    CountStar { n: u64 },
    /// `COUNT(col)` — increments only for non-NULL values.
    CountCol { n: u64 },
    /// `SUM(col)` — sum of non-NULL values. `None` = all values were NULL.
    Sum { acc: Option<Value> },
    /// `MIN(col)` — minimum non-NULL value.
    Min { acc: Option<Value> },
    /// `MAX(col)` — maximum non-NULL value.
    Max { acc: Option<Value> },
    /// `AVG(col)` — running sum + count; final = sum / count as Real.
    Avg { sum: Value, count: u64 },
}

impl AggAccumulator {
    fn new(agg: &AggExpr) -> Self {
        match agg.name.as_str() {
            "count" if agg.arg.is_none() => Self::CountStar { n: 0 },
            "count" => Self::CountCol { n: 0 },
            "sum" => Self::Sum { acc: None },
            "min" => Self::Min { acc: None },
            "max" => Self::Max { acc: None },
            "avg" => Self::Avg {
                sum: Value::Int(0),
                count: 0,
            },
            _ => unreachable!("AggAccumulator::new called with non-aggregate"),
        }
    }

    fn update(&mut self, row: &[Value], agg: &AggExpr) -> Result<(), DbError> {
        match self {
            Self::CountStar { n } => *n += 1,

            Self::CountCol { n } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *n += 1;
                }
            }

            Self::Sum { acc } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v,
                        Some(a) => agg_add(a, v)?,
                    });
                }
            }

            Self::Min { acc } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v.clone(),
                        Some(a) => {
                            if agg_compare(&v, &a)? == std::cmp::Ordering::Less {
                                v
                            } else {
                                a
                            }
                        }
                    });
                }
            }

            Self::Max { acc } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v.clone(),
                        Some(a) => {
                            if agg_compare(&v, &a)? == std::cmp::Ordering::Greater {
                                v
                            } else {
                                a
                            }
                        }
                    });
                }
            }

            Self::Avg { sum, count } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *sum = agg_add(sum.clone(), v)?;
                    *count += 1;
                }
            }
        }
        Ok(())
    }

    fn finalize(self) -> Result<Value, DbError> {
        match self {
            Self::CountStar { n } => Ok(Value::BigInt(n as i64)),
            Self::CountCol { n } => Ok(Value::BigInt(n as i64)),
            Self::Sum { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Min { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Max { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Avg { sum, count } => finalize_avg(sum, count),
        }
    }
}

/// Add two values for aggregation (reuses `eval` for type handling and coercion).
fn agg_add(a: Value, b: Value) -> Result<Value, DbError> {
    eval(
        &Expr::BinaryOp {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(a)),
            right: Box::new(Expr::Literal(b)),
        },
        &[],
    )
}

/// Compare two values for MIN/MAX (returns Ordering).
fn agg_compare(a: &Value, b: &Value) -> Result<std::cmp::Ordering, DbError> {
    // Delegate to eval: if a < b → Less, if a = b → Equal, else Greater.
    let lt = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    )?;
    if is_truthy(&lt) {
        return Ok(std::cmp::Ordering::Less);
    }
    let eq = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    )?;
    if is_truthy(&eq) {
        Ok(std::cmp::Ordering::Equal)
    } else {
        Ok(std::cmp::Ordering::Greater)
    }
}

/// Finalize AVG: always produces `Real`. Returns `Null` if count == 0.
fn finalize_avg(sum: Value, count: u64) -> Result<Value, DbError> {
    if count == 0 {
        return Ok(Value::Null);
    }
    // Convert sum to Real.
    let sum_real = match sum {
        Value::Int(n) => Value::Real(n as f64),
        Value::BigInt(n) => Value::Real(n as f64),
        Value::Real(f) => Value::Real(f),
        Value::Decimal(m, s) => Value::Real(m as f64 * 10f64.powi(-(s as i32))),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "numeric".into(),
                got: other.variant_name().into(),
            })
        }
    };
    eval(
        &Expr::BinaryOp {
            op: BinaryOp::Div,
            left: Box::new(Expr::Literal(sum_real)),
            right: Box::new(Expr::Literal(Value::Real(count as f64))),
        },
        &[],
    )
}

// ── GroupState ────────────────────────────────────────────────────────────────

/// State for one GROUP BY group.
struct GroupState {
    /// Evaluated GROUP BY expression values (for future sort-based output — 4.9b).
    #[allow(dead_code)]
    key_values: Vec<Value>,
    /// One source row from this group — used by HAVING/SELECT to resolve column refs.
    representative_row: Row,
    /// One accumulator per aggregate in the query (SELECT + HAVING).
    accumulators: Vec<AggAccumulator>,
}

// ── GROUP BY key hashing ──────────────────────────────────────────────────────

/// Serializes a `Value` to a self-describing byte sequence for use as a
/// GROUP BY hash key.
///
/// Properties:
/// - Two `NULL` values produce identical bytes `[0x00]` → they form one group
///   (SQL grouping semantics: NULLs are considered equal for GROUP BY).
/// - `Real(f64)` uses `to_bits()` for bit-exact representation. `NaN` would
///   produce a fixed bit pattern, but NaN is forbidden in stored values.
/// - The tag byte guarantees values of different types never collide.
fn value_to_key_bytes(v: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    match v {
        Value::Null => buf.push(0x00),
        Value::Bool(b) => {
            buf.push(0x01);
            buf.push(*b as u8);
        }
        Value::Int(n) => {
            buf.push(0x02);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::BigInt(n) => {
            buf.push(0x03);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Real(f) => {
            buf.push(0x04);
            buf.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::Decimal(m, s) => {
            buf.push(0x05);
            buf.extend_from_slice(&m.to_le_bytes());
            buf.push(*s);
        }
        Value::Text(s) => {
            buf.push(0x06);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            buf.push(0x07);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Date(d) => {
            buf.push(0x08);
            buf.extend_from_slice(&d.to_le_bytes());
        }
        Value::Timestamp(t) => {
            buf.push(0x09);
            buf.extend_from_slice(&t.to_le_bytes());
        }
        Value::Uuid(u) => {
            buf.push(0x0A);
            buf.extend_from_slice(u.as_slice());
        }
    }
    buf
}

/// Serializes a GROUP BY key (multiple values) to a single byte sequence.
fn group_key_bytes(key_values: &[Value]) -> Vec<u8> {
    key_values.iter().flat_map(value_to_key_bytes).collect()
}

// ── HAVING evaluator ──────────────────────────────────────────────────────────

/// Evaluates a HAVING expression against a finalized group.
///
/// `Expr::Column` references are evaluated against `representative_row`
/// (the original source row, so `col_idx` values from the analyzer are valid).
///
/// `Expr::Function` aggregate calls are looked up in `agg_values` by name + arg.
///
/// All other expressions are evaluated by delegating sub-expression results to
/// the standard `eval()` via synthetic `Expr::Literal` nodes.
fn eval_with_aggs(
    expr: &Expr,
    representative_row: &[Value],
    agg_values: &[Value],
    agg_exprs: &[AggExpr],
) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),

        Expr::Column { col_idx, .. } => {
            representative_row
                .get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: representative_row.len(),
                })
        }

        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let idx = agg_exprs
                .iter()
                .position(|ae| ae.matches(name.as_str(), args))
                .ok_or_else(|| {
                    DbError::Other(format!(
                        "aggregate '{name}' not pre-registered — internal error"
                    ))
                })?;
            Ok(agg_values[idx].clone())
        }

        // AND: short-circuit
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            match l {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true) => {
                    eval_with_aggs(right, representative_row, agg_values, agg_exprs)
                }
                Value::Null => {
                    let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
                    Ok(if matches!(r, Value::Bool(false)) {
                        Value::Bool(false)
                    } else {
                        Value::Null
                    })
                }
                other => Err(DbError::TypeMismatch {
                    expected: "Bool".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // OR: short-circuit
        Expr::BinaryOp {
            op: BinaryOp::Or,
            left,
            right,
        } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            match l {
                Value::Bool(true) => Ok(Value::Bool(true)),
                Value::Bool(false) => {
                    eval_with_aggs(right, representative_row, agg_values, agg_exprs)
                }
                Value::Null => {
                    let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
                    Ok(if matches!(r, Value::Bool(true)) {
                        Value::Bool(true)
                    } else {
                        Value::Null
                    })
                }
                other => Err(DbError::TypeMismatch {
                    expected: "Bool".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // Other binary ops: evaluate both sides, delegate to eval() via Literal.
        Expr::BinaryOp { op, left, right } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::BinaryOp {
                    op: *op,
                    left: Box::new(Expr::Literal(l)),
                    right: Box::new(Expr::Literal(r)),
                },
                &[],
            )
        }

        Expr::UnaryOp { op, operand } => {
            let v = eval_with_aggs(operand, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::UnaryOp {
                    op: *op,
                    operand: Box::new(Expr::Literal(v)),
                },
                &[],
            )
        }

        Expr::IsNull { expr, negated } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::IsNull {
                    expr: Box::new(Expr::Literal(v)),
                    negated: *negated,
                },
                &[],
            )
        }

        // CASE WHEN in HAVING context — recurse through eval_with_aggs for sub-exprs.
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            match operand {
                None => {
                    for (when_expr, then_expr) in when_thens {
                        let cond =
                            eval_with_aggs(when_expr, representative_row, agg_values, agg_exprs)?;
                        if is_truthy(&cond) {
                            return eval_with_aggs(
                                then_expr,
                                representative_row,
                                agg_values,
                                agg_exprs,
                            );
                        }
                    }
                }
                Some(base_expr) => {
                    let base_val =
                        eval_with_aggs(base_expr, representative_row, agg_values, agg_exprs)?;
                    for (val_expr, then_expr) in when_thens {
                        let val =
                            eval_with_aggs(val_expr, representative_row, agg_values, agg_exprs)?;
                        let eq = eval(
                            &Expr::BinaryOp {
                                op: BinaryOp::Eq,
                                left: Box::new(Expr::Literal(base_val.clone())),
                                right: Box::new(Expr::Literal(val)),
                            },
                            &[],
                        )?;
                        if is_truthy(&eq) {
                            return eval_with_aggs(
                                then_expr,
                                representative_row,
                                agg_values,
                                agg_exprs,
                            );
                        }
                    }
                }
            }
            match else_result {
                Some(else_expr) => {
                    eval_with_aggs(else_expr, representative_row, agg_values, agg_exprs)
                }
                None => Ok(Value::Null),
            }
        }

        // For remaining variants: fall back to standard eval against representative_row.
        other => eval(other, representative_row),
    }
}

// ── execute_select_grouped ────────────────────────────────────────────────────

/// Executes the GROUP BY + aggregation path.
///
/// `combined_rows` are the post-scan, post-WHERE rows (not yet projected).
/// They are the "source" rows for the GROUP BY grouping.
fn execute_select_grouped(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
) -> Result<QueryResult, DbError> {
    // Build aggregate registry.
    let agg_exprs = collect_agg_exprs(&stmt.columns, &stmt.having);

    // One-pass hash aggregation.
    let mut groups: HashMap<Vec<u8>, GroupState> = HashMap::new();

    for row in &combined_rows {
        // Evaluate GROUP BY expressions → key values.
        let key_values: Vec<Value> = stmt
            .group_by
            .iter()
            .map(|e| eval(e, row))
            .collect::<Result<_, _>>()?;

        let key_bytes = group_key_bytes(&key_values);

        let state = groups.entry(key_bytes).or_insert_with(|| GroupState {
            key_values: key_values.clone(),
            representative_row: row.clone(),
            accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
        });

        // Update each accumulator.
        for (acc, agg) in state.accumulators.iter_mut().zip(&agg_exprs) {
            acc.update(row, agg)?;
        }
    }

    // Ungrouped aggregate: if no GROUP BY and no rows, still emit one output group.
    // (e.g., SELECT COUNT(*) FROM empty_table → returns (0), not 0 rows)
    if stmt.group_by.is_empty() && groups.is_empty() {
        groups.insert(
            vec![],
            GroupState {
                key_values: vec![],
                representative_row: vec![],
                accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
            },
        );
    }

    // Build output column metadata.
    let out_cols = build_grouped_column_meta(&stmt.columns, &agg_exprs)?;

    // Finalize, HAVING filter, project.
    let mut rows: Vec<Row> = Vec::new();
    for (_, state) in groups {
        // Finalize all accumulators.
        let agg_values: Vec<Value> = state
            .accumulators
            .into_iter()
            .map(|acc| acc.finalize())
            .collect::<Result<_, _>>()?;

        // HAVING filter.
        if let Some(ref having) = stmt.having {
            let v = eval_with_aggs(having, &state.representative_row, &agg_values, &agg_exprs)?;
            if !is_truthy(&v) {
                continue;
            }
        }

        // Project SELECT list.
        let out_row = project_grouped_row(
            &stmt.columns,
            &state.representative_row,
            &agg_values,
            &agg_exprs,
        )?;
        rows.push(out_row);
    }

    // DISTINCT deduplication (after projection, before ORDER BY and LIMIT).
    if stmt.distinct {
        rows = apply_distinct(rows);
    }

    // ORDER BY applied to projected output rows.
    // For GROUP BY queries, ORDER BY evaluates against the output row.
    rows = apply_order_by(rows, &stmt.order_by)?;

    // LIMIT/OFFSET.
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Projects one output row for the grouped path.
///
/// For each `SelectItem::Expr`:
/// - If the expression contains an aggregate → `eval_with_aggs`
/// - Otherwise → standard `eval` against `representative_row`
fn project_grouped_row(
    items: &[SelectItem],
    representative_row: &[Value],
    agg_values: &[Value],
    agg_exprs: &[AggExpr],
) -> Result<Row, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return Err(DbError::TypeMismatch {
                    expected: "column in GROUP BY or aggregate function".into(),
                    got: "SELECT * (wildcard) with GROUP BY".into(),
                });
            }
            SelectItem::Expr { expr, .. } => {
                let v = if contains_aggregate(expr) {
                    eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?
                } else {
                    eval(expr, representative_row)?
                };
                out.push(v);
            }
        }
    }
    Ok(out)
}

/// Builds `ColumnMeta` for the output of a grouped SELECT.
fn build_grouped_column_meta(
    items: &[SelectItem],
    agg_exprs: &[AggExpr],
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return Err(DbError::TypeMismatch {
                    expected: "column in GROUP BY or aggregate function".into(),
                    got: "SELECT * (wildcard) with GROUP BY".into(),
                });
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| grouped_expr_name(expr, agg_exprs));
                let (dt, nullable) = grouped_expr_type(expr, agg_exprs);
                out.push(ColumnMeta {
                    name,
                    data_type: dt,
                    nullable,
                    table_name: None,
                });
            }
        }
    }
    Ok(out)
}

/// Returns a display name for a grouped SELECT expression.
fn grouped_expr_name(expr: &Expr, _agg_exprs: &[AggExpr]) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            if args.is_empty() {
                format!("{name}(*)")
            } else {
                format!("{name}(...)")
            }
        }
        _ => "?column?".into(),
    }
}

/// Infers `(DataType, nullable)` for a grouped SELECT expression.
/// Aggregate results: COUNT → BigInt non-null; SUM/MIN/MAX/AVG → nullable.
fn grouped_expr_type(expr: &Expr, _agg_exprs: &[AggExpr]) -> (DataType, bool) {
    match expr {
        Expr::Function { name, .. } if is_aggregate(name.as_str()) => match name.as_str() {
            "count" => (DataType::BigInt, false),
            "avg" => (DataType::Real, true),
            _ => (DataType::Text, true), // SUM/MIN/MAX: type depends on column — Text fallback
        },
        Expr::Column { .. } => (DataType::Text, true), // Column refs: safe fallback
        _ => (DataType::Text, true),
    }
}

// ── INSERT ────────────────────────────────────────────────────────────────────

fn execute_insert(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let schema_cols = &resolved.columns;

    // Determine the mapping: schema_column_index → values_row_index (or MAX = Null).
    let col_positions: Vec<usize> = match &stmt.columns {
        None => (0..schema_cols.len()).collect(),
        Some(named_cols) => {
            let mut map = vec![usize::MAX; schema_cols.len()];
            for (val_pos, col_name) in named_cols.iter().enumerate() {
                let schema_pos = schema_cols
                    .iter()
                    .position(|c| &c.name == col_name)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: resolved.def.table_name.clone(),
                    })?;
                map[schema_pos] = val_pos;
            }
            map
        }
    };

    let mut count = 0u64;

    // Use the already-loaded indexes from the resolved table (cached by SchemaCache).
    // Avoids a second catalog heap scan per INSERT.
    let secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
        .cloned()
        .collect();

    // Find the AUTO_INCREMENT column index (at most one per table).
    let auto_inc_col: Option<usize> = schema_cols.iter().position(|c| c.auto_increment);

    // Track the first generated ID for LAST_INSERT_ID() semantics.
    let mut first_generated: Option<u64> = None;

    /// Returns the next value from the per-table AUTO_INCREMENT sequence,
    /// initializing it from MAX(col)+1 on first use (restart-safe).
    fn next_auto_inc(
        storage: &mut dyn StorageEngine,
        txn: &TxnManager,
        table_def: &axiomdb_catalog::schema::TableDef,
        schema_cols: &[axiomdb_catalog::schema::ColumnDef],
        col_idx: usize,
    ) -> Result<u64, DbError> {
        let table_id = table_def.id;
        // Check if already initialized.
        let cached = AUTO_INC_SEQ.with(|seq| seq.borrow().get(&table_id).copied());
        if let Some(next) = cached {
            AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
            return Ok(next);
        }
        // First use: scan the table to find MAX of the auto-increment column.
        let snap = txn.active_snapshot()?;
        let rows = TableEngine::scan_table(storage, table_def, schema_cols, snap, None)?;
        let max_existing: u64 = rows
            .iter()
            .filter_map(|(_, vals)| vals.get(col_idx))
            .filter_map(|v| match v {
                Value::Int(n) => Some(*n as u64),
                Value::BigInt(n) => Some(*n as u64),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let next = max_existing + 1;
        AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
        Ok(next)
    }

    match stmt.source {
        // ── INSERT ... VALUES ─────────────────────────────────────────────────
        InsertSource::Values(rows) => {
            // ── Phase 1: evaluate expressions + resolve AUTO_INCREMENT for all rows ──
            // This is done upfront so that:
            // (a) any expression error fails fast before touching the heap, and
            // (b) the batch path receives final Value vecs (no per-row eval inside batch).
            let mut full_batch: Vec<Vec<Value>> = Vec::with_capacity(rows.len());

            for value_exprs in &rows {
                let provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;

                let mut full_values: Vec<Value> = col_positions
                    .iter()
                    .map(|&idx| {
                        if idx == usize::MAX {
                            Value::Null
                        } else {
                            provided.get(idx).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();

                // AUTO_INCREMENT: assign the next ID before batching.
                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id = next_auto_inc(storage, txn, &resolved.def, schema_cols, ai_col)?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }

                full_batch.push(full_values);
            }

            // ── Phase 2: insert into the heap ─────────────────────────────────
            //
            // Single-row path: use insert_row() directly — no Vec allocation
            // overhead, same as before this optimization.
            //
            // Multi-row path (N > 1, no secondary indexes): use insert_rows_batch()
            // which loads each heap page once for the entire batch (vs. once per row).
            //
            // Multi-row path (N > 1, with secondary indexes): fall back to the
            // per-row loop so that secondary index maintenance has the Value vecs
            // available for each row. This maintains correctness at a minor
            // performance cost; optimizing secondary-index batch maintenance is
            // deferred to a follow-up.
            if full_batch.len() == 1 {
                // ── Single row — existing path, no overhead ────────────────────
                let full_values = full_batch.remove(0);
                let rid = TableEngine::insert_row(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    full_values.clone(),
                )?;
                if !secondary_indexes.is_empty() {
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                    }
                }
                count = 1;
            } else if secondary_indexes.is_empty() {
                // ── Multi-row batch, no secondary indexes — fast path ──────────
                // HeapChain::insert_batch() loads each page once, writes once.
                let n = full_batch.len() as u64;
                TableEngine::insert_rows_batch(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    &full_batch,
                )?;
                count = n;
            } else {
                // ── Multi-row with secondary indexes — per-row fallback ────────
                for full_values in full_batch {
                    let rid = TableEngine::insert_row(
                        storage,
                        txn,
                        &resolved.def,
                        schema_cols,
                        full_values.clone(),
                    )?;
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                    }
                    count += 1;
                }
            }
        }

        // ── INSERT ... SELECT ─────────────────────────────────────────────────
        InsertSource::Select(select_stmt) => {
            let select_rows = match execute_select(*select_stmt, storage, txn)? {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(DbError::Other(format!(
                        "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                    )))
                }
            };

            for row_values in select_rows {
                let mut full_values: Vec<Value> = col_positions
                    .iter()
                    .map(|&idx| {
                        if idx == usize::MAX {
                            Value::Null
                        } else {
                            row_values.get(idx).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();

                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id = next_auto_inc(storage, txn, &resolved.def, schema_cols, ai_col)?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }

                let rid = TableEngine::insert_row(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    full_values.clone(),
                )?;
                if !secondary_indexes.is_empty() {
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                    }
                }
                count += 1;
            }
        }

        InsertSource::DefaultValues => {
            return Err(DbError::NotImplemented {
                feature: "DEFAULT VALUES — Phase 4.3c".into(),
            })
        }
    }

    // Update the thread-local LAST_INSERT_ID if we generated any IDs.
    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        return Ok(QueryResult::affected_with_id(count, id));
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn execute_update(
    stmt: UpdateStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let schema_cols = resolved.columns.clone();

    // Resolve assignment column positions once, before the scan.
    let assignments: Vec<(usize, Expr)> = stmt
        .assignments
        .into_iter()
        .map(|a| {
            let pos = schema_cols
                .iter()
                .position(|c| c.name == a.column)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: a.column.clone(),
                    table: resolved.def.table_name.clone(),
                })?;
            Ok((pos, a.value))
        })
        .collect::<Result<_, DbError>>()?;

    let snap = txn.active_snapshot()?;
    let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?;

    // Use the already-loaded indexes from the resolved table (cached by SchemaCache).
    let mut secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
        .cloned()
        .collect();

    let mut count = 0u64;
    for (rid, current_values) in rows {
        // WHERE filter.
        if let Some(ref wc) = stmt.where_clause {
            if !is_truthy(&eval(wc, &current_values)?) {
                continue;
            }
        }
        // Apply SET assignments.
        let mut new_values = current_values.clone();
        for (col_pos, val_expr) in &assignments {
            new_values[*col_pos] = eval(val_expr, &current_values)?;
        }
        let new_rid = TableEngine::update_row(
            storage,
            txn,
            &resolved.def,
            &schema_cols,
            rid,
            new_values.clone(),
        )?;
        // Index maintenance: delete old key, insert new key.
        if !secondary_indexes.is_empty() {
            let del_updated = crate::index_maintenance::delete_from_indexes(
                &secondary_indexes,
                &current_values,
                storage,
            )?;
            for (index_id, new_root) in &del_updated {
                CatalogWriter::new(storage, txn)?.update_index_root(*index_id, *new_root)?;
            }
            // Update in-memory root_page_ids before insert so insert uses the
            // correct (post-delete) root page.
            for (index_id, new_root) in del_updated {
                if let Some(idx) = secondary_indexes
                    .iter_mut()
                    .find(|i| i.index_id == index_id)
                {
                    idx.root_page_id = new_root;
                }
            }
            let ins_updated = crate::index_maintenance::insert_into_indexes(
                &secondary_indexes,
                &new_values,
                new_rid,
                storage,
            )?;
            for (index_id, new_root) in ins_updated {
                CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
            }
        }
        count += 1;
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── DELETE ────────────────────────────────────────────────────────────────────

fn execute_delete(
    stmt: DeleteStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let snap = txn.active_snapshot()?;

    // Use the already-loaded indexes from the resolved table (cached by SchemaCache).
    let secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
        .cloned()
        .collect();

    // No-WHERE + no secondary indexes → single Truncate WAL entry (10,000× less WAL I/O).
    // Secondary indexes need per-row values for key extraction, so we fall through to the
    // slow path when any secondary index exists.
    if stmt.where_clause.is_none() && secondary_indexes.is_empty() {
        let raw_rids = HeapChain::scan_rids_visible(storage, resolved.def.data_root_page_id, snap)?;
        let count = raw_rids.len() as u64;
        HeapChain::delete_batch(
            storage,
            resolved.def.data_root_page_id,
            &raw_rids,
            snap.current_txn_id,
        )?;
        txn.record_truncate(resolved.def.id, resolved.def.data_root_page_id)?;
        return Ok(QueryResult::Affected {
            count,
            last_insert_id: None,
        });
    }

    let schema_cols = resolved.columns.clone();
    let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?;

    // Collect all matching (rid, row_values) BEFORE deleting any row.
    let to_delete: Vec<(RecordId, Vec<Value>)> = rows
        .into_iter()
        .filter_map(|(rid, values)| match &stmt.where_clause {
            None => Some(Ok((rid, values))),
            Some(wc) => match eval(wc, &values) {
                Ok(v) if is_truthy(&v) => Some(Ok((rid, values))),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            },
        })
        .collect::<Result<_, DbError>>()?;

    // Batch-delete from heap: each page read+written once instead of 3× per row.
    let rids_only: Vec<RecordId> = to_delete.iter().map(|(rid, _)| *rid).collect();
    let count = TableEngine::delete_rows_batch(storage, txn, &resolved.def, &rids_only)?;

    // Index maintenance: still per-row (each B+Tree remove is its own traversal),
    // but heap I/O is now fully batched above.
    if !secondary_indexes.is_empty() {
        for (_, row_vals) in &to_delete {
            let updated = crate::index_maintenance::delete_from_indexes(
                &secondary_indexes,
                row_vals,
                storage,
            )?;
            for (index_id, new_root) in updated {
                CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
            }
        }
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── CREATE TABLE ─────────────────────────────────────────────────────────────

fn execute_create_table(
    stmt: CreateTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");

    // Check existence before constructing CatalogWriter (avoids double mutable borrow).
    {
        let mut resolver = make_resolver(storage, txn)?;
        if resolver.table_exists(Some(schema), &stmt.table.name)? {
            if stmt.if_not_exists {
                return Ok(QueryResult::Empty);
            }
            return Err(DbError::TableAlreadyExists {
                schema: schema.to_string(),
                name: stmt.table.name.clone(),
            });
        }
    } // resolver dropped here — releases immutable borrow on storage

    let mut writer = CatalogWriter::new(storage, txn)?;
    let table_id = writer.create_table(schema, &stmt.table.name)?;

    for (i, col_def) in stmt.columns.iter().enumerate() {
        let col_type = datatype_to_column_type(&col_def.data_type)?;
        let nullable = !col_def
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::NotNull));
        let auto_increment = col_def
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::AutoIncrement));
        writer.create_column(CatalogColumnDef {
            table_id,
            col_idx: i as u16,
            name: col_def.name.clone(),
            col_type,
            nullable,
            auto_increment,
        })?;
    }

    Ok(QueryResult::Empty)
}

// ── DROP TABLE ────────────────────────────────────────────────────────────────

fn execute_drop_table(
    stmt: DropTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    for table_ref in stmt.tables {
        let schema = table_ref.schema.as_deref().unwrap_or("public");
        let snap = txn.active_snapshot()?;

        let table_id = {
            let mut reader = CatalogReader::new(storage, snap)?;
            match reader.get_table(schema, &table_ref.name)? {
                Some(def) => def.id,
                None if stmt.if_exists => continue,
                None => {
                    return Err(DbError::TableNotFound {
                        name: table_ref.name.clone(),
                    })
                }
            }
        }; // reader dropped — immutable borrow released

        CatalogWriter::new(storage, txn)?.delete_table(table_id)?;
    }

    Ok(QueryResult::Empty)
}

// ── CREATE INDEX ──────────────────────────────────────────────────────────────

fn execute_create_index(
    stmt: CreateIndexStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    use crate::key_encoding::{encode_index_key, MAX_INDEX_KEY};
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
    use std::sync::atomic::{AtomicU64, Ordering};

    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot()?;

    // 1. Resolve table definition + column list.
    let (table_def, col_defs) = {
        let mut resolver = make_resolver(storage, txn)?;
        let resolved = resolver.resolve_table(Some(schema), &stmt.table.name)?;
        (resolved.def.clone(), resolved.columns.clone())
    };

    // 2. Check for a duplicate index name on this table.
    {
        let mut reader = CatalogReader::new(storage, snap)?;
        let existing = reader.list_indexes(table_def.id)?;
        if existing.iter().any(|i| i.name == stmt.name) {
            return Err(DbError::IndexAlreadyExists {
                name: stmt.name.clone(),
                table: stmt.table.name.clone(),
            });
        }
    }

    // 3. Build IndexColumnDef list from the CREATE INDEX statement.
    let index_columns: Vec<IndexColumnDef> = stmt
        .columns
        .iter()
        .map(|ic| {
            let col = col_defs
                .iter()
                .find(|c| c.name == ic.name)
                .expect("analyzer guarantees index columns exist in the table");
            IndexColumnDef {
                col_idx: col.col_idx,
                order: match ic.order {
                    crate::ast::SortOrder::Asc => CatalogSortOrder::Asc,
                    crate::ast::SortOrder::Desc => CatalogSortOrder::Desc,
                },
            }
        })
        .collect();

    // 4. Allocate and initialize a fresh B-Tree leaf root page.
    let root_page_id = storage.alloc_page(PageType::Index)?;
    {
        let mut page = Page::new(PageType::Index, root_page_id);
        let leaf = cast_leaf_mut(&mut page);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        leaf.set_next_leaf(NULL_PAGE);
        page.update_checksum();
        storage.write_page(root_page_id, &page)?;
    }
    let root_pid = AtomicU64::new(root_page_id);

    // 5. Scan the table and insert existing rows into the B-Tree.
    let rows = TableEngine::scan_table(storage, &table_def, &col_defs, snap, None)?;
    let mut skipped = 0usize;
    for (rid, row_vals) in rows {
        let key_vals: Vec<Value> = index_columns
            .iter()
            .map(|ic| row_vals[ic.col_idx as usize].clone())
            .collect();
        // Skip rows with NULL key values — NULLs are not indexed.
        if key_vals.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        match encode_index_key(&key_vals) {
            Ok(key) => {
                BTree::insert_in(storage, &root_pid, &key, rid)?;
            }
            Err(DbError::IndexKeyTooLong { .. }) => {
                skipped += 1;
            }
            Err(e) => return Err(e),
        }
    }
    if skipped > 0 {
        eprintln!(
            "CREATE INDEX \"{}\": skipped {skipped} row(s) with index key > {MAX_INDEX_KEY} bytes",
            stmt.name
        );
    }

    // 6. Persist IndexDef with column list and final root_page_id (may have changed after splits).
    let final_root = root_pid.load(Ordering::Acquire);
    let mut writer = CatalogWriter::new(storage, txn)?;
    writer.create_index(IndexDef {
        index_id: 0, // allocated by CatalogWriter::create_index
        table_id: table_def.id,
        name: stmt.name.clone(),
        root_page_id: final_root,
        is_unique: stmt.unique,
        is_primary: false,
        columns: index_columns,
    })?;

    Ok(QueryResult::Empty)
}

// ── DROP INDEX ────────────────────────────────────────────────────────────────

fn execute_drop_index(
    stmt: DropIndexStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot()?;

    // MySQL requires `DROP INDEX name ON table`. If no table is provided, we cannot
    // efficiently search all indexes for Phase 4.5.
    let table_ref = stmt.table.as_ref().ok_or_else(|| DbError::NotImplemented {
        feature: "DROP INDEX without ON table — Phase 4.20".into(),
    })?;

    let schema = table_ref.schema.as_deref().unwrap_or("public");

    // Capture both index_id and root_page_id for catalog deletion + B-Tree page reclamation.
    let (index_id, root_page_id) = {
        let mut reader = CatalogReader::new(storage, snap)?;
        let table_def = match reader.get_table(schema, &table_ref.name)? {
            Some(d) => d,
            None if stmt.if_exists => return Ok(QueryResult::Empty),
            None => {
                return Err(DbError::TableNotFound {
                    name: table_ref.name.clone(),
                })
            }
        };
        let indexes = reader.list_indexes(table_def.id)?;
        match indexes.into_iter().find(|i| i.name == stmt.name) {
            Some(i) => (Some(i.index_id), Some(i.root_page_id)),
            None => (None, None),
        }
    }; // reader dropped

    match index_id {
        None if stmt.if_exists => Ok(QueryResult::Empty),
        None => Err(DbError::NotImplemented {
            feature: format!("DROP INDEX — index '{}' not found", stmt.name),
        }),
        Some(id) => {
            // Delete catalog entry first.
            CatalogWriter::new(storage, txn)?.delete_index(id)?;
            // Then free all B-Tree pages to avoid leaks.
            if let Some(root) = root_page_id {
                free_btree_pages(storage, root)?;
            }
            Ok(QueryResult::Empty)
        }
    }
}

/// Frees all pages of a B-Tree rooted at `root_pid`.
///
/// Iteratively walks the tree (BFS via a stack) and calls `free_page` on each
/// node — both internal and leaf pages.
fn free_btree_pages(storage: &mut dyn StorageEngine, root_pid: u64) -> Result<(), DbError> {
    use axiomdb_index::page_layout::{cast_internal, cast_leaf};

    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        let page = storage.read_page(pid)?;
        if page.body()[0] != 1 {
            // Internal node — push all children before freeing.
            let node = cast_internal(page);
            let n = node.num_keys();
            for i in 0..=n {
                stack.push(node.child_at(i));
            }
        } else {
            // Leaf node — no children to push.
            let _leaf = cast_leaf(page); // just validate it reads correctly
        }
        storage.free_page(pid)?;
    }
    Ok(())
}

// ── TRUNCATE TABLE (4.21) ─────────────────────────────────────────────────────

fn execute_truncate(
    stmt: crate::ast::TruncateTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    // TRUNCATE has no WHERE predicate — use the no-WAL-per-row fast path.
    // delete_batch() marks slots dead; record_truncate() writes ONE WAL entry.
    let snap = txn.active_snapshot()?;
    let raw_rids = HeapChain::scan_rids_visible(storage, resolved.def.data_root_page_id, snap)?;
    HeapChain::delete_batch(
        storage,
        resolved.def.data_root_page_id,
        &raw_rids,
        snap.current_txn_id,
    )?;
    txn.record_truncate(resolved.def.id, resolved.def.data_root_page_id)?;

    // Reset the AUTO_INCREMENT sequence so the next insert starts from 1.
    AUTO_INC_SEQ.with(|seq| {
        seq.borrow_mut().remove(&resolved.def.id);
    });

    // MySQL convention: TRUNCATE returns count = 0, not the actual deleted count.
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

// ── SHOW TABLES / SHOW COLUMNS / DESCRIBE (4.20) ─────────────────────────────

fn execute_show_tables(
    stmt: crate::ast::ShowTablesStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let schema = stmt.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;
    let tables = reader.list_tables(schema)?;

    let col_name = format!("Tables_in_{schema}");
    let out_cols = vec![ColumnMeta::computed(col_name, DataType::Text)];
    let rows: Vec<Row> = tables
        .into_iter()
        .map(|t| vec![Value::Text(t.table_name)])
        .collect();

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

fn execute_show_columns(
    stmt: crate::ast::ShowColumnsStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;

    let table_def =
        reader
            .get_table(schema, &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
            })?;
    let columns = reader.list_columns(table_def.id)?;

    let out_cols = vec![
        ColumnMeta::computed("Field", DataType::Text),
        ColumnMeta::computed("Type", DataType::Text),
        ColumnMeta::computed("Null", DataType::Text),
        ColumnMeta::computed("Key", DataType::Text),
        ColumnMeta::computed("Default", DataType::Text),
        ColumnMeta::computed("Extra", DataType::Text),
    ];

    let rows: Vec<Row> = columns
        .iter()
        .map(|c| {
            let type_str = column_type_to_sql_name(c.col_type);
            let null_str = if c.nullable { "YES" } else { "NO" };
            let extra = if c.auto_increment {
                "auto_increment"
            } else {
                ""
            };
            vec![
                Value::Text(c.name.clone()),
                Value::Text(type_str.into()),
                Value::Text(null_str.into()),
                Value::Text("".into()), // Key — deferred
                Value::Null,            // Default — deferred
                Value::Text(extra.into()),
            ]
        })
        .collect();

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Returns the SQL type name string for display in SHOW COLUMNS / DESCRIBE.
fn column_type_to_sql_name(ct: ColumnType) -> &'static str {
    match ct {
        ColumnType::Bool => "BOOL",
        ColumnType::Int => "INT",
        ColumnType::BigInt => "BIGINT",
        ColumnType::Float => "REAL",
        ColumnType::Text => "TEXT",
        ColumnType::Bytes => "BYTES",
        ColumnType::Timestamp => "TIMESTAMP",
        ColumnType::Uuid => "UUID",
    }
}

// ── ALTER TABLE (4.22) ────────────────────────────────────────────────────────

/// Rewrites all rows in `table_def` by applying `transform` to each row.
///
/// The row is decoded using `old_columns`, transformed, then encoded and
/// reinserted using `new_columns`. Used by ADD COLUMN and DROP COLUMN.
///
/// **Ordering for ADD COLUMN**: call this AFTER updating the catalog so that
/// the new rows match the new schema.
/// **Ordering for DROP COLUMN**: call this BEFORE updating the catalog so that
/// if the rewrite fails the catalog is still consistent with the existing rows.
fn rewrite_rows(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    old_columns: &[axiomdb_catalog::schema::ColumnDef],
    new_columns: &[axiomdb_catalog::schema::ColumnDef],
    transform: &dyn Fn(Row) -> Row,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot()?;
    let rows = TableEngine::scan_table(storage, table_def, old_columns, snap, None)?;
    for (rid, old_values) in rows {
        let new_values = transform(old_values);
        TableEngine::delete_row(storage, txn, table_def, rid)?;
        TableEngine::insert_row(storage, txn, table_def, new_columns, new_values)?;
    }
    Ok(())
}

fn execute_alter_table(
    stmt: AlterTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");

    // Resolve the table once upfront.
    let table_def = {
        let mut resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };
    // Keep the current column list; update it as we apply operations.
    let mut columns = table_def.columns.clone();

    for op in stmt.operations {
        match op {
            AlterTableOp::AddColumn(col_def) => {
                alter_add_column(storage, txn, &table_def.def, &mut columns, col_def, schema)?;
            }
            AlterTableOp::DropColumn { name, if_exists } => {
                alter_drop_column(storage, txn, &table_def.def, &mut columns, &name, if_exists)?;
            }
            AlterTableOp::RenameColumn { old_name, new_name } => {
                alter_rename_column(
                    storage,
                    txn,
                    &table_def.def,
                    &columns,
                    &old_name,
                    &new_name,
                    schema,
                )?;
                // Refresh: catalog was updated, re-read column list.
                let snap2 = txn.active_snapshot()?;
                columns = CatalogReader::new(storage, snap2)?.list_columns(table_def.def.id)?;
            }
            AlterTableOp::RenameTable(new_name) => {
                alter_rename_table(storage, txn, &table_def.def, &new_name, schema)?;
                // After RENAME TABLE further operations would need the new table_def;
                // for simplicity, only one op per statement is expected for RENAME TO.
                break;
            }
            _ => {
                return Err(DbError::NotImplemented {
                    feature: "ALTER TABLE MODIFY COLUMN / ADD CONSTRAINT — Phase N".into(),
                })
            }
        }
    }

    Ok(QueryResult::Empty)
}

fn alter_add_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    columns: &mut Vec<axiomdb_catalog::schema::ColumnDef>,
    col_def: crate::ast::ColumnDef,
    schema: &str,
) -> Result<(), DbError> {
    // Check for duplicate column name.
    let table_name = &table_def.table_name;
    if columns.iter().any(|c| c.name == col_def.name) {
        return Err(DbError::ColumnAlreadyExists {
            name: col_def.name.clone(),
            table: table_name.clone(),
        });
    }

    // Evaluate DEFAULT expression (or NULL if no default).
    let default_value = col_def
        .constraints
        .iter()
        .find_map(|c| match c {
            crate::ast::ColumnConstraint::Default(expr) => {
                Some(eval(expr, &[]).unwrap_or(Value::Null))
            }
            _ => None,
        })
        .unwrap_or(Value::Null);

    let col_type = datatype_to_column_type(&col_def.data_type)?;
    let nullable = !col_def
        .constraints
        .iter()
        .any(|c| matches!(c, crate::ast::ColumnConstraint::NotNull));
    let auto_increment = col_def
        .constraints
        .iter()
        .any(|c| matches!(c, crate::ast::ColumnConstraint::AutoIncrement));

    let new_col_idx = columns
        .iter()
        .map(|c| c.col_idx)
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);

    let new_catalog_col = CatalogColumnDef {
        table_id: table_def.id,
        col_idx: new_col_idx,
        name: col_def.name.clone(),
        col_type,
        nullable,
        auto_increment,
    };

    // 1. Add column to catalog.
    CatalogWriter::new(storage, txn)?.create_column(new_catalog_col.clone())?;

    // 2. Rewrite rows (AFTER catalog update — new rows must include the new column).
    let old_columns = columns.clone();
    let mut new_columns = columns.clone();
    new_columns.push(new_catalog_col.clone());

    let dv = default_value;
    rewrite_rows(
        storage,
        txn,
        table_def,
        &old_columns,
        &new_columns,
        &|mut row| {
            row.push(dv.clone());
            row
        },
    )?;

    columns.push(new_catalog_col);
    let _ = schema; // schema already encoded in table_def
    Ok(())
}

fn alter_drop_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    columns: &mut Vec<axiomdb_catalog::schema::ColumnDef>,
    name: &str,
    if_exists: bool,
) -> Result<(), DbError> {
    // Find the column by name.
    let drop_pos = match columns.iter().position(|c| c.name == name) {
        Some(pos) => pos,
        None if if_exists => return Ok(()),
        None => {
            return Err(DbError::ColumnNotFound {
                name: name.to_string(),
                table: table_def.table_name.clone(),
            })
        }
    };

    let dropped_col_idx = columns[drop_pos].col_idx;
    let old_columns = columns.clone();

    // Build new column list (without the dropped column).
    let mut new_columns = columns.clone();
    new_columns.remove(drop_pos);

    // 1. Rewrite rows BEFORE updating catalog (if rewrite fails, catalog is still consistent).
    rewrite_rows(
        storage,
        txn,
        table_def,
        &old_columns,
        &new_columns,
        &move |mut row| {
            if drop_pos < row.len() {
                row.remove(drop_pos);
            }
            row
        },
    )?;

    // 2. Delete column from catalog.
    CatalogWriter::new(storage, txn)?.delete_column(table_def.id, dropped_col_idx)?;

    *columns = new_columns;
    Ok(())
}

fn alter_rename_column(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    columns: &[axiomdb_catalog::schema::ColumnDef],
    old_name: &str,
    new_name: &str,
    _schema: &str,
) -> Result<(), DbError> {
    // Find old column.
    let col =
        columns
            .iter()
            .find(|c| c.name == old_name)
            .ok_or_else(|| DbError::ColumnNotFound {
                name: old_name.to_string(),
                table: table_def.table_name.clone(),
            })?;

    // Check new name is not already in use.
    if columns.iter().any(|c| c.name == new_name) {
        return Err(DbError::ColumnAlreadyExists {
            name: new_name.to_string(),
            table: table_def.table_name.clone(),
        });
    }

    CatalogWriter::new(storage, txn)?.rename_column(
        table_def.id,
        col.col_idx,
        new_name.to_string(),
    )?;
    Ok(())
}

fn alter_rename_table(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::schema::TableDef,
    new_name: &str,
    schema: &str,
) -> Result<(), DbError> {
    // Check new name not already in use.
    let snap = txn.active_snapshot()?;
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.get_table(schema, new_name)?.is_some() {
        return Err(DbError::TableAlreadyExists {
            schema: schema.to_string(),
            name: new_name.to_string(),
        });
    }

    CatalogWriter::new(storage, txn)?.rename_table(table_def.id, new_name.to_string(), schema)?;
    Ok(())
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Creates a [`SchemaResolver`] using the current snapshot.
///
/// Uses `active_snapshot()` when a transaction is active, falling back to
/// `snapshot()` for read-only access outside a transaction.
fn make_resolver<'a>(
    storage: &'a mut dyn StorageEngine,
    txn: &TxnManager,
) -> Result<SchemaResolver<'a>, DbError> {
    let snap: TransactionSnapshot = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    SchemaResolver::new(storage, snap, "public")
}

/// Converts a SQL [`DataType`] (from the AST) to the compact [`ColumnType`] stored
/// in the catalog. Returns [`DbError::NotImplemented`] for types not yet in the catalog.
fn datatype_to_column_type(dt: &DataType) -> Result<ColumnType, DbError> {
    match dt {
        DataType::Bool => Ok(ColumnType::Bool),
        DataType::Int => Ok(ColumnType::Int),
        DataType::BigInt => Ok(ColumnType::BigInt),
        DataType::Real => Ok(ColumnType::Float),
        DataType::Text => Ok(ColumnType::Text),
        DataType::Bytes => Ok(ColumnType::Bytes),
        DataType::Timestamp => Ok(ColumnType::Timestamp),
        DataType::Uuid => Ok(ColumnType::Uuid),
        DataType::Decimal => Err(DbError::NotImplemented {
            feature: "DECIMAL column type — Phase 4.3".into(),
        }),
        DataType::Date => Err(DbError::NotImplemented {
            feature: "DATE column type — Phase 4.19".into(),
        }),
    }
}

/// Converts a compact catalog [`ColumnType`] back to the full [`DataType`].
fn column_type_to_datatype(ct: ColumnType) -> DataType {
    match ct {
        ColumnType::Bool => DataType::Bool,
        ColumnType::Int => DataType::Int,
        ColumnType::BigInt => DataType::BigInt,
        ColumnType::Float => DataType::Real,
        ColumnType::Text => DataType::Text,
        ColumnType::Bytes => DataType::Bytes,
        ColumnType::Timestamp => DataType::Timestamp,
        ColumnType::Uuid => DataType::Uuid,
    }
}

/// Returns the [`DataType`] that best describes a runtime [`Value`].
/// Used for computing `ColumnMeta.data_type` for computed SELECT expressions.
fn datatype_of_value(v: &Value) -> DataType {
    match v {
        Value::Null => DataType::Text, // unknown type — use Text as fallback
        Value::Bool(_) => DataType::Bool,
        Value::Int(_) => DataType::Int,
        Value::BigInt(_) => DataType::BigInt,
        Value::Real(_) => DataType::Real,
        Value::Decimal(..) => DataType::Decimal,
        Value::Text(_) => DataType::Text,
        Value::Bytes(_) => DataType::Bytes,
        Value::Date(_) => DataType::Date,
        Value::Timestamp(_) => DataType::Timestamp,
        Value::Uuid(_) => DataType::Uuid,
    }
}

/// Infers the `(DataType, nullable)` pair for a SELECT expression.
///
/// For plain column references, uses the catalog type. For all other expressions,
/// returns `(DataType::Text, true)` as a safe fallback (proper type inference is Phase 6).
fn infer_expr_type(expr: &Expr, columns: &[CatalogColumnDef]) -> (DataType, bool) {
    match expr {
        Expr::Column { col_idx, .. } => {
            if let Some(col) = columns.get(*col_idx) {
                (column_type_to_datatype(col.col_type), col.nullable)
            } else {
                (DataType::Text, true)
            }
        }
        _ => (DataType::Text, true),
    }
}

/// Returns the output name for a SELECT expression item.
fn expr_column_name(expr: &Expr, alias: Option<&str>) -> String {
    if let Some(a) = alias {
        return a.to_string();
    }
    match expr {
        Expr::Column { name, .. } => name.clone(),
        _ => "?column?".to_string(),
    }
}

/// Builds the [`ColumnMeta`] vector for the output of a SELECT statement.
fn build_select_column_meta(
    items: &[SelectItem],
    columns: &[CatalogColumnDef],
    table_def: &TableDef,
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                for col in columns {
                    out.push(ColumnMeta {
                        name: col.name.clone(),
                        data_type: column_type_to_datatype(col.col_type),
                        nullable: col.nullable,
                        table_name: Some(table_def.table_name.clone()),
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = expr_column_name(expr, alias.as_deref());
                let (dt, nullable) = infer_expr_type(expr, columns);
                out.push(ColumnMeta {
                    name,
                    data_type: dt,
                    nullable,
                    table_name: None,
                });
            }
        }
    }
    Ok(out)
}

/// Projects a row through a SELECT item list (no subquery support).
fn project_row(items: &[SelectItem], values: &[Value]) -> Result<Row, DbError> {
    project_row_with(items, values, &mut crate::eval::NoSubquery)
}

/// Subquery-aware version of [`project_row`].
///
/// Uses `eval_with` so that scalar subqueries in the SELECT list
/// (e.g., `(SELECT COUNT(*) FROM orders WHERE user_id = u.id)`) are executed
/// via `sq`. Performance identical to `project_row` when using [`NoSubquery`]
/// due to monomorphization.
fn project_row_with<R: SubqueryRunner>(
    items: &[SelectItem],
    values: &[Value],
    sq: &mut R,
) -> Result<Row, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                out.extend_from_slice(values);
            }
            SelectItem::Expr { expr, .. } => {
                out.push(eval_with(expr, values, sq)?);
            }
        }
    }
    Ok(out)
}

/// Builds output column metadata for a SELECT over a derived table
/// (`FROM (SELECT ...) AS alias`).
///
/// `SELECT *` expands to the derived table's own column metadata.
/// `SELECT expr [AS alias]` uses the alias or the expression name.
fn build_derived_output_columns(
    items: &[SelectItem],
    derived_cols: &[ColumnMeta],
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                out.extend_from_slice(derived_cols);
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| expr_column_name(expr, None));
                out.push(ColumnMeta::computed(name, axiomdb_types::DataType::Text));
            }
        }
    }
    Ok(out)
}

// ── ORDER BY / LIMIT helpers ──────────────────────────────────────────────────

/// Compares two values for ORDER BY sorting, correctly handling NULLs.
///
/// ## NULL ordering defaults (PostgreSQL-compatible)
/// - `ASC` with no explicit NULLS → NULLs sort **last** (after non-NULLs)
/// - `DESC` with no explicit NULLS → NULLs sort **first** (before non-NULLs)
///
/// Explicit `NULLS FIRST` or `NULLS LAST` overrides the default.
fn compare_sort_values(
    a: &Value,
    b: &Value,
    direction: SortOrder,
    nulls: Option<NullsOrder>,
) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;

    let nulls_first = match (direction, nulls) {
        (_, Some(NullsOrder::First)) => true,
        (_, Some(NullsOrder::Last)) => false,
        (SortOrder::Asc, None) => false, // default: NULLS LAST for ASC
        (SortOrder::Desc, None) => true, // default: NULLS FIRST for DESC
    };

    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => {
            if nulls_first {
                Less
            } else {
                Greater
            }
        }
        (_, Value::Null) => {
            if nulls_first {
                Greater
            } else {
                Less
            }
        }
        (a, b) => {
            let ord = compare_non_null_for_sort(a, b);
            if direction == SortOrder::Desc {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// Compares two non-NULL values using the expression evaluator.
///
/// Delegates to `eval()` via synthetic `Expr::BinaryOp { Lt }` and `Eq`
/// expressions to reuse all existing type coercion and comparison logic.
/// Returns `Equal` if the comparison fails (type mismatch in ORDER BY).
fn compare_non_null_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;

    let lt = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    );
    let eq = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    );
    match (lt, eq) {
        (Ok(lt_v), Ok(eq_v)) => {
            if is_truthy(&lt_v) {
                Less
            } else if is_truthy(&eq_v) {
                Equal
            } else {
                Greater
            }
        }
        // Type mismatch or error: treat as equal (stable, no crash).
        _ => Equal,
    }
}

/// Compares two rows using all ORDER BY items (multi-column composite key).
///
/// Items are applied left-to-right; the first non-Equal result determines
/// the order. Returns `Equal` only when all items produce equal keys.
fn compare_rows_for_sort(
    a: &[Value],
    b: &[Value],
    order_items: &[OrderByItem],
) -> Result<std::cmp::Ordering, DbError> {
    for item in order_items {
        let key_a = eval(&item.expr, a)?;
        let key_b = eval(&item.expr, b)?;
        let ord = compare_sort_values(&key_a, &key_b, item.order, item.nulls);
        if ord != std::cmp::Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(std::cmp::Ordering::Equal)
}

/// Sorts `rows` in place according to `order_items`.
///
/// Uses `sort_by` (stable) to preserve insertion order for equal keys.
/// Errors from expression evaluation are captured via `sort_err` and
/// returned after the sort completes — `sort_by` cannot return `Result`.
fn apply_order_by(mut rows: Vec<Row>, order_items: &[OrderByItem]) -> Result<Vec<Row>, DbError> {
    if order_items.is_empty() {
        return Ok(rows);
    }
    let mut sort_err: Option<DbError> = None;
    rows.sort_by(|a, b| {
        if sort_err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match compare_rows_for_sort(a, b, order_items) {
            Ok(ord) => ord,
            Err(e) => {
                sort_err = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(e) = sort_err {
        return Err(e);
    }
    Ok(rows)
}

/// Evaluates a LIMIT or OFFSET expression as a non-negative integer.
///
/// # Errors
/// - [`DbError::TypeMismatch`] if the value is negative or not an integer.
fn eval_as_usize(expr: &Expr) -> Result<usize, DbError> {
    match eval(expr, &[])? {
        Value::Int(n) if n >= 0 => Ok(n as usize),
        Value::BigInt(n) if n >= 0 => Ok(n as usize),
        Value::Int(_) | Value::BigInt(_) => Err(DbError::TypeMismatch {
            expected: "non-negative integer for LIMIT/OFFSET".into(),
            got: "negative integer".into(),
        }),
        other => Err(DbError::TypeMismatch {
            expected: "integer for LIMIT/OFFSET".into(),
            got: other.variant_name().into(),
        }),
    }
}

/// Applies LIMIT and OFFSET to a row vector.
///
/// `skip(offset).take(limit)` — LIMIT is applied after ORDER BY and after
/// OFFSET. Passing `limit = None` returns all remaining rows.
fn apply_limit_offset(
    rows: Vec<Row>,
    limit: &Option<Expr>,
    offset: &Option<Expr>,
) -> Result<Vec<Row>, DbError> {
    let offset_n = offset.as_ref().map(eval_as_usize).transpose()?.unwrap_or(0);
    let limit_n = limit.as_ref().map(eval_as_usize).transpose()?;
    Ok(rows
        .into_iter()
        .skip(offset_n)
        .take(limit_n.unwrap_or(usize::MAX))
        .collect())
}

/// Deduplicates output rows, keeping the first occurrence of each unique row.
///
/// Two rows are equal if every column value serializes to the same bytes via
/// `value_to_key_bytes`. `NULL` values produce `[0x00]` — two NULLs in the
/// same column position are considered equal, consistent with SQL DISTINCT
/// semantics (unlike `NULL = NULL` which is UNKNOWN in comparisons).
///
/// Preserves the insertion order of first occurrences (stable deduplication).
fn apply_distinct(rows: Vec<Row>) -> Vec<Row> {
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|row| {
            let key: Vec<u8> = row.iter().flat_map(value_to_key_bytes).collect();
            seen.insert(key)
        })
        .collect()
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_datatype_to_column_type_supported() {
        assert_eq!(
            datatype_to_column_type(&DataType::Bool).unwrap(),
            ColumnType::Bool
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Int).unwrap(),
            ColumnType::Int
        );
        assert_eq!(
            datatype_to_column_type(&DataType::BigInt).unwrap(),
            ColumnType::BigInt
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Real).unwrap(),
            ColumnType::Float
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Text).unwrap(),
            ColumnType::Text
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Bytes).unwrap(),
            ColumnType::Bytes
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Timestamp).unwrap(),
            ColumnType::Timestamp
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Uuid).unwrap(),
            ColumnType::Uuid
        );
    }

    #[test]
    fn test_datatype_to_column_type_unsupported() {
        assert!(matches!(
            datatype_to_column_type(&DataType::Decimal),
            Err(DbError::NotImplemented { .. })
        ));
        assert!(matches!(
            datatype_to_column_type(&DataType::Date),
            Err(DbError::NotImplemented { .. })
        ));
    }

    #[test]
    fn test_column_type_to_datatype_roundtrip() {
        for &dt in &[
            DataType::Bool,
            DataType::Int,
            DataType::BigInt,
            DataType::Real,
            DataType::Text,
            DataType::Bytes,
            DataType::Timestamp,
            DataType::Uuid,
        ] {
            let ct = datatype_to_column_type(&dt).unwrap();
            assert_eq!(column_type_to_datatype(ct), dt);
        }
    }

    #[test]
    fn test_expr_column_name_alias_wins() {
        let expr = Expr::Literal(Value::Int(1));
        assert_eq!(expr_column_name(&expr, Some("total")), "total");
    }

    #[test]
    fn test_expr_column_name_column_expr() {
        let expr = Expr::Column {
            name: "age".into(),
            col_idx: 0,
        };
        assert_eq!(expr_column_name(&expr, None), "age");
    }

    #[test]
    fn test_expr_column_name_other_expr_fallback() {
        let expr = Expr::Literal(Value::Int(1));
        assert_eq!(expr_column_name(&expr, None), "?column?");
    }
}
