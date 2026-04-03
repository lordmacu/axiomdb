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
        SortOrder as CatalogSortOrder, TableDef, DEFAULT_DATABASE_NAME,
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
        AlterTableOp, AlterTableStmt, ColumnConstraint, CreateDatabaseStmt, CreateIndexStmt,
        CreateTableStmt, DeleteStmt, DropDatabaseStmt, DropIndexStmt, DropTableStmt, FromClause,
        InsertSource, InsertStmt, JoinClause, JoinCondition, JoinType, NullsOrder, OrderByItem,
        SelectItem, SelectStmt, SetStmt, SetValue, ShowDatabasesStmt, SortOrder, Stmt, UpdateStmt,
        UseDatabaseStmt,
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
/// Holds shared refs to `storage`, `txn`, and `bloom`, a mutable ref to `ctx`,
/// plus the current outer row for `substitute_outer`. Created fresh for each outer row.
struct ExecSubqueryRunner<'a> {
    storage: &'a dyn StorageEngine,
    txn: &'a TxnManager,
    bloom: &'a crate::bloom::BloomRegistry,
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
/// Executes a read-only statement with shared references only (Phase 7.4).
///
/// Safe to call without any exclusive lock. Handles SELECT, SHOW TABLES,
/// SHOW COLUMNS, SHOW DATABASES. Returns `NotImplemented` for write statements.
///
/// Uses `txn` as `&TxnManager` (shared ref) — only calls `snapshot()` and
/// `active_snapshot()`, never `begin/commit/rollback`.
pub fn execute_read_only_with_ctx(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    match stmt {
        Stmt::Select(s) => execute_select_ctx(s, storage, txn, bloom, ctx),
        Stmt::ShowTables(mut s) => {
            if s.schema.is_none() {
                s.schema = Some(ctx.current_schema().to_string());
            }
            let db = ctx.effective_database();
            let schema = s.schema.as_deref().unwrap_or("public");
            let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
            let tables = reader.list_tables_in_database(db, schema)?;
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
        Stmt::ShowDatabases(_) => {
            let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
            let dbs = reader.list_databases()?;
            let out_cols = vec![ColumnMeta::computed(
                String::from("Database"),
                DataType::Text,
            )];
            let rows: Vec<Row> = dbs.into_iter().map(|d| vec![Value::Text(d.name)]).collect();
            Ok(QueryResult::Rows {
                columns: out_cols,
                rows,
            })
        }
        _ => Err(DbError::NotImplemented {
            feature: "read-only executor does not handle this statement type".into(),
        }),
    }
}

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
/// Undoes index inserts accumulated in the transaction's undo log, then
/// performs the heap-level rollback via `TxnManager::rollback()`.
///
/// `TxnManager` cannot depend on `axiomdb-index`, so index B-Tree deletes
/// are handled at the executor layer. This function must be called instead
/// of bare `txn.rollback(storage)` whenever the transaction may have
/// performed INSERT or UPDATE operations that added B-Tree entries.
fn rollback_with_index_undo(
    txn: &mut TxnManager,
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    // Collect index insert undos BEFORE rollback (rollback consumes the undo log).
    let index_undos = txn.collect_index_undos();
    let mut current_roots = load_current_index_roots(txn, storage, &index_undos)?;
    for (index_id, root_page_id, key) in &index_undos {
        let current_root = current_roots
            .get(index_id)
            .copied()
            .unwrap_or(*root_page_id);
        let root = std::sync::atomic::AtomicU64::new(current_root);
        // Best-effort: if the key is already absent (idempotent), ignore the error.
        let _ = BTree::delete_in(storage, &root, key);
        bloom.mark_dirty(*index_id);
        // If the root changed, update the catalog.
        let new_root = root.load(std::sync::atomic::Ordering::Acquire);
        current_roots.insert(*index_id, new_root);
        if new_root != current_root {
            if let Ok(mut cw) = CatalogWriter::new(storage, txn) {
                let _ = cw.update_index_root(*index_id, new_root);
            }
        }
    }
    txn.rollback(storage)
}

/// Like [`rollback_with_index_undo`] but for savepoint rollback.
fn rollback_to_savepoint_with_index_undo(
    txn: &mut TxnManager,
    sp: Savepoint,
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    let index_undos = txn.collect_index_undos_since(&sp);
    let mut current_roots = load_current_index_roots(txn, storage, &index_undos)?;
    for (index_id, root_page_id, key) in &index_undos {
        let current_root = current_roots
            .get(index_id)
            .copied()
            .unwrap_or(*root_page_id);
        let root = std::sync::atomic::AtomicU64::new(current_root);
        let _ = BTree::delete_in(storage, &root, key);
        bloom.mark_dirty(*index_id);
        let new_root = root.load(std::sync::atomic::Ordering::Acquire);
        current_roots.insert(*index_id, new_root);
        if new_root != current_root {
            if let Ok(mut cw) = CatalogWriter::new(storage, txn) {
                let _ = cw.update_index_root(*index_id, new_root);
            }
        }
    }
    txn.rollback_to_savepoint(sp, storage)
}

fn load_current_index_roots(
    txn: &TxnManager,
    storage: &dyn StorageEngine,
    index_undos: &[(u32, u64, Vec<u8>)],
) -> Result<std::collections::HashMap<u32, u64>, DbError> {
    let mut roots = std::collections::HashMap::new();
    if index_undos.is_empty() {
        return Ok(roots);
    }

    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let mut reader = CatalogReader::new(storage, snap)?;
    for (index_id, fallback_root, _) in index_undos {
        if roots.contains_key(index_id) {
            continue;
        }
        let root = reader
            .get_index_by_id(*index_id)?
            .map(|idx| idx.root_page_id)
            .unwrap_or(*fallback_root);
        roots.insert(*index_id, root);
    }
    Ok(roots)
}

pub fn execute_with_ctx(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    if txn.active_txn_id().is_some() {
        match &stmt {
            Stmt::Commit => {
                // Flush any staged rows before writing the Commit WAL entry.
                flush_pending_inserts_ctx(storage, txn, bloom, ctx)?;
                ctx.in_explicit_txn = false;
                ctx.savepoints.clear(); // all savepoints destroyed on COMMIT
                return txn.commit().map(|_| QueryResult::Empty);
            }
            Stmt::Rollback => {
                // Discard staged rows without writing to heap or WAL.
                ctx.discard_pending_inserts();
                ctx.savepoints.clear(); // all savepoints destroyed on ROLLBACK
                return rollback_with_index_undo(txn, storage, bloom).map(|_| QueryResult::Empty);
            }
            Stmt::Begin => {
                let txn_id = txn.active_txn_id().unwrap_or(0);
                return Err(DbError::TransactionAlreadyActive { txn_id });
            }
            Stmt::Savepoint(ref name) => {
                flush_pending_inserts_ctx(storage, txn, bloom, ctx)?;
                let sp = txn.savepoint();
                ctx.savepoints.push((name.clone(), sp));
                return Ok(QueryResult::Empty);
            }
            Stmt::RollbackToSavepoint(ref name) => {
                // Find savepoint by name (most recent match).
                let pos = ctx.savepoints.iter().rposition(|(n, _)| n == name);
                match pos {
                    None => {
                        return Err(DbError::Other(format!("SAVEPOINT '{name}' does not exist")));
                    }
                    Some(idx) => {
                        // Discard staged rows.
                        ctx.discard_pending_inserts();
                        let sp = ctx.savepoints[idx].1;
                        rollback_to_savepoint_with_index_undo(txn, sp, storage, bloom)?;
                        // Destroy all savepoints after the target (MySQL behavior).
                        ctx.savepoints.truncate(idx + 1);
                        return Ok(QueryResult::Empty);
                    }
                }
            }
            Stmt::ReleaseSavepoint(ref name) => {
                let pos = ctx.savepoints.iter().rposition(|(n, _)| n == name);
                match pos {
                    None => {
                        return Err(DbError::Other(format!("SAVEPOINT '{name}' does not exist")));
                    }
                    Some(idx) => {
                        // Destroy target savepoint and all later ones.
                        ctx.savepoints.truncate(idx);
                        return Ok(QueryResult::Empty);
                    }
                }
            }
            _ => {}
        }
        if is_ddl(&stmt) {
            // DDL implicitly commits the current transaction — flush staged
            // rows into the pre-DDL transaction before committing it.
            flush_pending_inserts_ctx(storage, txn, bloom, ctx)?;
            ctx.in_explicit_txn = false;
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
                    let _ = rollback_with_index_undo(txn, storage, bloom);
                    Err(e)
                }
            };
        }
        // Flush staged inserts BEFORE taking the per-statement savepoint when
        // the next statement cannot continue appending to the current batch.
        // This ensures:
        // (a) flush writes become part of the "pre-statement" state;
        // (b) a later statement error does not roll back previously staged rows;
        // (c) barrier semantics: the current statement sees flushed rows.
        if should_flush_pending_inserts_before_stmt(&stmt, ctx) {
            flush_pending_inserts_ctx(storage, txn, bloom, ctx)?;
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
                    ctx.discard_pending_inserts();
                    let _ = rollback_with_index_undo(txn, storage, bloom);
                    Err(e)
                }
                OnErrorMode::Ignore if crate::session::is_ignorable_on_error(&e) => {
                    if let Some(sp) = sp_opt {
                        let _ = rollback_to_savepoint_with_index_undo(txn, sp, storage, bloom);
                    }
                    Err(e)
                }
                OnErrorMode::Ignore => {
                    ctx.discard_pending_inserts();
                    let _ = rollback_with_index_undo(txn, storage, bloom);
                    Err(e)
                }
                _ => {
                    if let Some(sp) = sp_opt {
                        let _ = rollback_to_savepoint_with_index_undo(txn, sp, storage, bloom);
                    }
                    Err(e)
                }
            },
        }
    } else if ctx.autocommit {
        match stmt {
            Stmt::Begin => {
                let level = ctx.effective_isolation();
                txn.begin_with_isolation(level)?;
                ctx.in_explicit_txn = true;
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
            Stmt::Savepoint(_) | Stmt::RollbackToSavepoint(_) | Stmt::ReleaseSavepoint(_) => {
                Err(DbError::NoActiveTransaction)
            }
            other => {
                txn.begin()?;
                // NOTE: `in_explicit_txn` is NOT set here — this is an implicit
                // autocommit transaction. Single-statement INSERTs use the existing
                // multi-row batch path inside execute_insert_ctx, not the staging buffer.
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
                        let _ = rollback_with_index_undo(txn, storage, bloom);
                        Err(e)
                    }
                }
            }
        }
    } else {
        match stmt {
            Stmt::Begin => {
                txn.begin()?;
                ctx.in_explicit_txn = true;
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
                        let _ = rollback_with_index_undo(txn, storage, bloom);
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
                        let _ = rollback_with_index_undo(txn, storage, bloom);
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
                                let _ =
                                    rollback_to_savepoint_with_index_undo(txn, sp, storage, bloom);
                            }
                            Err(e)
                        }
                        OnErrorMode::Savepoint => {
                            if let Some(sp) = sp_opt {
                                let _ =
                                    rollback_to_savepoint_with_index_undo(txn, sp, storage, bloom);
                            }
                            Err(e)
                        }
                        _ => {
                            let _ = rollback_with_index_undo(txn, storage, bloom);
                            Err(e)
                        }
                    },
                }
            }
        }
    }
}

