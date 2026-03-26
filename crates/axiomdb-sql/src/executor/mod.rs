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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::HashMap as StdHashMap;

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
pub fn execute(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    if txn.active_txn_id().is_some() {
        dispatch(stmt, storage, txn)
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
                        let _ = txn.rollback(storage);
                        Err(e)
                    }
                }
            }
        }
    }
}

/// Like [`execute`] but uses a persistent [`SessionContext`] for schema caching.
pub fn execute_with_ctx(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    if txn.active_txn_id().is_some() {
        match &stmt {
            Stmt::Commit => return txn.commit().map(|_| QueryResult::Empty),
            Stmt::Rollback => return txn.rollback(storage).map(|_| QueryResult::Empty),
            Stmt::Begin => {
                let txn_id = txn.active_txn_id().unwrap_or(0);
                return Err(DbError::TransactionAlreadyActive { txn_id });
            }
            _ => {}
        }
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
        let sp_opt: Option<Savepoint> = if ctx.on_error == OnErrorMode::RollbackTransaction {
            None
        } else {
            Some(txn.savepoint())
        };
        match dispatch_ctx(stmt, storage, txn, bloom, ctx) {
            Ok(result) => Ok(result),
            Err(e) => match ctx.on_error {
                OnErrorMode::RollbackTransaction => {
                    let _ = txn.rollback(storage);
                    Err(e)
                }
                OnErrorMode::Ignore if crate::session::is_ignorable_on_error(&e) => {
                    if let Some(sp) = sp_opt {
                        let _ = txn.rollback_to_savepoint(sp, storage);
                    }
                    Err(e)
                }
                OnErrorMode::Ignore => {
                    let _ = txn.rollback(storage);
                    Err(e)
                }
                _ => {
                    if let Some(sp) = sp_opt {
                        let _ = txn.rollback_to_savepoint(sp, storage);
                    }
                    Err(e)
                }
            },
        }
    } else if ctx.autocommit {
        match stmt {
            Stmt::Begin => {
                txn.begin()?;
                Ok(QueryResult::Empty)
            }
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
    } else {
        match stmt {
            Stmt::Begin => {
                txn.begin()?;
                Ok(QueryResult::Empty)
            }
            Stmt::Commit => {
                ctx.warn(1592, "There is no active transaction");
                Ok(QueryResult::Empty)
            }
            Stmt::Rollback => {
                ctx.warn(1592, "There is no active transaction");
                Ok(QueryResult::Empty)
            }
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
        other => dispatch(other, storage, txn),
    }
}

/// Extracts a normalized string value from a `SetValue`.
fn set_value_to_setting_string(value: &SetValue) -> Result<Option<String>, DbError> {
    match value {
        SetValue::Default => Ok(None),
        SetValue::Expr(Expr::Literal(Value::Text(s))) => Ok(Some(s.clone())),
        SetValue::Expr(Expr::Literal(Value::Int(n))) => Ok(Some(n.to_string())),
        SetValue::Expr(Expr::Literal(Value::BigInt(n))) => Ok(Some(n.to_string())),
        SetValue::Expr(Expr::Literal(Value::Bool(b))) => {
            Ok(Some(if *b { "1".to_string() } else { "0".to_string() }))
        }
        SetValue::Expr(Expr::Column { name, .. }) => Ok(Some(name.clone())),
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
                        });
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
                        });
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
                        });
                    }
                };
                let normalized = normalize_sql_mode(&raw);
                ctx.strict_mode = sql_mode_is_strict(&normalized);
            }
        },
        "on_error" => {
            let raw = match set_value_to_setting_string(&stmt.value)? {
                None => "rollback_statement".to_string(),
                Some(s) => s,
            };
            ctx.on_error = parse_on_error_setting(&raw)?;
        }
        "axiom_compat" => {
            let raw = match set_value_to_setting_string(&stmt.value)? {
                None => "standard".to_string(),
                Some(s) => s,
            };
            ctx.compat_mode = parse_compat_mode_setting(&raw)?;
        }
        "collation" => {
            let raw = match set_value_to_setting_string(&stmt.value)? {
                None => "default".to_string(),
                Some(s) => s,
            };
            ctx.explicit_collation = parse_session_collation_setting(&raw)?;
        }
        _ => {}
    }
    Ok(QueryResult::Empty)
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
        Stmt::Select(s) => execute_select(s, storage, txn),
        Stmt::Insert(s) => execute_insert(s, storage, txn),
        Stmt::Update(s) => execute_update(s, storage, txn),
        Stmt::Delete(s) => execute_delete(s, storage, txn),
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
        Stmt::Set(_) => Ok(QueryResult::Empty),
        Stmt::TruncateTable(s) => execute_truncate(s, storage, txn),
        Stmt::AlterTable(s) => execute_alter_table(s, storage, txn),
        Stmt::ShowTables(s) => execute_show_tables(s, storage, txn),
        Stmt::ShowColumns(s) => execute_show_columns(s, storage, txn),
        Stmt::Analyze(_) => Err(DbError::NotImplemented {
            feature: "ANALYZE requires session context — use execute_with_ctx".into(),
        }),
    }
}

include!("shared.rs");
include!("joins.rs");
include!("aggregate.rs");
include!("select.rs");
include!("insert.rs");
include!("update.rs");
include!("bulk_empty.rs");
include!("delete.rs");
include!("ddl.rs");

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
            Ordering::Greater
        );
    }
}
