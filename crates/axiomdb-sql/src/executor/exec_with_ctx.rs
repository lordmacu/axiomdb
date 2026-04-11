pub fn execute_with_ctx(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    execute_with_ctx_locked(stmt, storage, txn, bloom, None, ctx)
}

/// Like [`execute_with_ctx`] but accepts an optional `LockManager` for
/// row-level lock acquisition (Phase 40.11).
pub fn execute_with_ctx_locked(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &crate::bloom::BloomRegistry,
    lock_mgr: Option<&axiomdb_lock::LockManager>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let exec_ctx = ExecutionContext::new(storage, txn, bloom, lock_mgr);
    if ctx.conn_txn.is_some() {
        match &stmt {
            Stmt::Commit => {
                // Flush any staged rows before writing the Commit WAL entry.
                flush_pending_inserts_ctx(&exec_ctx, ctx)?;
                flush_clustered_insert_batch(&exec_ctx, ctx)?;
                ctx.in_explicit_txn = false;
                ctx.savepoints.clear(); // all savepoints destroyed on COMMIT
                let conn = ctx.conn_txn.take().expect("conn_txn: checked by is_some() guard");
                let tid = conn.txn_id;
                ctx.pending_deferred_txn_id = txn.commit(conn)?;
                txn.release_immediate_committed_frees(storage, tid)?;
                txn.drain_committed_page_batches(storage)?;
                // Phase 40.11: release all row+table locks held by this txn.
                // InnoDB `lock_trx_release_locks()` pattern: always after WAL commit.
                if let Some(lm) = lock_mgr {
                    lm.release_all_for_txn(tid);
                }
                return Ok(QueryResult::Empty);
            }
            Stmt::Rollback => {
                // Discard staged rows without writing to heap or WAL.
                ctx.discard_pending_inserts();
                ctx.discard_clustered_insert_batch();
                ctx.savepoints.clear(); // all savepoints destroyed on ROLLBACK
                let conn = ctx.conn_txn.take().expect("conn_txn: checked by is_some() guard");
                let tid = conn.txn_id;
                let result = rollback_with_index_undo(txn, conn, storage, bloom);
                // Phase 40.11: release all locks on rollback too.
                if let Some(lm) = lock_mgr {
                    lm.release_all_for_txn(tid);
                }
                return result.map(|_| QueryResult::Empty);
            }
            Stmt::Begin => {
                let txn_id = ctx.conn_txn.as_ref().map(|c| c.txn_id).unwrap_or(0);
                return Err(DbError::TransactionAlreadyActive { txn_id });
            }
            Stmt::Savepoint(ref name) => {
                flush_pending_inserts_ctx(&exec_ctx, ctx)?;
                flush_clustered_insert_batch(&exec_ctx, ctx)?;
                let sp = txn.savepoint(ctx.conn_txn.as_ref().expect("conn_txn: checked by is_some() guard"));
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
                        ctx.discard_clustered_insert_batch();
                        let sp = ctx.savepoints[idx].1;
                        let conn = ctx.conn_txn.as_mut().expect("conn_txn: checked by is_some() guard");
                        rollback_to_savepoint_with_index_undo(txn, conn, sp, storage, bloom)?;
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
            flush_pending_inserts_ctx(&exec_ctx, ctx)?;
            flush_clustered_insert_batch(&exec_ctx, ctx)?;
            ctx.in_explicit_txn = false;
            let pre_conn = ctx.conn_txn.take().expect("conn_txn: checked by is_some() guard");
            let pre_tid = pre_conn.txn_id;
            // Pre-DDL commit: discard any pending deferred (pipeline handles it).
            let _ = txn.commit(pre_conn)?;
            txn.release_immediate_committed_frees(storage, pre_tid)?;
            txn.drain_committed_page_batches(storage)?;
            if let Some(lm) = lock_mgr { lm.release_all_for_txn(pre_tid); }
            ctx.conn_txn = Some(txn.begin()?);
            let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
            return match dispatch_ctx(stmt, &exec_ctx2, ctx) {
                Ok(result) => {
                    let ddl_conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                    let ddl_tid = ddl_conn.txn_id;
                    ctx.pending_deferred_txn_id = txn.commit(ddl_conn)?;
                    txn.release_immediate_committed_frees(storage, ddl_tid)?;
                    txn.drain_committed_page_batches(storage)?;
                    Ok(result)
                }
                Err(e) => {
                    let ddl_conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                    let _ = rollback_with_index_undo(txn, ddl_conn, storage, bloom);
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
            flush_pending_inserts_ctx(&exec_ctx, ctx)?;
        }
        if should_flush_clustered_batch_before_stmt(&stmt, ctx) {
            flush_clustered_insert_batch(&exec_ctx, ctx)?;
        }
        let sp_opt: Option<Savepoint> = if ctx.on_error == OnErrorMode::RollbackTransaction {
            None
        } else {
            Some(txn.savepoint(ctx.conn_txn.as_ref().expect("conn_txn: checked by is_some() guard")))
        };
        match dispatch_ctx(stmt, &exec_ctx, ctx) {
            Ok(result) => Ok(result),
            Err(e) => match ctx.on_error {
                OnErrorMode::RollbackTransaction => {
                    ctx.discard_pending_inserts();
                    ctx.discard_clustered_insert_batch();
                    let conn = ctx.conn_txn.take().expect("conn_txn: checked by is_some() guard");
                    let _ = rollback_with_index_undo(txn, conn, storage, bloom);
                    Err(e)
                }
                OnErrorMode::Ignore if crate::session::is_ignorable_on_error(&e) => {
                    if let Some(sp) = sp_opt {
                        if let Some(conn) = ctx.conn_txn.as_mut() {
                            let _ = rollback_to_savepoint_with_index_undo(
                                txn, conn, sp, storage, bloom,
                            );
                        }
                    }
                    Err(e)
                }
                OnErrorMode::Ignore => {
                    ctx.discard_pending_inserts();
                    ctx.discard_clustered_insert_batch();
                    let conn = ctx.conn_txn.take().expect("conn_txn: checked by is_some() guard");
                    let _ = rollback_with_index_undo(txn, conn, storage, bloom);
                    Err(e)
                }
                _ => {
                    if let Some(sp) = sp_opt {
                        if let Some(conn) = ctx.conn_txn.as_mut() {
                            let _ = rollback_to_savepoint_with_index_undo(
                                txn, conn, sp, storage, bloom,
                            );
                        }
                    }
                    Err(e)
                }
            },
        }
    } else if ctx.autocommit {
        match stmt {
            Stmt::Begin => {
                let level = ctx.effective_isolation();
                ctx.conn_txn = Some(txn.begin_with_isolation(level)?);
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
                ctx.conn_txn = Some(txn.begin()?);
                // NOTE: `in_explicit_txn` is NOT set here — this is an implicit
                // autocommit transaction. Single-statement INSERTs use the existing
                // multi-row batch path inside execute_insert_ctx, not the staging buffer.
                let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
                match dispatch_ctx(other, &exec_ctx2, ctx) {
                    Ok(result) => {
                        let conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                        let tid = conn.txn_id;
                        ctx.pending_deferred_txn_id = txn.commit(conn)?;
                        txn.release_immediate_committed_frees(storage, tid)?;
                        txn.drain_committed_page_batches(storage)?;
                        if let Some(lm) = lock_mgr { lm.release_all_for_txn(tid); }
                        Ok(result)
                    }
                    Err(e) => {
                        let conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                        let tid = conn.txn_id;
                        let _ = rollback_with_index_undo(txn, conn, storage, bloom);
                        if let Some(lm) = lock_mgr { lm.release_all_for_txn(tid); }
                        Err(e)
                    }
                }
            }
        }
    } else {
        match stmt {
            Stmt::Begin => {
                ctx.conn_txn = Some(txn.begin()?);
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
                ctx.conn_txn = Some(txn.begin()?);
                let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
                match dispatch_ctx(stmt, &exec_ctx2, ctx) {
                    Ok(result) => {
                        let conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                        let _ = txn.commit(conn)?;
                        Ok(result)
                    }
                    Err(e) => {
                        let conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                        let _ = rollback_with_index_undo(txn, conn, storage, bloom);
                        Err(e)
                    }
                }
            }
            other if is_ddl(&other) => {
                ctx.conn_txn = Some(txn.begin()?);
                let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
                match dispatch_ctx(other, &exec_ctx2, ctx) {
                    Ok(result) => {
                        let conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                        ctx.pending_deferred_txn_id = txn.commit(conn)?;
                        Ok(result)
                    }
                    Err(e) => {
                        let conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                        let _ = rollback_with_index_undo(txn, conn, storage, bloom);
                        Err(e)
                    }
                }
            }
            other => {
                ctx.conn_txn = Some(txn.begin()?);
                let sp_opt: Option<Savepoint> = if ctx.on_error == OnErrorMode::Savepoint
                    || ctx.on_error == OnErrorMode::Ignore
                {
                    Some(txn.savepoint(ctx.conn_txn.as_ref().expect("conn_txn: set by begin() on preceding line")))
                } else {
                    None
                };
                let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
                match dispatch_ctx(other, &exec_ctx2, ctx) {
                    Ok(result) => Ok(result),
                    Err(e) => match ctx.on_error {
                        OnErrorMode::Ignore if crate::session::is_ignorable_on_error(&e) => {
                            if let Some(sp) = sp_opt {
                                if let Some(conn) = ctx.conn_txn.as_mut() {
                                    let _ = rollback_to_savepoint_with_index_undo(
                                        txn, conn, sp, storage, bloom,
                                    );
                                }
                            }
                            Err(e)
                        }
                        OnErrorMode::Savepoint => {
                            if let Some(sp) = sp_opt {
                                if let Some(conn) = ctx.conn_txn.as_mut() {
                                    let _ = rollback_to_savepoint_with_index_undo(
                                        txn, conn, sp, storage, bloom,
                                    );
                                }
                            }
                            Err(e)
                        }
                        _ => {
                            let conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                            let _ = rollback_with_index_undo(txn, conn, storage, bloom);
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

fn should_flush_clustered_batch_before_stmt(stmt: &Stmt, ctx: &SessionContext) -> bool {
    let batch = match ctx.clustered_insert_batch.as_ref() {
        Some(b) => b,
        None => return false,
    };

    // Do NOT flush when the next statement is a VALUES INSERT into the same
    // clustered table — those rows will be appended to the existing batch.
    !matches!(
        stmt,
        Stmt::Insert(insert)
            if ctx.in_explicit_txn
                && matches!(insert.source, InsertSource::Values(_))
                && insert.table.name == batch.table_def.table_name
                && insert
                    .table
                    .schema
                    .as_deref()
                    .is_none_or(|schema| schema == batch.table_def.schema_name)
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

