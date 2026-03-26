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
use axiomdb_index::{page_layout::encode_rid, BTree};
use axiomdb_storage::{
    heap_chain::{chain_next_page, HeapChain},
    Page, PageType, StorageEngine,
};
use axiomdb_types::{DataType, Value};
use axiomdb_wal::{Savepoint, TxnManager};

use crate::{
    ast::{
        AlterTableOp, AlterTableStmt, ColumnConstraint, CreateIndexStmt, CreateTableStmt,
        DeleteStmt, DropIndexStmt, DropTableStmt, FromClause, InsertSource, InsertStmt, JoinClause,
        JoinCondition, JoinType, NullsOrder, OrderByItem, SelectItem, SelectStmt, SetStmt,
        SetValue, SortOrder, Stmt, UpdateStmt,
    },
    eval::{eval, eval_with, is_truthy, CollationGuard, SubqueryRunner},
    expr::{BinaryOp, Expr},
    result::{ColumnMeta, QueryResult, Row},
    session::{
        normalize_sql_mode, parse_boolish_setting, parse_compat_mode_setting,
        parse_on_error_setting, parse_session_collation_setting, sql_mode_is_strict, OnErrorMode,
        SessionCollation, SessionContext,
    },
    table::TableEngine,
    text_semantics::compare_text,
};

/// Inline FK spec collected during CREATE TABLE column processing.
/// `(child_col_idx, child_col_name, (parent_table, parent_col, on_delete, on_update))`
type InlineFkSpec = (
    u16,
    String,
    (
        String,
        Option<String>,
        crate::ast::ForeignKeyAction,
        crate::ast::ForeignKeyAction,
    ),
);

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
        // GroupConcat: substitute outer column refs inside the concat expr and ORDER BY.
        Expr::GroupConcat {
            expr,
            distinct,
            order_by,
            separator,
        } => Expr::GroupConcat {
            expr: Box::new(subst_expr(*expr, outer_row)),
            distinct,
            order_by: order_by
                .into_iter()
                .map(|(e, dir)| (subst_expr(e, outer_row), dir))
                .collect(),
            separator,
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
    bloom: &'a mut crate::bloom::BloomRegistry,
    ctx: &'a mut SessionContext,
    outer_row: &'a [Value],
}

impl<'a> SubqueryRunner for ExecSubqueryRunner<'a> {
    fn run(&mut self, stmt: &SelectStmt) -> Result<QueryResult, DbError> {
        let bound = substitute_outer(stmt.clone(), self.outer_row);
        execute_select_ctx(bound, self.storage, self.txn, self.bloom, self.ctx)
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
                        let tid = txn.active_txn_id();
                        txn.commit()?;
                        if let Some(t) = tid {
                            txn.release_immediate_committed_frees(storage, t)?;
                        }
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
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // ── Explicit transaction active ────────────────────────────────────────────
    // Statement runs inside the caller's transaction.
    // Wrap in a savepoint so that a statement error only undoes that statement's
    // writes — the transaction remains open (MySQL statement-level rollback).
    if txn.active_txn_id().is_some() {
        // TCL inside an active transaction
        match &stmt {
            Stmt::Commit => return txn.commit().map(|_| QueryResult::Empty),
            Stmt::Rollback => return txn.rollback(storage).map(|_| QueryResult::Empty),
            Stmt::Begin => {
                let txn_id = txn.active_txn_id().unwrap_or(0);
                return Err(DbError::TransactionAlreadyActive { txn_id });
            }
            _ => {}
        }
        // DDL inside an open transaction: implicit COMMIT of the current transaction
        // first (MySQL semantics), then DDL runs in its own autocommit transaction.
        if is_ddl(&stmt) {
            let pre_tid = txn.active_txn_id();
            txn.commit()?;
            if let Some(t) = pre_tid {
                txn.release_immediate_committed_frees(storage, t)?;
            }
            txn.begin()?;
            return match dispatch_ctx(stmt, storage, txn, bloom, ctx) {
                Ok(result) => {
                    let ddl_tid = txn.active_txn_id();
                    txn.commit()?;
                    if let Some(t) = ddl_tid {
                        txn.release_immediate_committed_frees(storage, t)?;
                    }
                    Ok(result)
                }
                Err(e) => {
                    let _ = txn.rollback(storage);
                    Err(e)
                }
            };
        }
        // Apply on_error policy for DML inside an active transaction.
        // RollbackTransaction skips the savepoint — it always rolls back the whole txn.
        let sp_opt: Option<Savepoint> = if ctx.on_error == OnErrorMode::RollbackTransaction {
            None
        } else {
            Some(txn.savepoint())
        };
        match dispatch_ctx(stmt, storage, txn, bloom, ctx) {
            Ok(result) => Ok(result),
            Err(e) => match ctx.on_error {
                OnErrorMode::RollbackTransaction => {
                    // Eager whole-transaction rollback.
                    let _ = txn.rollback(storage);
                    Err(e)
                }
                OnErrorMode::Ignore if crate::session::is_ignorable_on_error(&e) => {
                    // Ignorable error: undo statement, keep txn open, convert to warning.
                    // database.rs will emit the warning; executor just rolls back the stmt.
                    if let Some(sp) = sp_opt {
                        let _ = txn.rollback_to_savepoint(sp, storage);
                    }
                    Err(e) // database.rs intercepts this and converts to QueryResult::Empty
                }
                OnErrorMode::Ignore => {
                    // Infrastructure/runtime failures must not leave the txn open.
                    let _ = txn.rollback(storage);
                    Err(e)
                }
                _ => {
                    // RollbackStatement / Savepoint / Ignore non-ignorable:
                    // undo only this statement's writes, keep the transaction active.
                    if let Some(sp) = sp_opt {
                        let _ = txn.rollback_to_savepoint(sp, storage);
                    }
                    Err(e)
                }
            },
        }

    // ── No active transaction — autocommit=true (default) ─────────────────────
    } else if ctx.autocommit {
        match stmt {
            Stmt::Begin => {
                txn.begin()?;
                Ok(QueryResult::Empty)
            }
            // No active transaction: COMMIT/ROLLBACK is a no-op (MySQL compat).
            // Emit warning 1592 so clients can see it via SHOW WARNINGS.
            Stmt::Commit => {
                ctx.warn(1592, "There is no active transaction");
                Ok(QueryResult::Empty)
            }
            Stmt::Rollback => {
                ctx.warn(1592, "There is no active transaction");
                Ok(QueryResult::Empty)
            }
            other => {
                txn.begin()?;
                match dispatch_ctx(other, storage, txn, bloom, ctx) {
                    Ok(result) => {
                        let tid = txn.active_txn_id();
                        txn.commit()?;
                        if let Some(t) = tid {
                            txn.release_immediate_committed_frees(storage, t)?;
                        }
                        Ok(result)
                    }
                    Err(e) => {
                        let _ = txn.rollback(storage);
                        Err(e)
                    }
                }
            }
        }

    // ── No active transaction — autocommit=false (SET autocommit=0) ───────────
    } else {
        match stmt {
            // Explicit TCL — BEGIN is no-op if no txn active yet (MySQL compat).
            Stmt::Begin => {
                txn.begin()?;
                Ok(QueryResult::Empty)
            }
            // No active transaction: COMMIT/ROLLBACK is a no-op (MySQL compat).
            // Emit warning 1592 so clients can see it via SHOW WARNINGS.
            Stmt::Commit => {
                ctx.warn(1592, "There is no active transaction");
                Ok(QueryResult::Empty)
            }
            Stmt::Rollback => {
                ctx.warn(1592, "There is no active transaction");
                Ok(QueryResult::Empty)
            }

            // SELECT (read-only) — wrap in a read-only begin/commit so the executor
            // has a valid snapshot. The transaction is committed immediately after,
            // leaving no open transaction (MySQL: SELECT in autocommit=0 does not
            // start a lasting implicit transaction).
            Stmt::Select(_) => {
                txn.begin()?;
                match dispatch_ctx(stmt, storage, txn, bloom, ctx) {
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

            // DDL — implicit commit of any open txn (handled above), then DDL
            // runs in its own autocommit transaction.
            other if is_ddl(&other) => {
                txn.begin()?;
                match dispatch_ctx(other, storage, txn, bloom, ctx) {
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

            // DML (INSERT, UPDATE, DELETE) — implicit BEGIN, no COMMIT.
            // Transaction stays open until the client sends explicit COMMIT/ROLLBACK.
            //
            // on_error = 'savepoint': create a savepoint right after BEGIN so that
            // the first failing DML can be undone while keeping the implicit txn open.
            // All other modes roll back the whole txn on first-DML failure.
            other => {
                txn.begin()?;
                let sp_opt: Option<Savepoint> = if ctx.on_error == OnErrorMode::Savepoint
                    || ctx.on_error == OnErrorMode::Ignore
                {
                    Some(txn.savepoint())
                } else {
                    None
                };
                match dispatch_ctx(other, storage, txn, bloom, ctx) {
                    Ok(result) => Ok(result),
                    Err(e) => match ctx.on_error {
                        OnErrorMode::Ignore if crate::session::is_ignorable_on_error(&e) => {
                            if let Some(sp) = sp_opt {
                                // Ignore is Savepoint-like for ignorable SQL errors.
                                let _ = txn.rollback_to_savepoint(sp, storage);
                            }
                            Err(e)
                        }
                        OnErrorMode::Savepoint => {
                            if let Some(sp) = sp_opt {
                                let _ = txn.rollback_to_savepoint(sp, storage);
                            }
                            Err(e)
                        }
                        _ => {
                            // rollback_statement / rollback_transaction / ignore
                            // non-ignorable: close the implicit txn completely.
                            let _ = txn.rollback(storage);
                            Err(e)
                        }
                    },
                }
            }
        }
    }
}

/// Returns `true` for DDL statements that require their own autocommit transaction.
///
/// MySQL DDL always causes an implicit COMMIT of any open transaction and then
/// executes in its own single-statement transaction.
fn is_ddl(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateTable(_)
            | Stmt::DropTable(_)
            | Stmt::CreateIndex(_)
            | Stmt::DropIndex(_)
            | Stmt::AlterTable(_)
            | Stmt::TruncateTable(_)
    )
}

/// Routes a statement to its handler using a `SessionContext` for schema caching.
fn dispatch_ctx(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    match stmt {
        Stmt::Select(s) => execute_select_ctx(s, storage, txn, bloom, ctx),
        Stmt::Insert(s) => execute_insert_ctx(s, storage, txn, bloom, ctx),
        Stmt::Update(s) => execute_update_ctx(s, storage, txn, bloom, ctx),
        Stmt::Delete(s) => execute_delete_ctx(s, storage, txn, bloom, ctx),
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
            execute_create_index(s, storage, txn, bloom)
        }
        Stmt::DropIndex(s) => {
            ctx.invalidate_all();
            execute_drop_index(s, storage, txn, bloom)
        }
        Stmt::AlterTable(s) => {
            ctx.invalidate_all();
            execute_alter_table(s, storage, txn)
        }
        Stmt::Analyze(s) => execute_analyze(s, storage, txn, ctx),
        Stmt::Set(s) => execute_set_ctx(s, ctx),
        // Everything else: delegate to existing dispatch.
        other => dispatch(other, storage, txn),
    }
}

/// Handles `SET variable = value` in the ctx execution path.
///
/// Supported variables:
/// - `autocommit` — boolish ON/OFF/1/0/TRUE/FALSE
/// - `strict_mode` — boolish or DEFAULT (resets to `true`)
/// - `sql_mode` — normalized string; strict tokens control `ctx.strict_mode`
///
/// All other variables are accepted silently (existing stub behavior).
/// Extracts the string value from a `SetValue` for settings that accept string
/// literals or bare identifiers (e.g. `SET on_error = rollback_statement`).
///
/// Returns:
/// - `Ok(None)` for `SetValue::Default`
/// - `Ok(Some(s))` for string literals and bare identifiers
/// - `Err` for any other expression
fn set_value_to_setting_string(value: &SetValue) -> Result<Option<String>, DbError> {
    match value {
        SetValue::Default => Ok(None),
        // String literal: SET on_error = 'rollback_statement'
        SetValue::Expr(Expr::Literal(Value::Text(s))) => Ok(Some(s.clone())),
        // Integer literal: SET x = 1
        SetValue::Expr(Expr::Literal(Value::Int(n))) => Ok(Some(n.to_string())),
        SetValue::Expr(Expr::Literal(Value::BigInt(n))) => Ok(Some(n.to_string())),
        // Boolean literal: SET x = TRUE
        SetValue::Expr(Expr::Literal(Value::Bool(b))) => {
            Ok(Some(if *b { "1".to_string() } else { "0".to_string() }))
        }
        // Bare identifier: SET on_error = rollback_statement (no quotes).
        // The analyzer resolves bare identifiers in SET expressions to Column
        // nodes. Extracting the name avoids calling eval() with an empty row.
        SetValue::Expr(Expr::Column { name, .. }) => Ok(Some(name.clone())),
        // Constant expression fallback (e.g. CONCAT, arithmetic).
        SetValue::Expr(other) => match eval(other, &[]) {
            Ok(Value::Text(s)) => Ok(Some(s)),
            Ok(Value::Int(n)) => Ok(Some(n.to_string())),
            Ok(Value::BigInt(n)) => Ok(Some(n.to_string())),
            Ok(Value::Bool(b)) => Ok(Some(if b { "1".to_string() } else { "0".to_string() })),
            _ => Err(DbError::InvalidValue {
                reason: "SET value must be a string literal or bare identifier".to_string(),
            }),
        },
    }
}

fn execute_set_ctx(stmt: SetStmt, ctx: &mut SessionContext) -> Result<QueryResult, DbError> {
    match stmt.variable.to_ascii_lowercase().as_str() {
        "autocommit" => match stmt.value {
            SetValue::Default => ctx.autocommit = true,
            SetValue::Expr(expr) => {
                let v = eval(&expr, &[])?;
                let raw = match &v {
                    Value::Text(s) => s.clone(),
                    Value::Int(n) => n.to_string(),
                    Value::BigInt(n) => n.to_string(),
                    Value::Bool(b) => {
                        if *b {
                            "1".to_string()
                        } else {
                            "0".to_string()
                        }
                    }
                    other => {
                        return Err(DbError::InvalidValue {
                            reason: format!("autocommit: unsupported value type {other:?}"),
                        })
                    }
                };
                ctx.autocommit = parse_boolish_setting(&raw)?;
            }
        },
        "strict_mode" => match stmt.value {
            SetValue::Default => ctx.strict_mode = true,
            SetValue::Expr(expr) => {
                let v = eval(&expr, &[])?;
                let raw = match &v {
                    Value::Text(s) => s.clone(),
                    Value::Int(n) => n.to_string(),
                    Value::BigInt(n) => n.to_string(),
                    Value::Bool(b) => {
                        if *b {
                            "1".to_string()
                        } else {
                            "0".to_string()
                        }
                    }
                    other => {
                        return Err(DbError::InvalidValue {
                            reason: format!("strict_mode: unsupported value type {other:?}"),
                        })
                    }
                };
                ctx.strict_mode = parse_boolish_setting(&raw)?;
            }
        },
        "sql_mode" => match stmt.value {
            SetValue::Default => ctx.strict_mode = true,
            SetValue::Expr(expr) => {
                let v = eval(&expr, &[])?;
                let raw = match &v {
                    Value::Text(s) => s.clone(),
                    other => {
                        return Err(DbError::InvalidValue {
                            reason: format!("sql_mode: expected string literal, got {other:?}"),
                        })
                    }
                };
                let normalized = normalize_sql_mode(&raw);
                ctx.strict_mode = sql_mode_is_strict(&normalized);
            }
        },
        "on_error" => {
            let raw = match set_value_to_setting_string(&stmt.value)? {
                None => "rollback_statement".to_string(), // DEFAULT
                Some(s) => s,
            };
            ctx.on_error = parse_on_error_setting(&raw)?;
        }
        "axiom_compat" => {
            let raw = match set_value_to_setting_string(&stmt.value)? {
                None => "standard".to_string(), // DEFAULT
                Some(s) => s,
            };
            // Validate and store compat mode. Does NOT clear an explicit
            // collation override already chosen by the session.
            ctx.compat_mode = parse_compat_mode_setting(&raw)?;
        }
        "collation" => {
            let raw = match set_value_to_setting_string(&stmt.value)? {
                None => "default".to_string(), // DEFAULT = clear override
                Some(s) => s,
            };
            ctx.explicit_collation = parse_session_collation_setting(&raw)?;
        }
        _ => {} // other variables: silently accepted
    }
    Ok(QueryResult::Empty)
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
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // Set the session collation for all eval() calls in this ctx execution.
    // Cleared automatically when _coll_guard is dropped at function exit.
    let _coll_guard = CollationGuard::new(ctx.effective_collation());

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

        // ── Query planner: pick the best access method ────────────────────
        // Load per-column statistics for cost-based index selection (Phase 6.10).
        let table_stats: Vec<axiomdb_catalog::StatsDef> = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader.list_stats(resolved.def.id).unwrap_or_default()
        };
        // Collect SELECT column indices for index-only scan detection (Phase 6.13).
        // Returns empty slice for SELECT * (wildcard) → conservative, no index-only.
        let select_col_idxs: Vec<u16> = collect_select_col_idxs(&stmt);

        // Compute collation before the mutable borrow of ctx.stats below.
        let effective_coll = ctx.effective_collation();
        let access_method = crate::planner::plan_select_ctx(
            stmt.where_clause.as_ref(),
            &resolved.indexes,
            &resolved.columns,
            resolved.def.id,
            &table_stats,
            &mut ctx.stats,
            &select_col_idxs,
            effective_coll,
        );

        // Fetch rows via the chosen access method.
        let raw_rows: Vec<(RecordId, Vec<Value>)> = match &access_method {
            crate::planner::AccessMethod::Scan => {
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
                TableEngine::scan_table(
                    storage,
                    &resolved.def,
                    &resolved.columns,
                    snap,
                    column_mask.as_deref(),
                )?
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key } => {
                // Bloom filter: skip B-Tree read if key is definitely absent.
                // Only applied for UNIQUE indexes — non-unique indexes store key||RID in
                // the bloom (one entry per row), but the lookup key here is the bare value.
                // Checking a bare value key against a bloom populated with key||RID entries
                // produces false negatives, so we skip the bloom check for non-unique indexes.
                if index_def.is_unique && !bloom.might_exist(index_def.index_id, key) {
                    vec![]
                } else if index_def.is_unique {
                    // Unique index: exact key lookup → at most one RecordId.
                    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                        None => vec![],
                        Some(rid) => {
                            match TableEngine::read_row(storage, &resolved.columns, rid)? {
                                None => vec![], // row was deleted
                                Some(values) => vec![(rid, values)],
                            }
                        }
                    }
                } else {
                    // Non-unique index: key stored as key||RID — use range scan with
                    // [key||0x00..00, key||0xFF..FF] to find all rows with this value.
                    let lo = rid_lo(key);
                    let hi = rid_hi(key);
                    let pairs =
                        BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?;
                    let mut result = Vec::with_capacity(pairs.len());
                    for (rid, _k) in pairs {
                        if let Some(values) =
                            TableEngine::read_row(storage, &resolved.columns, rid)?
                        {
                            result.push((rid, values));
                        }
                    }
                    result
                }
            }
            crate::planner::AccessMethod::IndexRange { index_def, lo, hi } => {
                // Range scan: iterate B-Tree entries → heap reads.
                // Non-unique: append RID suffix to bounds so the range covers all RIDs.
                let (lo_adjusted, hi_adjusted);
                let (lo_ref, hi_ref) = if index_def.is_unique {
                    (lo.as_deref(), hi.as_deref())
                } else {
                    lo_adjusted = lo.as_deref().map(rid_lo);
                    hi_adjusted = hi.as_deref().map(rid_hi);
                    (lo_adjusted.as_deref(), hi_adjusted.as_deref())
                };
                let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;
                let mut result = Vec::with_capacity(pairs.len());
                for (rid, _key) in pairs {
                    if let Some(values) = TableEngine::read_row(storage, &resolved.columns, rid)? {
                        result.push((rid, values));
                    }
                }
                result
            }
            crate::planner::AccessMethod::IndexOnlyScan {
                index_def,
                lo,
                hi,
                n_key_cols,
                needed_key_positions: _,
            } => {
                // Index-only scan (Phase 6.13): values decoded from B-Tree key bytes.
                // Only the 24-byte heap slot header is read for MVCC visibility.
                // Non-unique: lo/hi need RID suffix for correct range bounds.
                let (lo_adj, hi_adj);
                let (lo_ref, hi_ref) = if index_def.is_unique {
                    (Some(lo.as_slice()), hi.as_deref())
                } else {
                    lo_adj = rid_lo(lo);
                    hi_adj = hi.as_deref().map(rid_hi);
                    (Some(lo_adj.as_slice()), hi_adj.as_deref())
                };
                let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;
                let n_table_cols = resolved.columns.len();
                let mut result = Vec::with_capacity(pairs.len());
                for (rid, key_bytes) in pairs {
                    if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap)? {
                        continue;
                    }
                    let (all_key_vals, _) =
                        crate::key_encoding::decode_index_key(&key_bytes, *n_key_cols)?;
                    // Build a full-width row (Null for non-indexed cols) so that
                    // WHERE and SELECT expressions can access values by table col_idx.
                    // Populate all decoded key columns — not just the SELECT ones —
                    // so that WHERE re-evaluation can access them too.
                    let mut row_values = vec![Value::Null; n_table_cols];
                    for (key_pos, idx_col) in index_def.columns.iter().enumerate() {
                        let table_idx = idx_col.col_idx as usize;
                        if let (true, Some(val)) =
                            (table_idx < n_table_cols, all_key_vals.get(key_pos))
                        {
                            row_values[table_idx] = val.clone();
                        }
                    }
                    result.push((rid, row_values));
                }
                result
            }
        };