fn should_flush_pending_inserts_before_stmt(stmt: &Stmt, ctx: &SessionContext) -> bool {
    let pending = match ctx.pending_inserts.as_ref() {
        Some(p) => p,
        None => return false,
    };

    !matches!(
        stmt,
        Stmt::Insert(insert)
            if ctx.in_explicit_txn
                && matches!(insert.source, InsertSource::Values(_))
                && insert.table.name == pending.table_def.table_name
                && insert
                    .table
                    .schema
                    .as_deref()
                    .is_none_or(|schema| schema == pending.table_def.schema_name)
    )
}

/// Returns `true` for DDL statements that require their own autocommit transaction.
fn is_ddl(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateTable(_)
            | Stmt::CreateDatabase(_)
            | Stmt::DropTable(_)
            | Stmt::DropDatabase(_)
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
    // Flush staged inserts before any non-INSERT barrier statement.
    // INSERT statements handle same-table vs. different-table flush internally.
    if !matches!(stmt, Stmt::Insert(_)) {
        flush_pending_inserts_ctx(storage, txn, bloom, ctx)?;
    }
    match stmt {
        Stmt::Select(s) => execute_select_ctx(s, storage, txn, bloom, ctx),
        Stmt::Insert(s) => execute_insert_ctx(s, storage, txn, bloom, ctx),
        Stmt::Update(s) => execute_update_ctx(s, storage, txn, bloom, ctx),
        Stmt::Delete(s) => execute_delete_ctx(s, storage, txn, bloom, ctx),
        Stmt::CreateTable(mut s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            // Unqualified CREATE TABLE uses the first schema in search_path.
            if s.table.schema.is_none() {
                s.table.schema = Some(ctx.current_schema().to_string());
            }
            execute_create_table(s, storage, txn, &db)
        }
        Stmt::CreateDatabase(s) => {
            ctx.invalidate_all();
            execute_create_database(s, storage, txn)
        }
        Stmt::CreateSchema(s) => {
            ctx.invalidate_all();
            execute_create_schema(s, storage, txn, ctx.effective_database())
        }
        Stmt::DropTable(s) => {
            ctx.invalidate_all();
            let db = s
                .tables
                .first()
                .and_then(|t| t.database.as_deref())
                .unwrap_or(ctx.effective_database())
                .to_string();
            execute_drop_table(s, storage, txn, &db)
        }
        Stmt::DropDatabase(s) => {
            ctx.invalidate_all();
            execute_drop_database(s, storage, txn, ctx)
        }
        Stmt::CreateIndex(s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            execute_create_index(s, storage, txn, bloom, &db)
        }
        Stmt::DropIndex(s) => {
            ctx.invalidate_all();
            let db = s
                .table
                .as_ref()
                .and_then(|t| t.database.as_deref())
                .unwrap_or(ctx.effective_database())
                .to_string();
            execute_drop_index(s, storage, txn, bloom, &db)
        }
        Stmt::AlterTable(s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            execute_alter_table(s, storage, txn, &db)
        }
        Stmt::Analyze(s) => execute_analyze(s, storage, txn, ctx),
        Stmt::Explain(inner) => execute_explain(*inner, storage, txn, bloom, ctx),
        Stmt::Vacuum(s) => crate::vacuum::execute_vacuum(s, storage, txn, bloom, ctx),
        Stmt::Set(s) => execute_set_ctx(s, ctx),
        Stmt::UseDatabase(s) => execute_use_database(s, storage, txn, ctx),
        Stmt::ShowDatabases(s) => execute_show_databases(s, storage, txn),
        Stmt::ShowTables(mut s) => {
            // Default to current schema from search_path if not explicit.
            if s.schema.is_none() {
                s.schema = Some(ctx.current_schema().to_string());
            }
            execute_show_tables(s, storage, txn, ctx.effective_database())
        }
        Stmt::ShowColumns(s) => {
            let db = ddl_database(&s.table.database, ctx);
            execute_show_columns(s, storage, txn, &db)
        }
        Stmt::TruncateTable(s) => {
            let db = ddl_database(&s.table.database, ctx);
            execute_truncate(s, storage, txn, &db)
        }
        other => dispatch(other, storage, txn),
    }
}

/// Compute the effective database for a DDL statement: if the `TableRef` has
/// an explicit `database` component, use it; otherwise fall back to the session
/// default. Returns an owned `String` so the original statement can be moved.
fn ddl_database(explicit: &Option<String>, ctx: &SessionContext) -> String {
    explicit
        .as_deref()
        .unwrap_or(ctx.effective_database())
        .to_string()
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
        "search_path" => {
            let raw = match set_value_to_setting_string(&stmt.value)? {
                None => {
                    // RESET search_path → restore default
                    ctx.search_path = vec!["public".to_string()];
                    return Ok(QueryResult::Empty);
                }
                Some(s) => s,
            };
            let schemas: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if schemas.is_empty() {
                return Err(DbError::InvalidValue {
                    reason: "search_path cannot be empty".into(),
                });
            }
            ctx.search_path = schemas;
        }
        "transaction_isolation" | "tx_isolation" => {
            let raw = match set_value_to_setting_string(&stmt.value)? {
                None => {
                    ctx.transaction_isolation = axiomdb_core::IsolationLevel::default();
                    return Ok(QueryResult::Empty);
                }
                Some(s) => s,
            };
            let level =
                axiomdb_core::IsolationLevel::parse(&raw).ok_or_else(|| DbError::InvalidValue {
                    reason: format!("unknown isolation level: '{raw}'"),
                })?;
            // Cannot change isolation level inside an active transaction.
            if ctx.in_explicit_txn {
                return Err(DbError::InvalidValue {
                    reason: "cannot change transaction_isolation inside an active transaction"
                        .into(),
                });
            }
            ctx.transaction_isolation = level;
        }
        "lock_timeout" | "lock_wait_timeout" | "innodb_lock_wait_timeout" => {
            let raw = match set_value_to_setting_string(&stmt.value)? {
                None => {
                    ctx.lock_timeout_secs = 30; // default
                    return Ok(QueryResult::Empty);
                }
                Some(s) => s,
            };
            let secs: u64 = raw.parse().map_err(|_| DbError::InvalidValue {
                reason: format!("lock_timeout: expected integer seconds, got '{raw}'"),
            })?;
            ctx.lock_timeout_secs = secs;
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
        Stmt::CreateTable(s) => execute_create_table(s, storage, txn, DEFAULT_DATABASE_NAME),
        Stmt::CreateDatabase(s) => execute_create_database(s, storage, txn),
        Stmt::CreateSchema(s) => execute_create_schema(s, storage, txn, DEFAULT_DATABASE_NAME),
        Stmt::DropTable(s) => execute_drop_table(s, storage, txn, DEFAULT_DATABASE_NAME),
        Stmt::DropDatabase(_) => Err(DbError::NotImplemented {
            feature: "DROP DATABASE requires session context".into(),
        }),
        Stmt::CreateIndex(s) => {
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_create_index(s, storage, txn, &mut noop_bloom, DEFAULT_DATABASE_NAME)
        }
        Stmt::DropIndex(s) => {
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_drop_index(s, storage, txn, &mut noop_bloom, DEFAULT_DATABASE_NAME)
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
        Stmt::UseDatabase(_) => Err(DbError::NotImplemented {
            feature: "USE requires session context".into(),
        }),
        Stmt::TruncateTable(s) => execute_truncate(s, storage, txn, DEFAULT_DATABASE_NAME),
        Stmt::AlterTable(s) => execute_alter_table(s, storage, txn, DEFAULT_DATABASE_NAME),
        Stmt::ShowDatabases(s) => execute_show_databases(s, storage, txn),
        Stmt::ShowTables(s) => execute_show_tables(s, storage, txn, DEFAULT_DATABASE_NAME),
        Stmt::ShowColumns(s) => execute_show_columns(s, storage, txn, DEFAULT_DATABASE_NAME),
        Stmt::Analyze(_) => Err(DbError::NotImplemented {
            feature: "ANALYZE requires session context — use execute_with_ctx".into(),
        }),
        Stmt::Vacuum(_) => Err(DbError::NotImplemented {
            feature: "VACUUM requires session context — use execute_with_ctx".into(),
        }),
        Stmt::Explain(_) => Err(DbError::NotImplemented {
            feature: "EXPLAIN requires session context — use execute_with_ctx".into(),
        }),
        Stmt::Savepoint(_) | Stmt::RollbackToSavepoint(_) | Stmt::ReleaseSavepoint(_) => {
            Err(DbError::NotImplemented {
                feature: "SAVEPOINT requires session context — use execute_with_ctx".into(),
            })
        }
    }
}

// ── EXPLAIN ─────────────────────────────────────────────────────────────────

/// Executes EXPLAIN: runs the planner on the inner SELECT but does NOT
/// execute the query. Returns the query plan as a result set in MySQL format.
fn execute_explain(
    inner: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    match inner {
        Stmt::Select(s) => explain_select(s, storage, txn, bloom, ctx),
        other => {
            // For non-SELECT, just show the statement type.
            let type_name = match &other {
                Stmt::Insert(_) => "INSERT",
                Stmt::Update(_) => "UPDATE",
                Stmt::Delete(_) => "DELETE",
                _ => "OTHER",
            };
            let columns = explain_columns();
            let rows = vec![vec![
                Value::Int(1),
                Value::Text("SIMPLE".into()),
                Value::Text("-".into()),
                Value::Text(type_name.into()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]];
            Ok(QueryResult::Rows { columns, rows })
        }
    }
}

fn explain_select(
    stmt: SelectStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    _bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let columns = explain_columns();

    // Resolve table (same as execute_select_ctx).
    let from = stmt
        .from
        .as_ref()
        .ok_or(DbError::Other("EXPLAIN requires a FROM clause".into()))?;
    let from_table_ref = match from {
        crate::ast::FromClause::Table(t) => t,
        crate::ast::FromClause::Subquery { .. } => {
            return Err(DbError::Other(
                "EXPLAIN for subquery FROM not yet supported".into(),
            ))
        }
    };

    let resolved = resolve_table_cached(storage, txn, ctx, from_table_ref)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());

    // Load stats + run planner (same as execute_select_ctx).
    let secondary_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    let table_stats: Vec<axiomdb_catalog::StatsDef> = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.list_stats(resolved.def.id).unwrap_or_default()
    };

    let select_col_idxs: Vec<usize> = stmt
        .columns
        .iter()
        .filter_map(|item| match item {
            crate::ast::SelectItem::Expr {
                expr: crate::expr::Expr::Column { col_idx, .. },
                ..
            } => Some(*col_idx),
            _ => None,
        })
        .collect();

    let effective_coll = ctx.effective_collation();
    let select_col_idxs_u16: Vec<u16> = select_col_idxs.iter().map(|&i| i as u16).collect();
    let access_method = crate::planner::plan_select_ctx(
        stmt.where_clause.as_ref(),
        &secondary_indexes,
        &resolved.columns,
        resolved.def.id,
        &table_stats,
        &mut ctx.stats,
        &select_col_idxs_u16,
        effective_coll,
    );

    // Format the plan as MySQL EXPLAIN row.
    let table_name = &resolved.def.table_name;
    let row_count = table_stats.first().map(|s| s.row_count).unwrap_or(0);

    let (access_type, key_name, key_len, ref_val, est_rows, extra) = match &access_method {
        crate::planner::AccessMethod::Scan => (
            "ALL",
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Int(row_count as i32),
            if stmt.where_clause.is_some() {
                "Using where"
            } else {
                ""
            },
        ),
        crate::planner::AccessMethod::IndexLookup { index_def, .. } => (
            if index_def.is_unique || index_def.is_primary {
                "const"
            } else {
                "ref"
            },
            Value::Text(index_def.name.clone()),
            Value::Int(index_def.columns.len() as i32),
            Value::Text("const".into()),
            Value::Int(1),
            if stmt.where_clause.is_some() {
                "Using where"
            } else {
                ""
            },
        ),
        crate::planner::AccessMethod::IndexRange { index_def, .. } => {
            let ndv = table_stats
                .iter()
                .find(|s| s.col_idx == index_def.columns[0].col_idx)
                .map(|s| s.ndv.max(1) as u64)
                .unwrap_or(200);
            let est = (row_count / ndv).max(1);
            (
                "range",
                Value::Text(index_def.name.clone()),
                Value::Int(index_def.columns.len() as i32),
                Value::Null,
                Value::Int(est as i32),
                "Using where; Using index condition",
            )
        }
        crate::planner::AccessMethod::IndexOnlyScan { index_def, .. } => {
            let ndv = table_stats
                .iter()
                .find(|s| s.col_idx == index_def.columns[0].col_idx)
                .map(|s| s.ndv.max(1) as u64)
                .unwrap_or(200);
            let est = (row_count / ndv).max(1);
            (
                "index",
                Value::Text(index_def.name.clone()),
                Value::Int(index_def.columns.len() as i32),
                Value::Null,
                Value::Int(est as i32),
                "Using index",
            )
        }
    };

    // Possible keys: all indexes on the table.
    let possible_keys = if secondary_indexes.is_empty() {
        Value::Null
    } else {
        Value::Text(
            secondary_indexes
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>()
                .join(","),
        )
    };

    let rows = vec![vec![
        Value::Int(1),                   // id
        Value::Text("SIMPLE".into()),    // select_type
        Value::Text(table_name.clone()), // table
        Value::Text(access_type.into()), // type
        possible_keys,                   // possible_keys
        key_name,                        // key
        key_len,                         // key_len
        ref_val,                         // ref
        est_rows,                        // rows
        Value::Text(extra.into()),       // Extra
    ]];

    Ok(QueryResult::Rows { columns, rows })
}

fn explain_columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta::computed("id", DataType::Int),
        ColumnMeta::computed("select_type", DataType::Text),
        ColumnMeta::computed("table", DataType::Text),
        ColumnMeta::computed("type", DataType::Text),
        ColumnMeta::computed("possible_keys", DataType::Text),
        ColumnMeta::computed("key", DataType::Text),
        ColumnMeta::computed("key_len", DataType::Int),
        ColumnMeta::computed("ref", DataType::Text),
        ColumnMeta::computed("rows", DataType::Int),
        ColumnMeta::computed("Extra", DataType::Text),
    ]
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
include!("staging.rs");

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
            index_type: 0,
            pages_per_range: 128,
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