        let mut combined_rows: Vec<Row> = Vec::new();
        for (_rid, values) in raw_rows {
            if let Some(ref wc) = stmt.where_clause {
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    bloom,
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
            // Single-table path: choose sorted strategy when the access method
            // already delivers rows in group-key order (Phase 4.9b).
            let strategy = choose_group_by_strategy_ctx_with_collation(
                &stmt.group_by,
                &access_method,
                effective_coll,
                &resolved.columns,
            );
            return execute_select_grouped(stmt, combined_rows, strategy);
        }

        combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

        let out_cols = build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
        let mut rows = combined_rows
            .iter()
            .map(|v| {
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    bloom,
                    ctx,
                    outer_row: v,
                };
                project_row_with(&stmt.columns, v, &mut runner)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if stmt.distinct {
            rows = apply_distinct_with_session(rows);
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
    // Session collation for eval()-based comparisons in join ON, WHERE, ORDER BY, etc.
    // Guard propagates from execute_select_ctx when called via the join path, but
    // we set it here too so this function can also be called independently.
    let _coll_guard = CollationGuard::new(ctx.effective_collation());

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
        // JOIN path: no ordering guarantee — always hash aggregate.
        return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
    }

    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

    let out_cols = build_join_column_meta(&stmt.columns, &all_resolved, &stmt.joins)?;

    let mut rows = combined_rows
        .iter()
        .map(|r| project_row(&stmt.columns, r))
        .collect::<Result<Vec<_>, _>>()?;

    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
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
    bloom: &mut crate::bloom::BloomRegistry,
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
    let mut secondary_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

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

    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, schema_cols)?;

    match stmt.source {
        InsertSource::Values(rows) => {
            for (row_idx, value_exprs) in rows.into_iter().enumerate() {
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

                // Evaluate active CHECK constraints from axiom_constraints.
                check_row_constraints(
                    &resolved.constraints,
                    &full_values,
                    &resolved.def.table_name,
                )?;

                // FK validation: every non-NULL FK value must reference an existing parent row.
                if !resolved.foreign_keys.is_empty() {
                    crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        bloom,
                    )?;
                }

                // Clone so full_values remains available for index maintenance.
                let rid = TableEngine::insert_row_with_ctx(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    ctx,
                    full_values.clone(),
                    row_idx + 1,
                )?;
                if !secondary_indexes.is_empty() {
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        bloom,
                        &compiled_preds,
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                        if let Some(idx) = secondary_indexes
                            .iter_mut()
                            .find(|i| i.index_id == index_id)
                        {
                            idx.root_page_id = new_root;
                        }
                        // The schema cache stores the old root_page_id. Invalidate
                        // so the next call re-reads from catalog rather than calling
                        // lookup_in with a freed page id.
                        ctx.invalidate_all();
                    }
                }
                count += 1;
            }
        }
        InsertSource::Select(select_stmt) => {
            let select_rows = match execute_select_ctx(*select_stmt, storage, txn, bloom, ctx)? {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(DbError::Other(format!(
                        "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                    )))
                }
            };
            for (row_idx, row_values) in select_rows.into_iter().enumerate() {
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
                // FK validation for INSERT SELECT path.
                if !resolved.foreign_keys.is_empty() {
                    crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        bloom,
                    )?;
                }
                // Clone so full_values remains available for index maintenance.
                let rid = TableEngine::insert_row_with_ctx(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    ctx,
                    full_values.clone(),
                    row_idx + 1,
                )?;
                if !secondary_indexes.is_empty() {
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        bloom,
                        &compiled_preds,
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                        if let Some(idx) = secondary_indexes
                            .iter_mut()
                            .find(|i| i.index_id == index_id)
                        {
                            idx.root_page_id = new_root;
                        }
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

    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        // Track row changes for stats staleness (Phase 6.11).
        ctx.stats.on_rows_changed(resolved.def.id, count);
        return Ok(QueryResult::affected_with_id(count, id));
    }

    // Track row changes for stats staleness (Phase 6.11).
    ctx.stats.on_rows_changed(resolved.def.id, count);

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

fn execute_update_ctx(
    stmt: UpdateStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
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
    let secondary_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

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

    // Collect all matching (rid, old_values, new_values) triples before touching
    // the heap. Old values are kept for secondary index maintenance (delete old
    // key before inserting new key into each B-Tree).
    let mut to_update: Vec<(RecordId, Vec<Value>, Vec<Value>)> = Vec::new();
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
        to_update.push((rid, current_values, new_values));
    }

    let count = to_update.len() as u64;

    // FK child validation: check new FK values before applying any updates.
    if !resolved.foreign_keys.is_empty() {
        for (_, old_values, new_values) in &to_update {
            crate::fk_enforcement::check_fk_child_update(
                old_values,
                new_values,
                &resolved.foreign_keys,
                storage,
                txn,
                bloom,
            )?;
        }
    }

    // FK parent enforcement: check if this table is referenced by any FK and
    // the referenced column value is changing (RESTRICT/NO ACTION).
    if !to_update.is_empty() {
        let old_rows: Vec<(RecordId, Vec<Value>)> = to_update
            .iter()
            .map(|(rid, old, _)| (*rid, old.clone()))
            .collect();
        let new_rows: Vec<Vec<Value>> = to_update.iter().map(|(_, _, new)| new.clone()).collect();
        crate::fk_enforcement::enforce_fk_on_parent_update(
            &old_rows,
            &new_rows,
            resolved.def.id,
            storage,
            txn,
        )?;
    }

    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, &schema_cols)?;

    if secondary_indexes.is_empty() {
        // Fast path: no secondary indexes — use batch heap update (O(P) page I/O).
        let heap_updates: Vec<(RecordId, Vec<Value>)> = to_update
            .into_iter()
            .map(|(rid, _old, new)| (rid, new))
            .collect();
        match heap_updates.len() {
            0 => {}
            1 => {
                let (rid, new_values) = heap_updates.into_iter().next().unwrap();
                TableEngine::update_row_with_ctx(
                    storage,
                    txn,
                    &resolved.def,
                    &schema_cols,
                    ctx,
                    rid,
                    new_values,
                )?;
            }
            _ => {
                // Multi-row batch: delete_batch + insert_batch — O(P) page I/O.
                TableEngine::update_rows_batch_with_ctx(
                    storage,
                    txn,
                    &resolved.def,
                    &schema_cols,
                    ctx,
                    heap_updates,
                )?;
            }
        }
    } else {
        // Secondary indexes present: per-row update so index maintenance has
        // both old key (for delete) and new RecordId (for insert + bloom.add).
        let mut current_indexes = secondary_indexes;
        for (rid, old_values, new_values) in to_update {
            let new_rid = TableEngine::update_row_with_ctx(
                storage,
                txn,
                &resolved.def,
                &schema_cols,
                ctx,
                rid,
                new_values.clone(),
            )?;
            // Remove old key from each secondary index; mark bloom dirty.
            let del_updated = crate::index_maintenance::delete_from_indexes(
                &current_indexes,
                &old_values,
                rid,
                storage,
                bloom,
                &compiled_preds,
            )?;
            for (index_id, new_root) in &del_updated {
                CatalogWriter::new(storage, txn)?.update_index_root(*index_id, *new_root)?;
            }
            // Refresh in-memory roots before insert so the insert uses the
            // correct (post-delete) root page.
            for (index_id, new_root) in del_updated {
                if let Some(idx) = current_indexes.iter_mut().find(|i| i.index_id == index_id) {
                    idx.root_page_id = new_root;
                }
            }
            // Insert new key into each secondary index; add to bloom.
            let ins_updated = crate::index_maintenance::insert_into_indexes(
                &current_indexes,
                &new_values,
                new_rid,
                storage,
                bloom,
                &compiled_preds,
            )?;
            for (index_id, new_root) in ins_updated {
                CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                if let Some(idx) = current_indexes.iter_mut().find(|i| i.index_id == index_id) {
                    idx.root_page_id = new_root;
                }
            }
        }
        // B-Tree CoW operations change root page IDs. Invalidate the session
        // cache after the full update so subsequent queries reload fresh roots.
        ctx.invalidate_all();
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
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let resolved = resolve_table_cached(
        storage,
        txn,
        ctx,
        stmt.table.schema.as_deref(),
        &stmt.table.name,
    )?;

    let secondary_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    let snap = txn.active_snapshot()?;

    // Check if any FK constraint references THIS table as the parent.
    // If so, we must scan rows (to get parent key values) and cannot use the fast path.
    let has_fk_references = {
        let mut reader = CatalogReader::new(storage, snap)?;
        !reader
            .list_fk_constraints_referencing(resolved.def.id)?
            .is_empty()
    };

    // No-WHERE + no FK parent references → bulk-empty fast path (Phase 5.16).
    // This replaces the old "no secondary indexes" gate: PK + UNIQUE + composite
    // indexes are all handled by root rotation, not per-row B-Tree deletes.
    if stmt.where_clause.is_none() && !has_fk_references {
        // Collect all indexes with columns (PK, UNIQUE, non-unique, FK auto-indexes).
        let all_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
            .indexes
            .iter()
            .filter(|i| !i.columns.is_empty())
            .cloned()
            .collect();

        let plan = plan_bulk_empty_table(storage, &resolved.def, &all_indexes, snap)?;
        let count = plan.visible_row_count;

        apply_bulk_empty_table(storage, txn, bloom, &resolved.def, plan)?;

        // Invalidate session schema cache so the next query reloads the new roots.
        ctx.invalidate_all();

        // Release deferred pages now if we're in immediate-commit mode.
        // In group-commit mode this is handled by the CommitCoordinator.
        // We use a best-effort release here; group-commit path does not hold
        // an active txn at this point, so active_txn_id() == None.
        if let Some(committed_txn_id) = txn.active_txn_id() {
            // Still inside an explicit transaction — pages freed at outer COMMIT.
            let _ = committed_txn_id; // suppress unused warning
        }

        return Ok(QueryResult::Affected {
            count,
            last_insert_id: None,
        });
    }

    // Candidate discovery (Phase 6.3b): use index when predicate is sargable.
    let schema_cols = resolved.columns.clone();
    let to_delete: Vec<(RecordId, Vec<Value>)> = if let Some(ref wc) = stmt.where_clause {
        let effective_coll = ctx.effective_collation();
        let delete_access = crate::planner::plan_delete_candidates_ctx(
            wc,
            &secondary_indexes,
            &schema_cols,
            effective_coll,
        );
        collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &delete_access,
            storage,
            snap,
            &resolved.def,
            bloom,
        )?
    } else {
        // No WHERE and has_fk_references=true (bulk-empty path already returned
        // for the no-WHERE + no-FK case). Full scan: all rows qualify.
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?
    };

    // FK parent enforcement: must run BEFORE heap delete so RESTRICT can abort
    // cleanly and CASCADE/SET NULL can still read/update child rows.
    if has_fk_references && !to_delete.is_empty() {
        crate::fk_enforcement::enforce_fk_on_parent_delete(
            &to_delete,
            resolved.def.id,
            storage,
            txn,
            bloom,
            0,
        )?;
    }

    // Batch-delete from heap: each page read+written once instead of 3× per row.
    let rids_only: Vec<RecordId> = to_delete.iter().map(|(rid, _)| *rid).collect();
    let count = TableEngine::delete_rows_batch(storage, txn, &resolved.def, &rids_only)?;

    // Index maintenance: per-row B-Tree deletes; bloom marked dirty per index.
    if !secondary_indexes.is_empty() {
        let compiled_preds =
            crate::partial_index::compile_index_predicates(&secondary_indexes, &schema_cols)?;
        let mut any_root_changed = false;
        let mut secondary_indexes = secondary_indexes; // shadow as mut for root sync
        for (rid, row_vals) in &to_delete {
            let updated = crate::index_maintenance::delete_from_indexes(
                &secondary_indexes,
                row_vals,
                *rid,
                storage,
                bloom,
                &compiled_preds,
            )?;
            for (index_id, new_root) in updated {
                CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                // Keep the in-memory snapshot in sync so the next row's deletion
                // starts from the correct (current) root — same fix as execute_delete.
                for idx in secondary_indexes.iter_mut() {
                    if idx.index_id == index_id {
                        idx.root_page_id = new_root;
                        break;
                    }
                }
                any_root_changed = true;
            }
        }
        // Invalidate the session cache so the next query reloads fresh root IDs.
        if any_root_changed {
            ctx.invalidate_all();
        }
    }

    // Track row changes for stats staleness (Phase 6.11).
    if count > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, count);
    }

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
        // GroupConcat: recurse into the concatenated expr and ORDER BY exprs.
        Expr::GroupConcat { expr, order_by, .. } => {
            collect_column_refs(expr, mask);
            for (e, _) in order_by {
                collect_column_refs(e, mask);
            }
        }
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
        Stmt::CreateIndex(s) => {
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_create_index(s, storage, txn, &mut noop_bloom)
        }
        Stmt::DropIndex(s) => {
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_drop_index(s, storage, txn, &mut noop_bloom)
        }
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
        Stmt::Analyze(_) => Err(DbError::NotImplemented {
            feature: "ANALYZE requires session context — use execute_with_ctx".into(),
        }),
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
        // require a runner; we use a temporary SessionContext and a temporary bloom.
        let mut temp_ctx = SessionContext::new();
        let mut temp_bloom = crate::bloom::BloomRegistry::new();
        let mut runner = ExecSubqueryRunner {
            storage,
            txn,
            bloom: &mut temp_bloom,
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
            apply_distinct_with_session(vec![out_row])
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

        // ── Query planner: pick the best access method (non-ctx path) ────
        // No session context available — use conservative defaults (no stats).
        let access_method = crate::planner::plan_select(
            stmt.where_clause.as_ref(),
            &resolved.indexes,
            &resolved.columns,
            resolved.def.id,
            &[], // no stats in non-ctx path — always use index (conservative)
            &mut crate::session::StaleStatsTracker::default(),
            &[], // no select_col_idxs in non-ctx path — no index-only scan
        );

        // ── Fetch rows via the chosen access method ───────────────────────
        let raw_rows: Vec<(RecordId, Vec<Value>)> = match &access_method {
            crate::planner::AccessMethod::Scan => {
                // Full sequential scan — existing behavior.
                TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap, None)?
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key } => {
                // Point lookup: unique → exact match; non-unique → range with RID suffix.
                if index_def.is_unique {
                    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                        None => vec![],
                        Some(rid) => {
                            match TableEngine::read_row(storage, &resolved.columns, rid)? {
                                None => vec![], // row was deleted
                                Some(values) => vec![(rid, values)],
                            }
                        }
                    }
                } else {
                    let lo = rid_lo(key);
                    let hi = rid_hi(key);
                    let pairs =
                        BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?;
                    let mut result = Vec::with_capacity(pairs.len());
                    for (rid, _k) in pairs {
                        if let Some(values) =
                            TableEngine::read_row(storage, &resolved.columns, rid)?
                        {
                            result.push((rid, values));
                        }
                    }
                    result
                }
            }
            crate::planner::AccessMethod::IndexRange { index_def, lo, hi } => {
                // Range scan: iterate B-Tree entries → heap reads.
                let (lo_adjusted, hi_adjusted);
                let (lo_ref, hi_ref) = if index_def.is_unique {
                    (lo.as_deref(), hi.as_deref())
                } else {
                    lo_adjusted = lo.as_deref().map(rid_lo);
                    hi_adjusted = hi.as_deref().map(rid_hi);
                    (lo_adjusted.as_deref(), hi_adjusted.as_deref())
                };
                let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;
                let mut result = Vec::with_capacity(pairs.len());
                for (rid, _key) in pairs {
                    if let Some(values) = TableEngine::read_row(storage, &resolved.columns, rid)? {
                        result.push((rid, values));
                    }
                }
                result
            }
            // IndexOnlyScan not used in non-ctx path (select_col_idxs = &[] above).
            crate::planner::AccessMethod::IndexOnlyScan { .. } => {
                unreachable!("IndexOnlyScan only emitted when select_col_idxs is non-empty")
            }
        };

        let mut combined_rows: Vec<Row> = Vec::new();
        for (_rid, values) in raw_rows {
            if let Some(ref wc) = stmt.where_clause {
                let mut temp_ctx = SessionContext::new();
                let mut temp_bloom = crate::bloom::BloomRegistry::new();
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    bloom: &mut temp_bloom,
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
            return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
        }

        combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

        let out_cols = build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
        let mut rows = combined_rows
            .iter()
            .map(|v| {
                let mut temp_ctx = SessionContext::new();
                let mut temp_bloom = crate::bloom::BloomRegistry::new();
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    bloom: &mut temp_bloom,
                    ctx: &mut temp_ctx,
                    outer_row: v,
                };
                project_row_with(&stmt.columns, v, &mut runner)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if stmt.distinct {
            rows = apply_distinct_with_session(rows);
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
    let mut temp_bloom = crate::bloom::BloomRegistry::new();
    let inner_result =
        execute_select_ctx(inner_query, storage, txn, &mut temp_bloom, &mut temp_ctx)?;
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
            let mut temp_bloom2 = crate::bloom::BloomRegistry::new();
            let mut runner = ExecSubqueryRunner {
                storage,
                txn,
                bloom: &mut temp_bloom2,
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
        return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
    }

    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

    // Build output columns from SELECT list against derived column metadata.
    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = combined_rows
        .iter()
        .map(|v| project_row(&stmt.columns, v))
        .collect::<Result<Vec<_>, _>>()?;

    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
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
        return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
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
        rows = apply_distinct_with_session(rows);
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

        JoinType::Full => {
            // FULL OUTER JOIN = matched pairs + unmatched left rows (NULL right)
            //                 + unmatched right rows (NULL left).
            //
            // A matched-right bitmap tracks which right rows were joined so the
            // second pass can emit the unmatched ones without duplicating them.
            let null_left: Row = vec![Value::Null; left_col_count];
            let null_right: Row = vec![Value::Null; right_col_count];
            let mut matched_right = vec![false; right_rows.len()];
            let mut result = Vec::new();

            for left in &left_rows {
                let mut matched = false;
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
                        matched = true;
                        matched_right[i] = true;
                    }
                }
                if !matched {
                    // Left row had no match — emit with NULLs on the right side.
                    result.push(concat_rows(left, &null_right));
                }
            }

            // Emit right rows that were never matched with NULLs on the left side.
            for (i, right) in right_rows.iter().enumerate() {
                if !matched_right[i] {
                    result.push(concat_rows(&null_left, right));
                }
            }

            Ok(result)
        }
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
    // Precompute outer-join nullability once for the whole chain.
    // This correctly handles LEFT, RIGHT, FULL, and mixed chains.
    let nullable_tables = compute_outer_nullable(all_tables.len(), joins);
    let mut out = Vec::new();

    for item in items {
        match item {
            SelectItem::Wildcard => {
                // Expand all columns from all tables in order.
                for (t_idx, table) in all_tables.iter().enumerate() {
                    let outer_nullable = nullable_tables[t_idx];
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
                let outer_nullable = nullable_tables[t_idx];
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
                let (dt, nullable) = infer_expr_type_join(expr, all_tables, &nullable_tables);
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

/// Computes per-table outer-join nullability for a join chain.
///
/// Returns a `Vec<bool>` of length `table_count` where `[i]` is `true` if
/// table `i` can be null-extended by any join in the chain:
///
/// - `LEFT JOIN`: the right table becomes nullable.
/// - `RIGHT JOIN`: all accumulated left tables (0..=join_idx) become nullable.
/// - `FULL JOIN`: both sides become nullable.
/// - `INNER` / `CROSS`: no side becomes nullable.
///
/// This replaces the old `is_outer_nullable(t_idx, joins)` helper which only
/// looked at a single join and therefore produced wrong metadata for mixed
/// outer-join chains and for `FULL JOIN`.
fn compute_outer_nullable(table_count: usize, joins: &[JoinClause]) -> Vec<bool> {
    let mut nullable = vec![false; table_count];
    for (join_idx, join) in joins.iter().enumerate() {
        let right_table = join_idx + 1;
        match join.join_type {
            JoinType::Inner | JoinType::Cross => {}
            JoinType::Left => {
                if right_table < table_count {
                    nullable[right_table] = true;
                }
            }
            JoinType::Right => {
                nullable[..right_table.min(table_count)].fill(true);
            }
            JoinType::Full => {
                nullable[..right_table.min(table_count)].fill(true);
                if right_table < table_count {
                    nullable[right_table] = true;
                }
            }
        }
    }
    nullable
}

/// Infers (DataType, nullable) for an expression in a JOIN context.
fn infer_expr_type_join(
    expr: &Expr,
    all_tables: &[axiomdb_catalog::ResolvedTable],
    nullable_tables: &[bool],
) -> (DataType, bool) {
    if let Expr::Column { col_idx, .. } = expr {
        // Find which table owns this col_idx and what the column type is.
        let mut offset = 0;
        for (t_idx, table) in all_tables.iter().enumerate() {
            let end = offset + table.columns.len();
            if *col_idx < end {
                let local_pos = col_idx - offset;
                if let Some(col) = table.columns.get(local_pos) {
                    let outer_nullable = nullable_tables.get(t_idx).copied().unwrap_or(false);
                    let nullable = col.nullable || outer_nullable;
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
        // GROUP_CONCAT is always an aggregate — detected via the dedicated AST variant.
        Expr::GroupConcat { .. } => true,
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
enum AggExpr {
    /// Standard aggregate: COUNT, SUM, MIN, MAX, AVG.
    Simple {
        /// Lowercase function name: "count", "sum", "min", "max", "avg".
        name: String,
        /// The argument expression. `None` for `COUNT(*)`.
        arg: Option<Expr>,
        /// Position in `GroupState::accumulators`. Preserved for diagnostics.
        #[allow(dead_code)]
        agg_idx: usize,
    },
    /// GROUP_CONCAT / string_agg aggregate.
    GroupConcat {
        /// Expression to evaluate and concatenate per row.
        expr: Box<Expr>,
        /// If true, deduplicate values before concatenating.
        distinct: bool,
        /// Per-aggregate ORDER BY: (sort_expr, direction) pairs.
        order_by: Vec<(Expr, crate::ast::SortOrder)>,
        /// Separator string (default `","`).
        separator: String,
        /// Position in `GroupState::accumulators`. Preserved for diagnostics.
        #[allow(dead_code)]
        agg_idx: usize,
    },
}

impl AggExpr {
    /// Returns the accumulator index for this aggregate.
    #[allow(dead_code)]
    fn agg_idx(&self) -> usize {
        match self {
            Self::Simple { agg_idx, .. } | Self::GroupConcat { agg_idx, .. } => *agg_idx,
        }
    }

    /// Returns `true` if this descriptor matches the given simple function call.
    fn matches_simple(&self, name: &str, args: &[Expr]) -> bool {
        match self {
            Self::Simple { name: n, arg, .. } => {
                if n != name {
                    return false;
                }
                match (arg, args.first()) {
                    // Both COUNT(*): arg = None, args is empty
                    (None, None) => args.is_empty(),
                    // Both have an argument — compare by col_idx if both are Column refs
                    (
                        Some(Expr::Column { col_idx: a, .. }),
                        Some(Expr::Column { col_idx: b, .. }),
                    ) => a == b,
                    _ => false,
                }
            }
            Self::GroupConcat { .. } => false,
        }
    }

    /// Returns `true` if this descriptor matches the given GROUP_CONCAT call.
    fn matches_group_concat(
        &self,
        gc_expr: &Expr,
        distinct: bool,
        order_by: &[(Expr, crate::ast::SortOrder)],
        separator: &str,
    ) -> bool {
        match self {
            Self::GroupConcat {
                expr,
                distinct: d,
                order_by: ob,
                separator: sep,
                ..
            } => {
                expr.as_ref() == gc_expr
                    && *d == distinct
                    && ob == order_by
                    && sep.as_str() == separator
            }
            Self::Simple { .. } => false,
        }
    }
}

/// Walks `expr` and registers any aggregate function calls into `result`.
fn collect_agg_exprs_from(expr: &Expr, result: &mut Vec<AggExpr>) {
    match expr {
        // GROUP_CONCAT: register as GroupConcat AggExpr and deduplicate.
        // Do NOT recurse into `gc_expr` itself (it IS the aggregate root).
        // Only recurse into ORDER BY sub-exprs (they could contain subqueries, etc.).
        Expr::GroupConcat {
            expr: gc_expr,
            distinct,
            order_by,
            separator,
        } => {
            let already = result
                .iter()
                .any(|ae| ae.matches_group_concat(gc_expr, *distinct, order_by, separator));
            if !already {
                let idx = result.len();
                result.push(AggExpr::GroupConcat {
                    expr: gc_expr.clone(),
                    distinct: *distinct,
                    order_by: order_by.clone(),
                    separator: separator.clone(),
                    agg_idx: idx,
                });
            }
            for (e, _) in order_by {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let arg = args.first().cloned();
            // Deduplicate: only add if not already registered.
            let already = result
                .iter()
                .any(|ae| ae.matches_simple(name.as_str(), args));
            if !already {
                let idx = result.len();
                result.push(AggExpr::Simple {
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
        Expr::Like { expr, pattern, .. } => {
            collect_agg_exprs_from(expr, result);
            collect_agg_exprs_from(pattern, result);
        }
        Expr::Literal(_) | Expr::Column { .. } | Expr::OuterColumn { .. } | Expr::Param { .. } => {}
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
    /// `GROUP_CONCAT(...)` — accumulates `(text_value, sort_key_values)` per row.
    GroupConcat {
        /// Accumulated rows: (coerced-to-text value, evaluated ORDER BY key values).
        rows: Vec<(String, Vec<Value>)>,
        /// Separator string placed between values in finalize.
        separator: String,
        /// Whether to deduplicate values before concatenating.
        distinct: bool,
        /// Sort directions: `true` = ASC, `false` = DESC. One per ORDER BY key.
        order_by_dirs: Vec<bool>,
    },
}

impl AggAccumulator {
    fn new(agg: &AggExpr) -> Self {
        match agg {
            AggExpr::GroupConcat {
                separator,
                distinct,
                order_by,
                ..
            } => Self::GroupConcat {
                rows: Vec::new(),
                separator: separator.clone(),
                distinct: *distinct,
                order_by_dirs: order_by
                    .iter()
                    .map(|(_, dir)| matches!(dir, crate::ast::SortOrder::Asc))
                    .collect(),
            },
            AggExpr::Simple { name, arg, .. } => match name.as_str() {
                "count" if arg.is_none() => Self::CountStar { n: 0 },
                "count" => Self::CountCol { n: 0 },
                "sum" => Self::Sum { acc: None },
                "min" => Self::Min { acc: None },
                "max" => Self::Max { acc: None },
                "avg" => Self::Avg {
                    sum: Value::Int(0),
                    count: 0,
                },
                _ => unreachable!("AggAccumulator::new called with non-aggregate"),
            },
        }
    }

    fn update(&mut self, row: &[Value], agg: &AggExpr) -> Result<(), DbError> {
        // Extract the argument expression from Simple aggregates.
        let simple_arg = match agg {
            AggExpr::Simple { arg, .. } => arg.as_ref(),
            AggExpr::GroupConcat { .. } => None,
        };

        match self {
            Self::CountStar { n } => *n += 1,

            Self::CountCol { n } => {
                let v = eval(simple_arg.unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *n += 1;
                }
            }

            Self::Sum { acc } => {
                let v = eval(simple_arg.unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v,
                        Some(a) => agg_add(a, v)?,
                    });
                }
            }

            Self::Min { acc } => {
                let v = eval(simple_arg.unwrap(), row)?;
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
                let v = eval(simple_arg.unwrap(), row)?;
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
                let v = eval(simple_arg.unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *sum = agg_add(sum.clone(), v)?;
                    *count += 1;
                }
            }

            Self::GroupConcat { rows, .. } => {
                // Extract the GROUP_CONCAT expression and ORDER BY from the AggExpr descriptor.
                let (gc_expr, gc_order_by) = match agg {
                    AggExpr::GroupConcat { expr, order_by, .. } => (expr.as_ref(), order_by),
                    _ => {
                        unreachable!("GroupConcat accumulator paired with non-GroupConcat AggExpr")
                    }
                };

                // Evaluate the concatenated expression; skip NULLs.
                let val = match eval(gc_expr, row)? {
                    Value::Null => return Ok(()),
                    v => value_to_display_string(v),
                };

                // Evaluate ORDER BY key expressions for this row.
                let keys: Vec<Value> = gc_order_by
                    .iter()
                    .map(|(e, _)| eval(e, row))
                    .collect::<Result<Vec<_>, _>>()?;

                rows.push((val, keys));
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
            Self::GroupConcat {
                mut rows,
                separator,
                distinct,
                order_by_dirs,
            } => {
                if rows.is_empty() {
                    return Ok(Value::Null);
                }

                // 1. Sort if ORDER BY keys are present.
                if !order_by_dirs.is_empty() {
                    rows.sort_by(|(_, keys_a), (_, keys_b)| {
                        for (i, &asc) in order_by_dirs.iter().enumerate() {
                            let a = keys_a.get(i).unwrap_or(&Value::Null);
                            let b = keys_b.get(i).unwrap_or(&Value::Null);
                            let cmp = compare_values_null_last_session(a, b);
                            let cmp = if asc { cmp } else { cmp.reverse() };
                            if cmp != std::cmp::Ordering::Equal {
                                return cmp;
                            }
                        }
                        std::cmp::Ordering::Equal
                    });
                }

                // 2. Deduplicate if DISTINCT (preserves sorted order).
                // Uses the session collation so that folded-equal strings
                // (e.g. "José" == "jose" under Es) are treated as duplicates.
                let values: Vec<&str> = if distinct {
                    use crate::eval::current_eval_collation;
                    use crate::text_semantics::canonical_text;
                    let coll = current_eval_collation();
                    let mut seen: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    rows.iter()
                        .filter(|(v, _)| seen.insert(canonical_text(coll, v.as_str()).into_owned()))
                        .map(|(v, _)| v.as_str())
                        .collect()
                } else {
                    rows.iter().map(|(v, _)| v.as_str()).collect()
                };

                // 3. Concatenate with separator; truncate at 1 MB (group_concat_max_len).
                const MAX_LEN: usize = 1_048_576;
                let mut result = String::new();
                for (i, val) in values.into_iter().enumerate() {
                    if i > 0 {
                        result.push_str(&separator);
                    }
                    result.push_str(val);
                    if result.len() >= MAX_LEN {
                        result.truncate(MAX_LEN);
                        break;
                    }
                }
                Ok(Value::Text(result))
            }
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

// ── GROUP_CONCAT helpers ──────────────────────────────────────────────────────

/// Converts a non-NULL `Value` to its text representation for GROUP_CONCAT.
///
/// Mirrors MySQL's `val_str()` coercion rules:
/// - `Text` → unchanged
/// - `Int`/`BigInt` → decimal representation
/// - `Real` → Rust default float formatting
/// - `Bool` → `"1"` (true) or `"0"` (false) — MySQL behavior
/// - Others → debug representation (fallback; should not occur in practice)
fn value_to_display_string(v: Value) -> String {
    match v {
        Value::Text(s) => s,
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => format!("{f}"),
        Value::Bool(b) => {
            if b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Value::Null => String::new(), // should not be reached (callers skip NULLs)
        other => format!("{other:?}"),
    }
}

/// Compares two `Value`s for ORDER BY inside GROUP_CONCAT.
///
/// Uses proper type-aware comparison:
/// - `NULL` sorts last (greater than any non-NULL), matching MySQL behavior.
/// - Numeric types compared numerically.
/// - `Text` compared lexicographically (not by length).
/// - Other types fall back to `value_to_key_bytes` for a stable total order.
fn compare_values_null_last(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        // Numeric types — proper numeric ordering.
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::Int(x), Value::BigInt(y)) => (*x as i64).cmp(y),
        (Value::BigInt(x), Value::Int(y)) => x.cmp(&(*y as i64)),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        // Text — lexicographic (not length-prefixed).
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        // All other types — stable fallback via key-bytes.
        _ => value_to_key_bytes(a).cmp(&value_to_key_bytes(b)),
    }
}

/// Session-aware version of [`compare_values_null_last`].
///
/// For `Text` values, uses the active thread-local session collation (set by
/// [`CollationGuard`]) instead of binary ordering. Used in GROUP_CONCAT ORDER BY.
fn compare_values_null_last_session(a: &Value, b: &Value) -> std::cmp::Ordering {
    use crate::eval::current_eval_collation;
    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::Int(x), Value::BigInt(y)) => (*x as i64).cmp(y),
        (Value::BigInt(x), Value::Int(y)) => x.cmp(&(*y as i64)),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Text(x), Value::Text(y)) => compare_text(current_eval_collation(), x, y),
        _ => value_to_key_bytes(a).cmp(&value_to_key_bytes(b)),
    }
}

/// Session-aware serialization for GROUP BY hash keys and DISTINCT deduplication.
///
/// For `Text` values, uses the canonical fold from the active thread-local
/// session collation so that `jose` and `José` map to the same group key under `Es`.
/// All non-text types use the binary serialization unchanged.
fn value_to_session_key_bytes(v: &Value) -> Vec<u8> {
    use crate::eval::current_eval_collation;
    use crate::text_semantics::canonical_text;
    let coll = current_eval_collation();
    if coll == SessionCollation::Binary {
        return value_to_key_bytes(v);
    }
    let mut buf = Vec::new();
    match v {
        Value::Text(s) => {
            let key = canonical_text(coll, s);
            buf.push(0x06);
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(key.as_bytes());
        }
        other => return value_to_key_bytes(other),
    }
    buf
}

/// Session-aware DISTINCT deduplication.
///
/// Uses [`value_to_session_key_bytes`] so that folded-equal text strings are
/// treated as duplicates under `Es` session collation.
fn apply_distinct_with_session(rows: Vec<Row>) -> Vec<Row> {
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|row| {
            let key: Vec<u8> = row.iter().flat_map(value_to_session_key_bytes).collect();
            seen.insert(key)
        })
        .collect()
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

/// Session-aware GROUP BY key serialization.
///
/// Uses [`value_to_session_key_bytes`] so that text values are canonicalized
/// according to the active session collation (e.g. `es` folds `José` = `jose`).
fn group_key_bytes_session(key_values: &[Value]) -> Vec<u8> {
    key_values
        .iter()
        .flat_map(value_to_session_key_bytes)
        .collect()
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

        // GROUP_CONCAT: look up the pre-computed finalized value by structural match.
        Expr::GroupConcat {
            expr: gc_expr,
            distinct,
            order_by,
            separator,
        } => {
            let idx = agg_exprs
                .iter()
                .position(|ae| ae.matches_group_concat(gc_expr, *distinct, order_by, separator))
                .ok_or_else(|| {
                    DbError::Other("GROUP_CONCAT not pre-registered — internal error".to_string())
                })?;
            Ok(agg_values[idx].clone())
        }

        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let idx = agg_exprs
                .iter()
                .position(|ae| ae.matches_simple(name.as_str(), args))
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

        // Compound predicates that may contain aggregates in sub-expressions.
        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            let p = eval_with_aggs(pattern, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::Like {
                    expr: Box::new(Expr::Literal(v)),
                    pattern: Box::new(Expr::Literal(p)),
                    negated: *negated,
                },
                &[],
            )
        }

        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            let lo = eval_with_aggs(low, representative_row, agg_values, agg_exprs)?;
            let hi = eval_with_aggs(high, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::Between {
                    expr: Box::new(Expr::Literal(v)),
                    low: Box::new(Expr::Literal(lo)),
                    high: Box::new(Expr::Literal(hi)),
                    negated: *negated,
                },
                &[],
            )
        }

        Expr::In {
            expr,
            list,
            negated,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            let evaluated_list: Result<Vec<Expr>, _> = list
                .iter()
                .map(|e| {
                    eval_with_aggs(e, representative_row, agg_values, agg_exprs).map(Expr::Literal)
                })
                .collect();
            eval(
                &Expr::In {
                    expr: Box::new(Expr::Literal(v)),
                    list: evaluated_list?,
                    negated: *negated,
                },
                &[],
            )
        }

        Expr::Cast { expr, target } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::Cast {
                    expr: Box::new(Expr::Literal(v)),
                    target: *target,
                },
                &[],
            )
        }

        // For remaining variants: fall back to standard eval against representative_row.
        other => eval(other, representative_row),
    }
}

// ── execute_select_grouped ────────────────────────────────────────────────────

// ── GROUP BY strategy ────────────────────────────────────────────────────────

/// Controls which GROUP BY execution algorithm is used.
#[derive(Debug, Clone, Copy)]
enum GroupByStrategy {
    /// Default: one-pass hash aggregation (always correct, no ordering required).
    Hash,
    /// Stream adjacent equal groups from an already-ordered input.
    ///
    /// `presorted = true`  → caller guarantees input is in group-key order.
    /// `presorted = false` → executor sorts the input by group keys first.
    Sorted { presorted: bool },
}

/// Collation-aware GROUP BY strategy selection.
///
/// When the effective session collation is non-binary AND any GROUP BY expression
/// references a TEXT column, the presorted strategy must be rejected because the
/// index uses binary key order while the session uses a different text ordering.
///
/// `columns` should be the resolved columns of the FROM table; pass `&[]` when
/// they are unavailable (conservative: binary GROUP BY path is still available).
fn choose_group_by_strategy_ctx_with_collation(
    group_by: &[Expr],
    access_method: &crate::planner::AccessMethod,
    collation: SessionCollation,
    columns: &[axiomdb_catalog::schema::ColumnDef],
) -> GroupByStrategy {
    if group_by.is_empty() {
        return GroupByStrategy::Hash;
    }

    // Safety gate: if collation is non-binary and any GROUP BY key is a TEXT
    // column, the index-ordered GROUP BY would produce wrong groupings.
    if collation != SessionCollation::Binary && !columns.is_empty() {
        let has_text_key = group_by.iter().any(|expr| {
            if let Expr::Column { col_idx, .. } = expr {
                columns
                    .get(*col_idx)
                    .map(|col| col.col_type == axiomdb_catalog::schema::ColumnType::Text)
                    .unwrap_or(false)
            } else {
                false
            }
        });
        if has_text_key {
            return GroupByStrategy::Hash;
        }
    }

    let index_def = match access_method {
        crate::planner::AccessMethod::IndexLookup { index_def, .. }
        | crate::planner::AccessMethod::IndexRange { index_def, .. }
        | crate::planner::AccessMethod::IndexOnlyScan { index_def, .. } => index_def,
        crate::planner::AccessMethod::Scan => return GroupByStrategy::Hash,
    };

    if group_by_matches_index_prefix(group_by, index_def) {
        GroupByStrategy::Sorted { presorted: true }
    } else {
        GroupByStrategy::Hash
    }
}

/// Returns `true` iff every element of `group_by` is a plain `Expr::Column`
/// whose `col_idx` matches the corresponding leading column of `index_def`,
/// in the same order, without gaps.
fn group_by_matches_index_prefix(group_by: &[Expr], index_def: &IndexDef) -> bool {
    if group_by.len() > index_def.columns.len() {
        return false;
    }
    for (gb_expr, idx_col) in group_by.iter().zip(&index_def.columns) {
        match gb_expr {
            Expr::Column { col_idx, .. } => {
                if *col_idx as u16 != idx_col.col_idx {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Compare two group-key value lists lexicographically, NULL last.
fn compare_group_key_lists(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = compare_values_null_last(x, y);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// Returns `true` iff `a` and `b` are considered the same GROUP BY group.
///
/// NULL == NULL for grouping purposes (matches SQL GROUP BY semantics).
fn group_keys_equal(a: &[Value], b: &[Value]) -> bool {
    compare_group_key_lists(a, b) == std::cmp::Ordering::Equal
}

// ── Grouped executor entry point ─────────────────────────────────────────────

/// Executes the GROUP BY + aggregation path.
///
/// `combined_rows` are the post-scan, post-WHERE rows (not yet projected).
/// `strategy` controls whether hash or sorted streaming aggregation is used.
fn execute_select_grouped(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
    strategy: GroupByStrategy,
) -> Result<QueryResult, DbError> {
    match strategy {
        GroupByStrategy::Hash => execute_select_grouped_hash(stmt, combined_rows),
        GroupByStrategy::Sorted { presorted } => {
            execute_select_grouped_sorted(stmt, combined_rows, presorted)
        }
    }
}

// ── Hash aggregation (original 4.9a implementation) ──────────────────────────

fn execute_select_grouped_hash(
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

        // Session-aware: folds text under Es so "José" and "jose" share a group.
        let key_bytes = group_key_bytes_session(&key_values);

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
        let agg_values: Vec<Value> = state
            .accumulators
            .into_iter()
            .map(|acc| acc.finalize())
            .collect::<Result<_, _>>()?;

        if let Some(ref having) = stmt.having {
            let v = eval_with_aggs(having, &state.representative_row, &agg_values, &agg_exprs)?;
            if !is_truthy(&v) {
                continue;
            }
        }

        let out_row = project_grouped_row(
            &stmt.columns,
            &state.representative_row,
            &agg_values,
            &agg_exprs,
        )?;
        rows.push(out_row);
    }

    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    let remapped_ob = remap_order_by_for_grouped(&stmt.order_by, &stmt.columns);
    rows = apply_order_by(rows, &remapped_ob)?;
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

// ── Sorted streaming aggregation (4.9b) ──────────────────────────────────────

/// Sorted streaming GROUP BY.
///
/// When `presorted = true`, input rows are already in group-key order
/// (guaranteed by the B-Tree access method). Groups are formed by streaming
/// adjacent equal-key rows without building any hash table.
///
/// When `presorted = false`, the input is sorted by group keys first, then
/// streamed. This path is not auto-selected in 4.9b but is available for
/// testing and future use.
fn execute_select_grouped_sorted(
    stmt: SelectStmt,
    mut combined_rows: Vec<Row>,
    presorted: bool,
) -> Result<QueryResult, DbError> {
    let agg_exprs = collect_agg_exprs(&stmt.columns, &stmt.having);
    let out_cols = build_grouped_column_meta(&stmt.columns, &agg_exprs)?;

    // Evaluate GROUP BY expressions for every row up front.
    // This avoids re-evaluating the same expressions during boundary detection.
    struct KeyedRow {
        row: Row,
        key_values: Vec<Value>,
    }
    let mut keyed: Vec<KeyedRow> = combined_rows
        .drain(..)
        .map(|row| {
            let key_values: Vec<Value> = stmt
                .group_by
                .iter()
                .map(|e| eval(e, &row))
                .collect::<Result<_, _>>()?;
            Ok(KeyedRow { row, key_values })
        })
        .collect::<Result<Vec<_>, DbError>>()?;

    if !presorted {
        // Stable sort by group keys — NULL last, same as hash path output order.
        keyed.sort_by(|a, b| compare_group_key_lists(&a.key_values, &b.key_values));
    }

    // Stream adjacent equal groups.
    let mut output_rows: Vec<Row> = Vec::new();

    if keyed.is_empty() {
        // Ungrouped aggregate on empty input: emit one row (e.g., COUNT(*) → 0).
        if stmt.group_by.is_empty() {
            let accumulators: Vec<AggAccumulator> =
                agg_exprs.iter().map(AggAccumulator::new).collect();
            let agg_values: Vec<Value> = accumulators
                .into_iter()
                .map(|acc| acc.finalize())
                .collect::<Result<_, _>>()?;
            let out_row = project_grouped_row(&stmt.columns, &[], &agg_values, &agg_exprs)?;
            output_rows.push(out_row);
        }
    } else {
        // Initialize first group.
        let first = &keyed[0];
        let mut current_key = first.key_values.clone();
        let mut representative_row = first.row.clone();
        let mut accumulators: Vec<AggAccumulator> =
            agg_exprs.iter().map(AggAccumulator::new).collect();
        for (acc, agg) in accumulators.iter_mut().zip(&agg_exprs) {
            acc.update(&first.row, agg)?;
        }

        for kr in &keyed[1..] {
            if group_keys_equal(&current_key, &kr.key_values) {
                // Same group — accumulate.
                for (acc, agg) in accumulators.iter_mut().zip(&agg_exprs) {
                    acc.update(&kr.row, agg)?;
                }
            } else {
                // Group boundary — drain current accumulators by value, finalize, emit.
                let finished: Vec<AggAccumulator> = std::mem::replace(
                    &mut accumulators,
                    agg_exprs.iter().map(AggAccumulator::new).collect(),
                );
                let agg_values: Vec<Value> = finished
                    .into_iter()
                    .map(|acc| acc.finalize())
                    .collect::<Result<_, _>>()?;
                if let Some(ref having) = stmt.having {
                    let v = eval_with_aggs(having, &representative_row, &agg_values, &agg_exprs)?;
                    if is_truthy(&v) {
                        let out_row = project_grouped_row(
                            &stmt.columns,
                            &representative_row,
                            &agg_values,
                            &agg_exprs,
                        )?;
                        output_rows.push(out_row);
                    }
                } else {
                    let out_row = project_grouped_row(
                        &stmt.columns,
                        &representative_row,
                        &agg_values,
                        &agg_exprs,
                    )?;
                    output_rows.push(out_row);
                }

                // Start next group (accumulators already reset by mem::replace above).
                current_key = kr.key_values.clone();
                representative_row = kr.row.clone();
                for (acc, agg) in accumulators.iter_mut().zip(&agg_exprs) {
                    acc.update(&kr.row, agg)?;
                }
            }
        }

        // Finalize the last group.
        let agg_values: Vec<Value> = accumulators
            .into_iter()
            .map(|acc| acc.finalize())
            .collect::<Result<_, _>>()?;
        if let Some(ref having) = stmt.having {
            let v = eval_with_aggs(having, &representative_row, &agg_values, &agg_exprs)?;
            if is_truthy(&v) {
                let out_row = project_grouped_row(
                    &stmt.columns,
                    &representative_row,
                    &agg_values,
                    &agg_exprs,
                )?;
                output_rows.push(out_row);
            }
        } else {
            let out_row =
                project_grouped_row(&stmt.columns, &representative_row, &agg_values, &agg_exprs)?;
            output_rows.push(out_row);
        }
    }

    let mut rows = output_rows;
    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    let remapped_ob = remap_order_by_for_grouped(&stmt.order_by, &stmt.columns);
    rows = apply_order_by(rows, &remapped_ob)?;
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
        Expr::GroupConcat { .. } => "GROUP_CONCAT(...)".into(),
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
        // GROUP_CONCAT always produces TEXT; nullable (empty group → NULL).
        Expr::GroupConcat { .. } => (DataType::Text, true),
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
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    // No-op bloom for the non-ctx path (bloom is managed by execute_with_ctx callers).
    let mut noop_bloom = crate::bloom::BloomRegistry::new();

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

    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, schema_cols)?;

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
                        &mut noop_bloom,
                        &compiled_preds,
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
                        &mut noop_bloom,
                        &compiled_preds,
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
                        &mut noop_bloom,
                        &compiled_preds,
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
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    // No-op bloom for the non-ctx path (bloom is managed by execute_with_ctx callers).
    let mut noop_bloom = crate::bloom::BloomRegistry::new();

    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, &schema_cols)?;

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
                rid,
                storage,
                &mut noop_bloom,
                &compiled_preds,
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
                &mut noop_bloom,
                &compiled_preds,
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

// ── DELETE candidate discovery (Phase 6.3b) ──────────────────────────────────

/// Discovers and materializes `(RecordId, row_values)` pairs for a
/// `DELETE ... WHERE` statement using the best available access method.
///
/// ## Guarantee
///
/// 1. B-Tree state is never mutated while candidates are being collected.
/// 2. The full original `where_clause` is always rechecked on fetched row values
///    before a row is included in the result set, regardless of which index path
///    was used to find it.
/// 3. Only rows visible to `snap` are returned.
///
/// ## Access path selection
///
/// - Indexed (`IndexLookup` / `IndexRange`): B-Tree lookup or range → RIDs →
///   heap reads → full `WHERE` recheck.
/// - `Scan`: full heap scan via `TableEngine::scan_table` + `WHERE` filter
///   (existing behavior, unchanged).
fn collect_delete_candidates(
    where_clause: &Expr,
    indexes: &[axiomdb_catalog::IndexDef],
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    access: &crate::planner::AccessMethod,
    storage: &mut dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    table_def: &axiomdb_catalog::TableDef,
    bloom: &crate::bloom::BloomRegistry,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    use crate::planner::AccessMethod;

    match access {
        AccessMethod::Scan | AccessMethod::IndexOnlyScan { .. } => {
            // Full heap scan — existing behavior.
            let rows = TableEngine::scan_table(storage, table_def, schema_cols, snap, None)?;
            rows.into_iter()
                .filter_map(|(rid, values)| match eval(where_clause, &values) {
                    Ok(v) if is_truthy(&v) => Some(Ok((rid, values))),
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect::<Result<_, DbError>>()
        }

        AccessMethod::IndexLookup { index_def, key } => {
            // Point lookup via B-Tree → heap read → WHERE recheck.
            let candidate_rids: Vec<RecordId> = if index_def.is_unique {
                if index_def.is_unique && !bloom.might_exist(index_def.index_id, key) {
                    vec![]
                } else {
                    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                        None => vec![],
                        Some(rid) => vec![rid],
                    }
                }
            } else {
                // Non-unique: key||RID format — range [key||0..0, key||FF..FF].
                let lo = rid_lo(key);
                let hi = rid_hi(key);
                BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?
                    .into_iter()
                    .map(|(rid, _)| rid)
                    .collect()
            };

            let mut result = Vec::with_capacity(candidate_rids.len());
            for rid in candidate_rids {
                if let Some(values) = TableEngine::read_row(storage, schema_cols, rid)? {
                    // Recheck full WHERE — the index only narrowed candidates.
                    if is_truthy(&eval(where_clause, &values)?) {
                        result.push((rid, values));
                    }
                }
            }
            Ok(result)
        }

        AccessMethod::IndexRange { index_def, lo, hi } => {
            // Range scan via B-Tree → heap reads → WHERE recheck.
            let (lo_adj, hi_adj);
            let (lo_ref, hi_ref) = if index_def.is_unique {
                (lo.as_deref(), hi.as_deref())
            } else {
                lo_adj = lo.as_deref().map(rid_lo);
                hi_adj = hi.as_deref().map(rid_hi);
                (lo_adj.as_deref(), hi_adj.as_deref())
            };
            let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;

            let mut result = Vec::with_capacity(pairs.len());
            for (rid, _key) in pairs {
                if let Some(values) = TableEngine::read_row(storage, schema_cols, rid)? {
                    if is_truthy(&eval(where_clause, &values)?) {
                        result.push((rid, values));
                    }
                }
            }
            Ok(result)
        }
    }
}

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
    // Must be `mut` so we can keep root_page_id in sync as rows are deleted.
    let mut secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    // No-op bloom for the non-ctx path (bloom is managed by execute_with_ctx callers).
    let mut noop_bloom = crate::bloom::BloomRegistry::new();

    // Check if any FK constraint references THIS table as the parent.
    // If so, fall through to the row-by-row path so RESTRICT/CASCADE still fires.
    let has_fk_references = {
        let mut reader = CatalogReader::new(storage, snap)?;
        !reader
            .list_fk_constraints_referencing(resolved.def.id)?
            .is_empty()
    };

    // No-WHERE + no parent-FK references → bulk-empty fast path (Phase 5.16).
    if stmt.where_clause.is_none() && !has_fk_references {
        let plan = plan_bulk_empty_table(storage, &resolved.def, &secondary_indexes, snap)?;
        let count = plan.visible_row_count;
        apply_bulk_empty_table(storage, txn, &mut noop_bloom, &resolved.def, plan)?;
        return Ok(QueryResult::Affected {
            count,
            last_insert_id: None,
        });
    }

    // Candidate discovery (Phase 6.3b): index path when predicate is sargable.
    let schema_cols = resolved.columns.clone();
    let to_delete: Vec<(RecordId, Vec<Value>)> = if let Some(ref wc) = stmt.where_clause {
        let delete_access =
            crate::planner::plan_delete_candidates(wc, &secondary_indexes, &schema_cols);
        collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &delete_access,
            storage,
            snap,
            &resolved.def,
            &noop_bloom,
        )?
    } else {
        // No WHERE + has_fk_references=true — full scan, all rows qualify.
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?
    };

    // Batch-delete from heap: each page read+written once instead of 3× per row.
    let rids_only: Vec<RecordId> = to_delete.iter().map(|(rid, _)| *rid).collect();
    let count = TableEngine::delete_rows_batch(storage, txn, &resolved.def, &rids_only)?;

    // Index maintenance: still per-row (each B+Tree remove is its own traversal),
    // but heap I/O is now fully batched above.
    if !secondary_indexes.is_empty() {
        let compiled_preds =
            crate::partial_index::compile_index_predicates(&secondary_indexes, &schema_cols)?;
        for (rid, row_vals) in &to_delete {
            let updated = crate::index_maintenance::delete_from_indexes(
                &secondary_indexes,
                row_vals,
                *rid,
                storage,
                &mut noop_bloom,
                &compiled_preds,
            )?;
            for (index_id, new_root) in updated {
                CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                // Keep the in-memory snapshot in sync so the next row's deletion
                // starts from the correct (current) root. Without this, a root
                // collapse on row N causes row N+1 to start from a freed page.
                for idx in secondary_indexes.iter_mut() {
                    if idx.index_id == index_id {
                        idx.root_page_id = new_root;
                        break;
                    }
                }
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

    // Collect inline REFERENCES constraints for processing after all columns are created.
    // We must create all columns first so col_idx values are stable.
    let mut inline_fk_specs: Vec<InlineFkSpec> = Vec::new();

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
        // Also detect inline REFERENCES constraints — collect for processing below.
        if let Some(refs) = col_def.constraints.iter().find_map(|c| {
            if let ColumnConstraint::References {
                table,
                column,
                on_delete,
                on_update,
            } = c
            {
                Some((table.clone(), column.clone(), *on_delete, *on_update))
            } else {
                None
            }
        }) {
            inline_fk_specs.push((i as u16, col_def.name.clone(), refs));
        }

        writer.create_column(CatalogColumnDef {
            table_id,
            col_idx: i as u16,
            name: col_def.name.clone(),
            col_type,
            nullable,
            auto_increment,
        })?;
    }

    // Create B-Tree indexes for PRIMARY KEY and UNIQUE column constraints.
    //
    // `CREATE TABLE t (id INT PRIMARY KEY)` must create a unique B-Tree index on
    // `id` so that:
    // (a) the planner can use it for O(log n) point lookups, and
    // (b) FK validation in `persist_fk_constraint` can verify parent key existence.
    //
    // Since the table was just created (empty heap), index build is trivial.
    {
        use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
        use std::sync::atomic::{AtomicU64, Ordering};

        let mut pk_col: Option<(u16, String)> = None; // (col_idx, col_name) for PK
        let mut unique_cols: Vec<(u16, String)> = Vec::new(); // (col_idx, col_name) for UNIQUE

        for (i, col_def) in stmt.columns.iter().enumerate() {
            for constraint in &col_def.constraints {
                match constraint {
                    ColumnConstraint::PrimaryKey => {
                        pk_col = Some((i as u16, col_def.name.clone()));
                    }
                    crate::ast::ColumnConstraint::Unique => {
                        unique_cols.push((i as u16, col_def.name.clone()));
                    }
                    _ => {}
                }
            }
        }
        // Also check table-level PRIMARY KEY and UNIQUE constraints.
        for tc in &stmt.table_constraints {
            match tc {
                crate::ast::TableConstraint::PrimaryKey { columns, .. } => {
                    if columns.len() == 1 {
                        let snap = txn.active_snapshot()?;
                        let col_idx = {
                            let mut reader = CatalogReader::new(storage, snap)?;
                            let cols = reader.list_columns(table_id)?;
                            cols.iter()
                                .find(|c| c.name == columns[0])
                                .map(|c| c.col_idx)
                        };
                        if let Some(idx) = col_idx {
                            pk_col = Some((idx, columns[0].clone()));
                        }
                    }
                }
                crate::ast::TableConstraint::Unique { columns, .. } => {
                    if columns.len() == 1 {
                        let snap = txn.active_snapshot()?;
                        let col_idx = {
                            let mut reader = CatalogReader::new(storage, snap)?;
                            let cols = reader.list_columns(table_id)?;
                            cols.iter()
                                .find(|c| c.name == columns[0])
                                .map(|c| c.col_idx)
                        };
                        if let Some(idx) = col_idx {
                            unique_cols.push((idx, columns[0].clone()));
                        }
                    }
                }
                _ => {}
            }
        }

        // Helper: create a single-column B-Tree index on an empty table.
        let create_empty_index = |col_idx: u16,
                                  index_name: String,
                                  is_unique: bool,
                                  is_primary: bool,
                                  storage: &mut dyn StorageEngine,
                                  txn: &mut TxnManager|
         -> Result<u32, DbError> {
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
            let final_root = AtomicU64::new(root_page_id).load(Ordering::Acquire);
            let idx_id = CatalogWriter::new(storage, txn)?.create_index(IndexDef {
                index_id: 0,
                table_id,
                name: index_name,
                root_page_id: final_root,
                is_unique,
                fillfactor: 90, // auto-created indexes use default
                is_primary,
                columns: vec![IndexColumnDef {
                    col_idx,
                    order: CatalogSortOrder::Asc,
                }],
                predicate: None,
                is_fk_index: false,
                include_columns: vec![],
            })?;
            Ok(idx_id)
        };

        // Create PRIMARY KEY index.
        if let Some((col_idx, col_name)) = pk_col {
            let idx_name = format!("{}_pkey", stmt.table.name);
            let idx_id = create_empty_index(col_idx, idx_name, true, true, storage, txn)?;
            // Populate bloom for the new PK index (table is empty, so no keys to add).
            // bloom is not available here (non-ctx path), handled lazily.
            let _ = idx_id;
            let _ = col_name;
        }

        // Create UNIQUE indexes.
        for (col_idx, col_name) in unique_cols {
            let idx_name = format!("{}_{}_unique", stmt.table.name, col_name);
            let idx_id = create_empty_index(col_idx, idx_name, true, false, storage, txn)?;
            let _ = idx_id;
        }
    }

    // Process FK constraints collected from inline column definitions.
    for (child_col_idx, child_col_name, (ref_table, ref_col, on_delete, on_update)) in
        inline_fk_specs
    {
        persist_fk_constraint(
            table_id,
            &stmt.table.name,
            child_col_idx,
            &child_col_name,
            &ref_table,
            ref_col.as_deref(),
            ast_fk_action_to_catalog(on_delete),
            ast_fk_action_to_catalog(on_update),
            None, // auto-name
            storage,
            txn,
        )?;
    }

    // Process FK constraints from table-level FOREIGN KEY declarations.
    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::ForeignKey {
            name,
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
        } = tc
        {
            if columns.len() != 1 {
                return Err(DbError::NotImplemented {
                    feature: "composite foreign key (multiple columns) — Phase 6.9".into(),
                });
            }
            let child_col_name = &columns[0];
            // Find col_idx for the FK column.
            let snap = txn.active_snapshot()?;
            let child_col_idx = {
                let mut reader = CatalogReader::new(storage, snap)?;
                let cols = reader.list_columns(table_id)?;
                cols.iter()
                    .find(|c| &c.name == child_col_name)
                    .map(|c| c.col_idx)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: child_col_name.clone(),
                        table: stmt.table.name.clone(),
                    })?
            };
            let ref_col = ref_columns.first().map(|s| s.as_str());
            persist_fk_constraint(
                table_id,
                &stmt.table.name,
                child_col_idx,
                child_col_name,
                ref_table,
                ref_col,
                ast_fk_action_to_catalog(*on_delete),
                ast_fk_action_to_catalog(*on_update),
                name.as_deref(),
                storage,
                txn,
            )?;
        }
    }

    Ok(QueryResult::Empty)
}

// ── FK helpers ────────────────────────────────────────────────────────────────

/// Converts an AST [`ForeignKeyAction`] to the catalog [`FkAction`] used in `FkDef`.
fn ast_fk_action_to_catalog(action: crate::ast::ForeignKeyAction) -> axiomdb_catalog::FkAction {
    use crate::ast::ForeignKeyAction;
    use axiomdb_catalog::FkAction;
    match action {
        ForeignKeyAction::NoAction => FkAction::NoAction,
        ForeignKeyAction::Restrict => FkAction::Restrict,
        ForeignKeyAction::Cascade => FkAction::Cascade,
        ForeignKeyAction::SetNull => FkAction::SetNull,
        ForeignKeyAction::SetDefault => FkAction::SetDefault,
    }
}

/// Validates and persists a single FK constraint definition.
///
/// Called from `execute_create_table` (inline `REFERENCES` and table-level
/// `FOREIGN KEY`) and from `alter_add_constraint`.
///
/// # Steps
/// 1. Resolve parent table and referenced column (defaults to PK if unspecified).
/// 2. Verify parent column has a PRIMARY KEY or UNIQUE index.
/// 3. Auto-generate constraint name if not provided.
/// 4. Check uniqueness of constraint name on this child table.
/// 5. Create an index on the FK column in the child table if none exists.
/// 6. Persist `FkDef` in `axiom_foreign_keys`.
#[allow(clippy::too_many_arguments)]
fn persist_fk_constraint(
    child_table_id: u32,
    child_table_name: &str,
    child_col_idx: u16,
    child_col_name: &str,
    ref_table: &str,
    ref_col: Option<&str>,
    on_delete: axiomdb_catalog::FkAction,
    on_update: axiomdb_catalog::FkAction,
    fk_name: Option<&str>,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<(), DbError> {
    use axiomdb_catalog::FkDef;

    let snap = txn.active_snapshot()?;

    // 1. Resolve parent table.
    let parent_def = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader
            .get_table("public", ref_table)?
            .ok_or_else(|| DbError::TableNotFound {
                name: ref_table.to_string(),
            })?
    };

    // 2. Find the referenced column in the parent table.
    let parent_cols = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.list_columns(parent_def.id)?
    };
    let parent_col_idx: u16 = if let Some(col_name) = ref_col {
        parent_cols
            .iter()
            .find(|c| c.name == col_name)
            .map(|c| c.col_idx)
            .ok_or_else(|| DbError::ColumnNotFound {
                name: col_name.to_string(),
                table: ref_table.to_string(),
            })?
    } else {
        // Default: use the leading column of the primary key index.
        let parent_indexes = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader.list_indexes(parent_def.id)?
        };
        let pk_idx = parent_indexes
            .iter()
            .find(|i| i.is_primary && !i.columns.is_empty())
            .ok_or_else(|| DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: "<primary key>".to_string(),
            })?;
        pk_idx.columns[0].col_idx
    };

    // 3. Verify the parent column has a PRIMARY KEY or UNIQUE index covering it.
    {
        let mut reader = CatalogReader::new(storage, snap)?;
        let parent_indexes = reader.list_indexes(parent_def.id)?;
        let has_unique = parent_indexes.iter().any(|i| {
            (i.is_primary || i.is_unique)
                && i.columns.len() == 1
                && i.columns[0].col_idx == parent_col_idx
        });
        if !has_unique {
            let col_name = parent_cols
                .iter()
                .find(|c| c.col_idx == parent_col_idx)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("col_{parent_col_idx}"));
            return Err(DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: col_name,
            });
        }
    }

    // 4. Auto-generate FK name if not provided.
    let constraint_name: String = fk_name
        .map(|n| n.to_string())
        .unwrap_or_else(|| format!("fk_{child_table_name}_{child_col_name}_{ref_table}"));

    // 5. Check FK name uniqueness on this child table.
    {
        let mut reader = CatalogReader::new(storage, snap)?;
        if reader
            .get_fk_by_name(child_table_id, &constraint_name)?
            .is_some()
        {
            return Err(DbError::Other(format!(
                "foreign key constraint '{constraint_name}' already exists on table \
                 '{child_table_name}'"
            )));
        }
    }

    // 6. FK auto-index on child table (Phase 6.9).
    use axiomdb_catalog::{IndexColumnDef as CatIndexColumnDef, SortOrder as CatSortOrder};
    //
    // Uses composite keys: encode_index_key(&[fk_val]) ++ encode_rid(rid) (10 bytes).
    // Every entry is globally unique even when multiple rows share the same FK value —
    // the InnoDB approach (appending PK as tiebreaker). This enables O(log n)
    // range scans for RESTRICT/CASCADE/SET NULL enforcement.
    let fk_index_id: u32 = {
        use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
        use std::sync::atomic::{AtomicU64, Ordering};

        // Check if child already has a suitable covering index on child_col_idx
        // (user-provided, not an FK auto-index).
        let existing_covers = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader.list_indexes(child_table_id)?.into_iter().any(|i| {
                !i.is_fk_index && !i.columns.is_empty() && i.columns[0].col_idx == child_col_idx
            })
        };

        if existing_covers {
            0 // reuse existing user-provided index; will not be dropped with FK
        } else {
            // Build FK auto-index with composite keys from existing child rows.
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

            let child_table_def = {
                let mut reader = CatalogReader::new(storage, snap)?;
                reader
                    .get_table_by_id(child_table_id)?
                    .ok_or(DbError::CatalogTableNotFound {
                        table_id: child_table_id,
                    })?
            };
            let child_cols = {
                let mut reader = CatalogReader::new(storage, snap)?;
                reader.list_columns(child_table_id)?
            };

            // Insert composite key entry for every existing child row.
            let rows = TableEngine::scan_table(storage, &child_table_def, &child_cols, snap, None)?;
            for (rid, row_vals) in rows {
                let fk_val = row_vals.get(child_col_idx as usize).unwrap_or(&Value::Null);
                if matches!(fk_val, Value::Null) {
                    continue;
                }
                if let Ok(key) = crate::index_maintenance::fk_composite_key(fk_val, rid) {
                    BTree::insert_in(storage, &root_pid, &key, rid, 90)?;
                }
            }

            let final_root = root_pid.load(Ordering::Acquire);
            let new_idx_id = CatalogWriter::new(storage, txn)?.create_index(IndexDef {
                index_id: 0,
                table_id: child_table_id,
                name: format!("_fk_{constraint_name}"),
                root_page_id: final_root,
                is_unique: false,
                is_primary: false,
                is_fk_index: true, // marks composite-key FK auto-index
                columns: vec![CatIndexColumnDef {
                    col_idx: child_col_idx,
                    order: CatSortOrder::Asc,
                }],
                predicate: None,
                fillfactor: 90,
                include_columns: vec![],
            })?;
            new_idx_id
        }
    };

    // 7. Persist FkDef in axiom_foreign_keys.
    CatalogWriter::new(storage, txn)?.create_foreign_key(FkDef {
        fk_id: 0, // allocated by CatalogWriter::create_foreign_key
        child_table_id,
        child_col_idx,
        parent_table_id: parent_def.id,
        parent_col_idx,
        on_delete,
        on_update,
        fk_index_id,
        name: constraint_name,
    })?;

    Ok(())
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
    bloom: &mut crate::bloom::BloomRegistry,
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
    //    For partial indexes, compile the predicate once and skip non-matching rows.
    let index_fillfactor = stmt.fillfactor.unwrap_or(90);
    let pred_expr: Option<crate::expr::Expr> = match &stmt.predicate {
        Some(pred) => {
            let sql = expr_to_sql_string(pred);
            Some(crate::partial_index::compile_predicate_sql(
                &sql, &col_defs,
            )?)
        }
        None => None,
    };

    let rows = TableEngine::scan_table(storage, &table_def, &col_defs, snap, None)?;
    let mut skipped = 0usize;
    let mut bloom_keys: Vec<Vec<u8>> = Vec::new();
    for (rid, row_vals) in &rows {
        let (rid, row_vals) = (*rid, row_vals);
        // Partial index: skip rows that don't satisfy the predicate.
        if let Some(pred) = &pred_expr {
            if !crate::eval::is_truthy(&crate::eval::eval(pred, row_vals)?) {
                continue;
            }
        }

        let key_vals: Vec<Value> = index_columns
            .iter()
            .map(|ic| row_vals[ic.col_idx as usize].clone())
            .collect();
        // Skip rows with NULL key values — NULLs are not indexed.
        if key_vals.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        match encode_index_key(&key_vals) {
            Ok(base_key) => {
                // Non-unique indexes append the RecordId so that multiple rows with
                // the same indexed value each get a unique B-Tree key (InnoDB approach).
                let key = if !stmt.unique {
                    let mut k = base_key;
                    k.extend_from_slice(&encode_rid(rid));
                    k
                } else {
                    base_key
                };
                BTree::insert_in(storage, &root_pid, &key, rid, index_fillfactor)?;
                bloom_keys.push(key);
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
    // Serialize the predicate expression to SQL string for catalog storage.
    // Stored as a human-readable string for debuggability and backward-compat.
    let predicate_sql: Option<String> = stmt.predicate.as_ref().map(expr_to_sql_string);

    // Resolve INCLUDE column names to col_idx values for catalog storage (Phase 6.13).
    let include_col_idxs: Vec<u16> = stmt
        .include_columns
        .iter()
        .filter_map(|name| col_defs.iter().find(|c| &c.name == name).map(|c| c.col_idx))
        .collect();

    let new_index_id = writer.create_index(IndexDef {
        index_id: 0, // allocated by CatalogWriter::create_index
        table_id: table_def.id,
        name: stmt.name.clone(),
        root_page_id: final_root,
        is_unique: stmt.unique,
        is_primary: false,
        columns: index_columns.clone(), // clone kept for stats bootstrap step 8
        predicate: predicate_sql,
        fillfactor: stmt.fillfactor.unwrap_or(90),
        is_fk_index: false, // user-created indexes are never FK auto-indexes
        include_columns: include_col_idxs,
    })?;

    // 7. Populate bloom filter for the newly created index.
    bloom.create(new_index_id, bloom_keys.len().max(1));
    for key in &bloom_keys {
        bloom.add(new_index_id, key);
    }

    // 8. Bootstrap per-column statistics (Phase 6.10).
    // Reuses the `rows` scan from step 5 — no extra I/O.
    for idx_col in &index_columns {
        let ndv = compute_ndv_exact(idx_col.col_idx, &rows);
        // Ignore stats write errors — stats are advisory, not correctness-critical.
        let _ = CatalogWriter::new(storage, txn)?.upsert_stats(axiomdb_catalog::StatsDef {
            table_id: table_def.id,
            col_idx: idx_col.col_idx,
            row_count: rows.len() as u64,
            ndv,
        });
    }

    Ok(QueryResult::Empty)
}

// ── Index-only scan helpers (Phase 6.13) ────────────────────────────────────

/// Collects the set of column indices (`col_idx`) needed in the SELECT output.
///
/// Returns an empty vec for `SELECT *` or when window functions / expressions
/// prevent precise tracking — in those cases, the planner conservatively
/// falls back to a regular index scan (never wrong, just no index-only optimization).
fn collect_select_col_idxs(stmt: &SelectStmt) -> Vec<u16> {
    let mut col_idxs = Vec::new();
    for item in &stmt.columns {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return vec![]; // wildcard → conservative, no index-only scan
            }
            SelectItem::Expr { expr, .. } => match expr {
                // Plain column reference: directly use its col_idx.
                Expr::Column { col_idx, .. } => {
                    col_idxs.push(*col_idx as u16);
                }
                // Any other expression (function call, literal, etc.) → conservative.
                _ => return vec![],
            },
        }
    }
    col_idxs
}

// ── NDV helper (Phase 6.10) ───────────────────────────────────────────────────

/// Computes the exact number of distinct non-NULL values for `col_idx` in `rows`.
///
/// Uses order-preserving encoded key bytes as the hash key so that the result
/// is consistent with the B-Tree key encoding (encode_index_key).
/// Phase 6.15 will add reservoir sampling (Duj1 estimator) for large tables.
fn compute_ndv_exact(col_idx: u16, rows: &[(RecordId, Vec<Value>)]) -> i64 {
    use crate::key_encoding::encode_index_key;
    use std::collections::HashSet;
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for (_, row) in rows {
        let val = row.get(col_idx as usize).unwrap_or(&Value::Null);
        if matches!(val, Value::Null) {
            continue; // NULLs are not indexed and don't count toward NDV
        }
        if let Ok(key) = encode_index_key(std::slice::from_ref(val)) {
            seen.insert(key);
        }
    }
    seen.len() as i64
}

// ── DROP INDEX ────────────────────────────────────────────────────────────────

fn execute_drop_index(
    stmt: DropIndexStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
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
            bloom.remove(id);
            // Then free all B-Tree pages to avoid leaks.
            if let Some(root) = root_page_id {
                free_btree_pages(storage, root)?;
            }
            Ok(QueryResult::Empty)
        }
    }
}

/// Drops an index by its catalog `index_id`, without requiring the index name.
///
/// Used by `alter_drop_constraint` to remove the auto-created FK index when a
/// FK constraint is dropped.
fn execute_drop_index_by_id(
    index_id: u32,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot()?;
    // Find the root page ID so we can free the B-Tree pages.
    let root_page_id = {
        // Scan all indexes looking for this index_id.
        // We scan axiom_indexes; CatalogReader::list_indexes requires a table_id,
        // so we use a raw catalog reader to get the TableDef first, but since we
        // only have index_id, we scan all tables. For Phase 6.5 this is acceptable
        // (index count is small). A direct get_index_by_id is deferred.
        // Scan axiom_indexes heap directly to find root by index_id (no table filter needed).
        let page_ids = axiomdb_catalog::bootstrap::CatalogBootstrap::page_ids(storage)?;
        let rows = axiomdb_storage::heap_chain::HeapChain::scan_visible_ro(
            storage,
            page_ids.indexes,
            snap,
        )?;
        let mut found_root = None;
        for (_, _, data) in rows {
            if let Ok((def, _)) = axiomdb_catalog::schema::IndexDef::from_bytes(&data) {
                if def.index_id == index_id {
                    found_root = Some(def.root_page_id);
                    break;
                }
            }
        }
        found_root
    };

    CatalogWriter::new(storage, txn)?.delete_index(index_id)?;
    bloom.remove(index_id);
    if let Some(root) = root_page_id {
        free_btree_pages(storage, root)?;
    }
    Ok(())
}

// ── Bulk table-empty machinery (Phase 5.16) ──────────────────────────────────

/// Everything needed to swap a table (and all its indexes) to empty roots.
struct BulkEmptyPlan {
    /// Rows visible to the statement snapshot — used as the DELETE row count.
    visible_row_count: u64,
    /// Freshly-allocated empty root page for the heap chain.
    new_data_root: u64,
    /// Freshly-allocated empty roots per index: `(index_id, new_root_page_id)`.
    new_index_roots: Vec<(u32, u64)>,
    /// All old pages to free AFTER commit durability is confirmed.
    old_pages_to_free: Vec<u64>,
}

/// Allocates a fresh empty heap-chain root page and returns its page_id.
fn alloc_empty_heap_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError> {
    let pid = storage.alloc_page(PageType::Data)?;
    let page = Page::new(PageType::Data, pid);
    storage.write_page(pid, &page)?;
    Ok(pid)
}

/// Allocates a fresh empty B-Tree leaf root page and returns its page_id.
fn alloc_empty_index_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError> {
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
    let pid = storage.alloc_page(PageType::Index)?;
    let mut page = Page::new(PageType::Index, pid);
    {
        let leaf = cast_leaf_mut(&mut page);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        leaf.set_next_leaf(NULL_PAGE);
    }
    page.update_checksum();
    storage.write_page(pid, &page)?;
    Ok(pid)
}

/// Collects all page_ids in a heap chain rooted at `root_page_id`.
///
/// Follows `chain_next_page(...)` links until `0`. The root page is included.
fn collect_heap_chain_pages(
    storage: &mut dyn StorageEngine,
    root_page_id: u64,
) -> Result<Vec<u64>, DbError> {
    let mut pages = Vec::new();
    let mut pid = root_page_id;
    while pid != 0 {
        pages.push(pid);
        let page = storage.read_page(pid)?;
        pid = chain_next_page(page);
    }
    Ok(pages)
}

/// Collects all page_ids in a B-Tree rooted at `root_pid` (BFS walk).
///
/// The result includes internal nodes and leaf nodes but excludes `0` sentinels.
fn collect_btree_pages(
    storage: &mut dyn StorageEngine,
    root_pid: u64,
) -> Result<Vec<u64>, DbError> {
    use axiomdb_index::page_layout::cast_internal;

    let mut collected = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        collected.push(pid);
        let page = storage.read_page(pid)?;
        if page.body()[0] != 1 {
            // Internal node — push all children.
            let node = cast_internal(page);
            let n = node.num_keys();
            for i in 0..=n {
                stack.push(node.child_at(i));
            }
        }
    }
    Ok(collected)
}

/// Plans a bulk table-empty operation: counts visible rows, allocates fresh roots,
/// and collects old page IDs for deferred reclamation.
///
/// Collect old pages FIRST, then allocate new ones so freshly-allocated pages
/// are never accidentally added to the free list.
fn plan_bulk_empty_table(
    storage: &mut dyn StorageEngine,
    table_def: &axiomdb_catalog::TableDef,
    indexes: &[axiomdb_catalog::IndexDef],
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<BulkEmptyPlan, DbError> {
    // Count rows visible to this statement for DELETE row-count semantics.
    let rids = HeapChain::scan_rids_visible(storage, table_def.data_root_page_id, snap)?;
    let visible_row_count = rids.len() as u64;

    // Collect old page IDs before allocating new ones (avoids any overlap).
    let mut old_pages = collect_heap_chain_pages(storage, table_def.data_root_page_id)?;
    for idx in indexes {
        old_pages.extend(collect_btree_pages(storage, idx.root_page_id)?);
    }
    old_pages.sort_unstable();
    old_pages.dedup();

    // Allocate fresh empty roots AFTER collecting old IDs.
    let new_data_root = alloc_empty_heap_root(storage)?;
    let mut new_index_roots = Vec::with_capacity(indexes.len());
    for idx in indexes {
        new_index_roots.push((idx.index_id, alloc_empty_index_root(storage)?));
    }

    Ok(BulkEmptyPlan {
        visible_row_count,
        new_data_root,
        new_index_roots,
        old_pages_to_free: old_pages,
    })
}

/// Applies a [`BulkEmptyPlan`]: rotates heap + index roots in the catalog,
/// resets Bloom filters, schedules old pages for deferred free, and invalidates
/// the session schema cache.
///
/// All catalog mutations happen inside the current active transaction, so they
/// are fully undone on rollback or savepoint rollback.
fn apply_bulk_empty_table(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    table_def: &axiomdb_catalog::TableDef,
    plan: BulkEmptyPlan,
) -> Result<(), DbError> {
    // Rotate heap root in the catalog.
    CatalogWriter::new(storage, txn)?.update_table_data_root(table_def.id, plan.new_data_root)?;

    // Rotate each index root in the catalog + reset its Bloom filter.
    for (index_id, new_root) in &plan.new_index_roots {
        CatalogWriter::new(storage, txn)?.update_index_root(*index_id, *new_root)?;
        // Reset bloom filter so old key presence checks return false.
        bloom.create(*index_id, 0);
    }

    // Enqueue old pages for post-commit reclamation.
    txn.defer_free_pages(plan.old_pages_to_free)?;

    Ok(())
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

// ── ANALYZE (Phase 6.12) ──────────────────────────────────────────────────────

/// Refreshes per-column statistics by doing an exact full-table scan.
///
/// Computes `row_count` and `ndv` (distinct non-NULL values) for each target
/// column and writes them to `axiom_stats` via `CatalogWriter::upsert_stats`.
///
/// After ANALYZE, the staleness counter for the table is cleared in `ctx.stats`
/// so the query planner can immediately use the fresh statistics.
fn execute_analyze(
    stmt: crate::ast::AnalyzeStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let schema = "public";
    let snap = txn.active_snapshot()?;

    // Collect target tables.
    let target_tables: Vec<String> = if let Some(table_name) = stmt.table {
        vec![table_name]
    } else {
        // ANALYZE without TABLE — all tables in schema.
        let mut reader = CatalogReader::new(storage, snap)?;
        reader
            .list_tables(schema)?
            .into_iter()
            .map(|t| t.table_name)
            .collect()
    };

    for table_name in target_tables {
        let resolved = {
            let mut resolver = make_resolver(storage, txn)?;
            match resolver.resolve_table(Some(schema), &table_name) {
                Ok(r) => r,
                Err(_) => continue, // table may not exist — skip
            }
        };

        // Scan the full table once.
        let rows = TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap, None)?;
        let row_count = rows.len() as u64;

        // Determine target columns: all indexed columns OR a specific one.
        let target_col_idxs: Vec<u16> = if let Some(col_name) = &stmt.column {
            resolved
                .columns
                .iter()
                .filter(|c| &c.name == col_name)
                .map(|c| c.col_idx)
                .collect()
        } else {
            // All columns that appear as leading columns of any index.
            let mut seen = std::collections::HashSet::new();
            resolved
                .indexes
                .iter()
                .filter_map(|i| i.columns.first().map(|c| c.col_idx))
                .filter(|col_idx| seen.insert(*col_idx))
                .collect()
        };

        for col_idx in target_col_idxs {
            let ndv = compute_ndv_exact(col_idx, &rows);
            // Ignore write errors — stats are advisory.
            let _ = CatalogWriter::new(storage, txn)?.upsert_stats(axiomdb_catalog::StatsDef {
                table_id: resolved.def.id,
                col_idx,
                row_count,
                ndv,
            });
        }

        // Clear staleness so the planner uses fresh stats immediately.
        ctx.stats.mark_fresh(resolved.def.id);
        // Invalidate schema cache so next query gets fresh resolved table.
        ctx.invalidate_table(schema, &table_name);
    }

    Ok(QueryResult::Empty)
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

    let snap = txn.active_snapshot()?;

    // TRUNCATE TABLE must fail if child FKs reference this table as the parent.
    // AxiomDB does not implement TRUNCATE ... CASCADE; the caller must DELETE
    // or TRUNCATE child tables first (same as PostgreSQL's behavior).
    {
        let mut reader = CatalogReader::new(storage, snap)?;
        let parent_fks = reader.list_fk_constraints_referencing(resolved.def.id)?;
        if !parent_fks.is_empty() {
            let fk = &parent_fks[0];
            return Err(DbError::ForeignKeyParentViolation {
                constraint: fk.name.clone(),
                child_table: format!("table_id={}", fk.child_table_id),
                child_column: format!("col_idx={}", fk.child_col_idx),
            });
        }
    }

    // Collect all indexes with columns for root rotation.
    let all_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    // Bulk-empty via root rotation (Phase 5.16): correct for indexed tables.
    let mut noop_bloom = crate::bloom::BloomRegistry::new();
    let plan = plan_bulk_empty_table(storage, &resolved.def, &all_indexes, snap)?;
    apply_bulk_empty_table(storage, txn, &mut noop_bloom, &resolved.def, plan)?;

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
            AlterTableOp::AddConstraint(tc) => {
                alter_add_constraint(storage, txn, &table_def, &columns, tc, schema)?;
            }
            AlterTableOp::DropConstraint { name, if_exists } => {
                alter_drop_constraint(storage, txn, &table_def, &name, if_exists)?;
            }
            _ => {
                return Err(DbError::NotImplemented {
                    feature: "ALTER TABLE MODIFY COLUMN — Phase N".into(),
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

// ── CHECK constraint enforcement (Phase 4.22b) ────────────────────────────────

/// Evaluates active CHECK constraints for a row about to be inserted/updated.
fn check_row_constraints(
    constraints: &[axiomdb_catalog::schema::ConstraintDef],
    row_values: &[Value],
    table_name: &str,
) -> Result<(), DbError> {
    for c in constraints {
        if c.check_expr.is_empty() {
            continue;
        }
        let expr = match crate::parser::parse_expr_only(&c.check_expr) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let result = eval(&expr, row_values)?;
        if !crate::eval::is_truthy(&result) {
            return Err(DbError::CheckViolation {
                table: table_name.to_string(),
                constraint: c.name.clone(),
            });
        }
    }
    Ok(())
}

// ── ALTER TABLE constraint helpers (Phase 4.22b) ──────────────────────────────

fn alter_add_constraint(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::ResolvedTable,
    columns_arg: &[axiomdb_catalog::schema::ColumnDef],
    tc: crate::ast::TableConstraint,
    schema: &str,
) -> Result<(), DbError> {
    use crate::ast::TableConstraint;
    use axiomdb_catalog::schema::ConstraintDef;

    match tc {
        TableConstraint::Unique {
            name,
            columns: col_names,
        } => {
            // ADD CONSTRAINT name UNIQUE (cols) → create a unique index named `name`.
            let idx_name = name.unwrap_or_else(|| {
                format!(
                    "axiom_uq_{}_{}",
                    table_def.def.table_name,
                    col_names.join("_")
                )
            });
            let stmt = crate::ast::CreateIndexStmt {
                name: idx_name,
                table: crate::ast::TableRef {
                    schema: Some(schema.to_string()),
                    name: table_def.def.table_name.clone(),
                    alias: None,
                },
                columns: col_names
                    .into_iter()
                    .map(|c| crate::ast::IndexColumn {
                        name: c,
                        order: crate::ast::SortOrder::Asc,
                    })
                    .collect(),
                unique: true,
                if_not_exists: false,
                predicate: None,         // UNIQUE constraints are always full indexes
                fillfactor: None,        // use default 90
                include_columns: vec![], // UNIQUE constraints have no included columns
            };
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_create_index(stmt, storage, txn, &mut noop_bloom)?;
            Ok(())
        }

        TableConstraint::Check { name, expr } => {
            let cname = name.ok_or_else(|| DbError::ParseError {
                message: "ADD CONSTRAINT CHECK requires an explicit constraint name".into(),
                position: None,
            })?;

            // Check for duplicate constraint name.
            let snap = txn.active_snapshot()?;
            {
                let mut reader = CatalogReader::new(storage, snap)?;
                if reader
                    .get_constraint_by_name(table_def.def.id, &cname)?
                    .is_some()
                {
                    return Err(DbError::Other(format!(
                        "constraint '{cname}' already exists on table '{}'",
                        table_def.def.table_name
                    )));
                }
            }

            // Validate all existing rows.
            let existing_rows =
                TableEngine::scan_table(storage, &table_def.def, columns_arg, snap, None)?;
            for (_rid, row_values) in &existing_rows {
                let result = eval(&expr, row_values)?;
                if !crate::eval::is_truthy(&result) {
                    return Err(DbError::CheckViolation {
                        table: table_def.def.table_name.clone(),
                        constraint: cname.clone(),
                    });
                }
            }

            // Serialize the expression to SQL string for persistence.
            let check_expr = expr_to_sql_string(&expr);

            // Persist in axiom_constraints.
            CatalogWriter::new(storage, txn)?.create_constraint(ConstraintDef {
                constraint_id: 0, // allocated by writer
                table_id: table_def.def.id,
                name: cname,
                check_expr,
            })?;
            Ok(())
        }

        TableConstraint::ForeignKey {
            name,
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
        } => {
            if columns.len() != 1 {
                return Err(DbError::NotImplemented {
                    feature: "composite foreign key (multiple columns) — Phase 6.9".into(),
                });
            }
            let child_col_name = &columns[0];
            let child_col_idx = columns_arg
                .iter()
                .find(|c| &c.name == child_col_name)
                .map(|c| c.col_idx)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: child_col_name.clone(),
                    table: table_def.def.table_name.clone(),
                })?;
            let ref_col = ref_columns.first().map(|s| s.as_str());

            // Persist the FK definition (validates parent, creates auto-index if needed).
            persist_fk_constraint(
                table_def.def.id,
                &table_def.def.table_name,
                child_col_idx,
                child_col_name,
                &ref_table,
                ref_col,
                ast_fk_action_to_catalog(on_delete),
                ast_fk_action_to_catalog(on_update),
                name.as_deref(),
                storage,
                txn,
            )?;

            // Validate existing data: every non-NULL FK value must reference a parent row.
            let snap = txn.active_snapshot()?;
            let default_constraint_name = format!(
                "fk_{}_{}_{ref_table}",
                table_def.def.table_name, child_col_name
            );
            let constraint_name = name.as_deref().unwrap_or(&default_constraint_name);
            let new_fk = {
                let mut reader = CatalogReader::new(storage, snap)?;
                reader
                    .get_fk_by_name(table_def.def.id, constraint_name)?
                    .ok_or_else(|| DbError::Internal {
                        message: "FK just created not found in catalog".into(),
                    })?
            };
            let existing_rows =
                TableEngine::scan_table(storage, &table_def.def, columns_arg, snap, None)?;
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            for (_, row) in &existing_rows {
                if let Err(e) = crate::fk_enforcement::check_fk_child_insert(
                    row,
                    std::slice::from_ref(&new_fk),
                    storage,
                    txn,
                    &mut noop_bloom,
                ) {
                    // Roll back: drop the FK definition (and its auto-created index).
                    let snap2 = txn.active_snapshot()?;
                    if let Ok(Some(fk)) = CatalogReader::new(storage, snap2)?
                        .get_fk_by_name(table_def.def.id, &new_fk.name)
                    {
                        let fk_index_id = fk.fk_index_id;
                        CatalogWriter::new(storage, txn)?.drop_foreign_key(fk.fk_id)?;
                        if fk_index_id != 0 {
                            let _ = execute_drop_index_by_id(
                                fk_index_id,
                                storage,
                                txn,
                                &mut noop_bloom,
                            );
                        }
                    }
                    return Err(e);
                }
            }

            Ok(())
        }

        TableConstraint::PrimaryKey { .. } => Err(DbError::NotImplemented {
            feature: "ADD CONSTRAINT PRIMARY KEY — requires full table rewrite".into(),
        }),
    }
}

fn alter_drop_constraint(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &axiomdb_catalog::ResolvedTable,
    name: &str,
    if_exists: bool,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot()?;
    let table_id = table_def.def.id;

    // 1. Search in axiom_indexes (UNIQUE constraints stored as indexes).
    let (idx_id, idx_root) = {
        let mut reader = CatalogReader::new(storage, snap)?;
        let indexes = reader.list_indexes(table_id)?;
        match indexes.into_iter().find(|i| i.name == name) {
            Some(i) => (Some(i.index_id), Some(i.root_page_id)),
            None => (None, None),
        }
    };

    if let Some(index_id) = idx_id {
        CatalogWriter::new(storage, txn)?.delete_index(index_id)?;
        if let Some(root) = idx_root {
            free_btree_pages(storage, root)?;
        }
        return Ok(());
    }

    // 2. Search in axiom_constraints (CHECK constraints).
    let constraint = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.get_constraint_by_name(table_id, name)?
    };

    if let Some(c) = constraint {
        CatalogWriter::new(storage, txn)?.drop_constraint(c.constraint_id)?;
        return Ok(());
    }

    // 3. Search in axiom_foreign_keys (FK constraints — Phase 6.5).
    let fk = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.get_fk_by_name(table_id, name)?
    };

    if let Some(fk_def) = fk {
        let fk_index_id = fk_def.fk_index_id;
        CatalogWriter::new(storage, txn)?.drop_foreign_key(fk_def.fk_id)?;
        // Drop the auto-created FK index (fk_index_id != 0 means we created it).
        if fk_index_id != 0 {
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_drop_index_by_id(fk_index_id, storage, txn, &mut noop_bloom)?;
        }
        return Ok(());
    }

    if if_exists {
        Ok(())
    } else {
        Err(DbError::Other(format!(
            "constraint '{name}' not found on table '{}'",
            table_def.def.table_name
        )))
    }
}

/// Converts an [`Expr`] to a SQL string suitable for storing in `axiom_constraints`.
///
/// Not a perfect round-trip — whitespace and casing may differ from the original
/// input, but the output is valid SQL that can be re-parsed and evaluated.
fn expr_to_sql_string(expr: &Expr) -> String {
    use crate::expr::BinaryOp;

    match expr {
        Expr::Literal(v) => match v {
            Value::Int(n) => n.to_string(),
            Value::BigInt(n) => n.to_string(),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Null => "NULL".to_string(),
            Value::Real(f) => f.to_string(),
            _ => format!("{v}"),
        },
        Expr::Column { name, .. } => name.clone(),
        Expr::BinaryOp { left, op, right } => {
            let op_str = match op {
                BinaryOp::Eq => "=",
                BinaryOp::NotEq => "!=",
                BinaryOp::Lt => "<",
                BinaryOp::LtEq => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::GtEq => ">=",
                BinaryOp::And => "AND",
                BinaryOp::Or => "OR",
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::Concat => "||",
            };
            format!(
                "({} {op_str} {})",
                expr_to_sql_string(left),
                expr_to_sql_string(right)
            )
        }
        Expr::UnaryOp {
            op: crate::expr::UnaryOp::Not,
            operand,
        } => {
            format!("NOT {}", expr_to_sql_string(operand))
        }
        Expr::IsNull {
            expr: inner,
            negated: false,
        } => {
            format!("{} IS NULL", expr_to_sql_string(inner))
        }
        Expr::IsNull {
            expr: inner,
            negated: true,
        } => {
            format!("{} IS NOT NULL", expr_to_sql_string(inner))
        }
        // For complex expressions not yet handled, fall back to a debug representation.
        other => format!("{other:?}"),
    }
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

/// Remaps ORDER BY expressions so they can be evaluated against grouped output rows.
///
/// Grouped output rows are indexed by SELECT output position (0 = first SELECT item,
/// 1 = second, ...).  ORDER BY expressions, however, reference the *source* schema:
/// `Expr::Column { col_idx }` means "column col_idx in the original table row".
///
/// This function rewrites every sub-expression that structurally matches a SELECT
/// item with `Expr::Column { col_idx: output_pos }`, so that `apply_order_by` can
/// index into the projected output row correctly.  Handles both plain column
/// references and aggregate expressions (COUNT(*), SUM(col), GROUP_CONCAT, …).
///
/// Expressions that match no SELECT item are left unchanged (they will produce
/// errors or Null at evaluation time, which is the correct behavior for
/// semantically invalid ORDER BY in GROUP BY context).
fn remap_order_by_for_grouped(
    order_by: &[crate::ast::OrderByItem],
    select_items: &[SelectItem],
) -> Vec<crate::ast::OrderByItem> {
    order_by
        .iter()
        .map(|item| crate::ast::OrderByItem {
            expr: remap_expr_for_grouped(&item.expr, select_items),
            order: item.order,
            nulls: item.nulls,
        })
        .collect()
}

/// Recursively rewrites `expr` for grouped output row evaluation.
///
/// - If `expr` structurally matches a SELECT item at output position `pos`,
///   returns `Expr::Column { col_idx: pos, … }`.
/// - Otherwise recurses into compound expressions (BinaryOp, UnaryOp, etc.)
///   so that `ORDER BY col + 1` is also handled when `col` is in the SELECT.
fn remap_expr_for_grouped(expr: &Expr, select_items: &[SelectItem]) -> Expr {
    // Direct match against a SELECT item.
    for (pos, item) in select_items.iter().enumerate() {
        if let SelectItem::Expr { expr: sel_expr, .. } = item {
            if expr == sel_expr {
                return Expr::Column {
                    col_idx: pos,
                    name: format!("_out{pos}"),
                };
            }
        }
    }
    // Recurse into compound expressions.
    match expr.clone() {
        Expr::BinaryOp { op, left, right } => Expr::BinaryOp {
            op,
            left: Box::new(remap_expr_for_grouped(&left, select_items)),
            right: Box::new(remap_expr_for_grouped(&right, select_items)),
        },
        Expr::UnaryOp { op, operand } => Expr::UnaryOp {
            op,
            operand: Box::new(remap_expr_for_grouped(&operand, select_items)),
        },
        Expr::IsNull {
            expr: inner,
            negated,
        } => Expr::IsNull {
            expr: Box::new(remap_expr_for_grouped(&inner, select_items)),
            negated,
        },
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(remap_expr_for_grouped(&inner, select_items)),
            low: Box::new(remap_expr_for_grouped(&low, select_items)),
            high: Box::new(remap_expr_for_grouped(&high, select_items)),
            negated,
        },
        Expr::Function { name, args } => Expr::Function {
            name,
            args: args
                .iter()
                .map(|a| remap_expr_for_grouped(a, select_items))
                .collect(),
        },
        other => other,
    }
}

/// Evaluates a LIMIT or OFFSET expression as a non-negative `usize`.
///
/// Accepted value types and their contracts:
/// - `Int(n)`    where `n >= 0`  → `n as usize`
/// - `BigInt(n)` where `n >= 0`  → `usize::try_from(n)` (errors on overflow)
/// - `Text(s)`   where `s.trim()` is an exact base-10 integer `>= 0`  → parsed
///
/// Everything else — negatives, non-integral text, NULL, REAL, BOOL, etc. —
/// returns `DbError::TypeMismatch`.
///
/// This function is the single enforcement point for LIMIT/OFFSET row-count
/// coercion for both the cached-AST prepared-statement path and the
/// SQL-string substitution fallback path.
fn eval_row_count_as_usize(expr: &Expr) -> Result<usize, DbError> {
    fn mismatch(expected: &str, got: &str) -> DbError {
        DbError::TypeMismatch {
            expected: expected.into(),
            got: got.into(),
        }
    }

    match eval(expr, &[])? {
        Value::Int(n) if n >= 0 => Ok(n as usize),
        Value::Int(_) => Err(mismatch(
            "non-negative integer for LIMIT/OFFSET",
            "negative integer",
        )),
        Value::BigInt(n) if n >= 0 => usize::try_from(n).map_err(|_| {
            mismatch(
                "non-negative integer for LIMIT/OFFSET",
                "integer too large for this platform",
            )
        }),
        Value::BigInt(_) => Err(mismatch(
            "non-negative integer for LIMIT/OFFSET",
            "negative integer",
        )),
        Value::Text(s) => {
            let trimmed = s.trim();
            let parsed = trimmed.parse::<i64>().map_err(|_| {
                mismatch(
                    "non-negative integer for LIMIT/OFFSET",
                    &format!("non-integral text: {trimmed:?}"),
                )
            })?;
            if parsed < 0 {
                return Err(mismatch(
                    "non-negative integer for LIMIT/OFFSET",
                    "negative integer",
                ));
            }
            usize::try_from(parsed).map_err(|_| {
                mismatch(
                    "non-negative integer for LIMIT/OFFSET",
                    "integer too large for this platform",
                )
            })
        }
        other => Err(mismatch("integer for LIMIT/OFFSET", other.variant_name())),
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
    let offset_n = offset
        .as_ref()
        .map(eval_row_count_as_usize)
        .transpose()?
        .unwrap_or(0);
    let limit_n = limit.as_ref().map(eval_row_count_as_usize).transpose()?;
    Ok(rows
        .into_iter()
        .skip(offset_n)
        .take(limit_n.unwrap_or(usize::MAX))
        .collect())
}

// ── Non-unique index key helpers ──────────────────────────────────────────────

/// Returns the lower bound for a non-unique index range scan on `prefix`.
///
/// Non-unique secondary indexes store `encode_index_key(vals) || encode_rid(rid)`
/// so that multiple rows with the same indexed value each get a unique B-Tree key.
/// To find all entries with a given prefix, use `[prefix||0x00..00, prefix||0xFF..FF]`.
fn rid_lo(prefix: &[u8]) -> Vec<u8> {
    let mut v = prefix.to_vec();
    v.extend_from_slice(&[0u8; 10]);
    v
}

/// Returns the upper bound for a non-unique index range scan on `prefix`.
fn rid_hi(prefix: &[u8]) -> Vec<u8> {
    let mut v = prefix.to_vec();
    v.extend_from_slice(&[0xFFu8; 10]);
    v
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

    // ── 4.9b: GROUP BY strategy helpers ──────────────────────────────────────

    fn make_index_def(col_idxs: &[u16]) -> IndexDef {
        use axiomdb_catalog::schema::{IndexColumnDef, SortOrder};
        IndexDef {
            index_id: 1,
            table_id: 1,
            name: "test_idx".into(),
            root_page_id: 1,
            is_unique: false,
            is_primary: false,
            columns: col_idxs
                .iter()
                .map(|&c| IndexColumnDef {
                    col_idx: c,
                    order: SortOrder::Asc,
                })
                .collect(),
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
        }
    }

    fn col_expr(idx: usize) -> Expr {
        Expr::Column {
            col_idx: idx,
            name: format!("c{idx}"),
        }
    }

    #[test]
    fn test_group_by_matches_index_prefix_single_col() {
        let idx = make_index_def(&[2]);
        assert!(group_by_matches_index_prefix(&[col_expr(2)], &idx));
    }

    #[test]
    fn test_group_by_matches_index_prefix_composite_full() {
        let idx = make_index_def(&[1, 3]);
        assert!(group_by_matches_index_prefix(
            &[col_expr(1), col_expr(3)],
            &idx
        ));
    }

    #[test]
    fn test_group_by_matches_index_prefix_leading_only() {
        // GROUP BY on only the first column of a (region, dept) index — valid prefix.
        let idx = make_index_def(&[1, 3]);
        assert!(group_by_matches_index_prefix(&[col_expr(1)], &idx));
    }

    #[test]
    fn test_group_by_matches_index_prefix_reordered_fails() {
        let idx = make_index_def(&[1, 3]);
        assert!(!group_by_matches_index_prefix(
            &[col_expr(3), col_expr(1)],
            &idx
        ));
    }

    #[test]
    fn test_group_by_matches_index_prefix_non_column_expr_fails() {
        let idx = make_index_def(&[2]);
        let lower_expr = Expr::Function {
            name: "lower".into(),
            args: vec![col_expr(2)],
        };
        assert!(!group_by_matches_index_prefix(&[lower_expr], &idx));
    }

    #[test]
    fn test_group_by_matches_index_prefix_empty_group_by() {
        let idx = make_index_def(&[2]);
        assert!(group_by_matches_index_prefix(&[], &idx));
    }

    #[test]
    fn test_group_by_matches_index_prefix_longer_than_index_fails() {
        let idx = make_index_def(&[1]);
        assert!(!group_by_matches_index_prefix(
            &[col_expr(1), col_expr(2)],
            &idx
        ));
    }

    #[test]
    fn test_group_keys_equal_nulls() {
        // NULL == NULL for GROUP BY grouping purposes.
        assert!(group_keys_equal(&[Value::Null], &[Value::Null]));
    }

    #[test]
    fn test_group_keys_equal_mixed() {
        assert!(group_keys_equal(
            &[Value::Int(1), Value::Text("a".into())],
            &[Value::Int(1), Value::Text("a".into())]
        ));
        assert!(!group_keys_equal(
            &[Value::Int(1), Value::Text("a".into())],
            &[Value::Int(1), Value::Text("b".into())]
        ));
    }

    #[test]
    fn test_compare_group_key_lists_ordering() {
        use std::cmp::Ordering;
        assert_eq!(
            compare_group_key_lists(&[Value::Int(1)], &[Value::Int(2)]),
            Ordering::Less
        );
        assert_eq!(
            compare_group_key_lists(&[Value::Int(2)], &[Value::Int(1)]),
            Ordering::Greater
        );
        assert_eq!(
            compare_group_key_lists(&[Value::Null], &[Value::Int(1)]),
            Ordering::Greater // NULL last
        );
    }
}
