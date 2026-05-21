pub fn execute_with_ctx(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    execute_with_ctx_locked(stmt, storage, txn, bloom, None, ctx)
}

/// Attack 6: opens a `ConnectionTxn` and stamps it with the session's
/// current `synchronous` durability override. Every `txn.begin()` site
/// in `execute_with_ctx_locked` routes through here so `commit()` sees
/// the per-session policy.
#[inline]
fn begin_session_txn(
    txn: &TxnManager,
    ctx: &SessionContext,
) -> Result<axiomdb_wal::ConnectionTxn, DbError> {
    let mut t = txn.begin()?;
    t.durability_override = Some(ctx.synchronous().to_wal_policy());
    Ok(t)
}

/// Like [`begin_session_txn`] but with an explicit isolation level
/// (used by `BEGIN ... ISOLATION LEVEL ...`).
#[inline]
fn begin_session_txn_with_isolation(
    txn: &TxnManager,
    level: axiomdb_core::IsolationLevel,
    ctx: &SessionContext,
) -> Result<axiomdb_wal::ConnectionTxn, DbError> {
    let mut t = txn.begin_with_isolation(level)?;
    t.durability_override = Some(ctx.synchronous().to_wal_policy());
    Ok(t)
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
    // BACKUP/RESTORE run their own checkpoint — bypass all transaction wrappers.
    if matches!(&stmt, Stmt::Backup(_) | Stmt::Restore(_)) {
        return match stmt {
            Stmt::Backup(b) => execute_backup(b, storage, txn),
            Stmt::Restore(r) => execute_restore(r),
            _ => unreachable!(),
        };
    }
    let exec_ctx = ExecutionContext::new(storage, txn, bloom, lock_mgr);
    if ctx.conn_txn.is_some() {
        match &stmt {
            Stmt::Commit => {
                // Flush any staged rows before writing the Commit WAL entry.
                flush_pending_inserts_ctx(&exec_ctx, ctx)?;
                flush_clustered_insert_batch(&exec_ctx, ctx)?;
                ctx.in_explicit_txn = false;
                ctx.close_all_cursors();
                ctx.savepoints.clear(); // all savepoints destroyed on COMMIT
                let tid = ctx
                    .conn_txn
                    .as_ref()
                    .expect("conn_txn: checked by is_some() guard")
                    .txn_id;
                ctx.pending_deferred_txn_id = commit_active_txn(txn, storage, bloom, ctx)?;
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
                ctx.close_all_cursors();
                ctx.savepoints.clear(); // all savepoints destroyed on ROLLBACK
                ctx.clear_deferred_fk_constraints();
                ctx.clear_pending_notifications();
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
                ctx.savepoints.push((
                    name.clone(),
                    SessionSavepoint {
                        wal: sp,
                        deferred_fk_len: ctx.deferred_fk_constraint_ids.len(),
                        pending_notify_len: ctx.pending_notification_len(),
                    },
                ));
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
                        ctx.truncate_deferred_fk_constraints(sp.deferred_fk_len);
                        ctx.truncate_pending_notifications(sp.pending_notify_len);
                        let conn = ctx.conn_txn.as_mut().expect("conn_txn: checked by is_some() guard");
                        rollback_to_savepoint_with_index_undo(txn, conn, sp.wal, storage, bloom)?;
                        // Invalidate schema cache: rollback_to_savepoint reverts
                        // catalog changes (e.g. update_table_root from bulk DELETE)
                        // whose root_page_id or schema_version may differ from what
                        // the cache holds. Clearing ensures the next query re-resolves
                        // from the rolled-back catalog state.
                        ctx.invalidate_all();
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
                ctx.close_all_cursors();
                ctx.savepoints.clear();
                let pre_tid = ctx.conn_txn.as_ref().expect("conn_txn: checked by is_some() guard").txn_id;
            let pre_ddl_pending = commit_active_txn(txn, storage, bloom, ctx)?;
            // DDL is a hard commit boundary: the statement below runs in a fresh
            // transaction (started further down) and must observe the rows just
            // committed — e.g. CREATE INDEX bulk-builds over existing rows. In
            // deferred (group-commit) mode the commit above removes the txn from
            // the active set but does NOT advance `max_committed`; that is
            // normally driven by the network layer only AFTER this call returns.
            // Drive it here so the new DDL snapshot sees the pre-DDL writes.
            if let Some(pending) = pre_ddl_pending {
                txn.wal_flush_and_fsync()?;
                txn.advance_committed_single(pending);
                txn.release_committed_frees(storage, &[pending])?;
            }
            txn.release_immediate_committed_frees(storage, pre_tid)?;
            txn.drain_committed_page_batches(storage)?;
            if let Some(lm) = lock_mgr { lm.release_all_for_txn(pre_tid); }
            ctx.conn_txn = Some(begin_session_txn(txn, ctx)?);
            let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
            return match dispatch_ctx(stmt, &exec_ctx2, ctx) {
                Ok(result) => {
                    let ddl_tid = ctx.conn_txn.as_ref().expect("conn_txn: set by begin() on preceding line").txn_id;
                    ctx.pending_deferred_txn_id = commit_active_txn(txn, storage, bloom, ctx)?;
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
        let sp_opt: Option<SessionSavepoint> = if ctx.on_error == OnErrorMode::RollbackTransaction {
            None
        } else {
            Some(SessionSavepoint {
                wal: txn.savepoint(
                    ctx.conn_txn
                        .as_ref()
                        .expect("conn_txn: checked by is_some() guard"),
                ),
                deferred_fk_len: ctx.deferred_fk_constraint_ids.len(),
                pending_notify_len: ctx.pending_notification_len(),
            })
        };
        match dispatch_ctx(stmt, &exec_ctx, ctx) {
            Ok(result) => Ok(result),
            Err(e) => match ctx.on_error {
                OnErrorMode::RollbackTransaction => {
                    ctx.discard_pending_inserts();
                    ctx.discard_clustered_insert_batch();
                    ctx.close_all_cursors();
                    ctx.clear_deferred_fk_constraints();
                    ctx.clear_pending_notifications();
                    let conn = ctx.conn_txn.take().expect("conn_txn: checked by is_some() guard");
                    let _ = rollback_with_index_undo(txn, conn, storage, bloom);
                    Err(e)
                }
                OnErrorMode::Ignore if crate::session::is_ignorable_on_error(&e) => {
                    if let Some(sp) = sp_opt {
                        ctx.truncate_deferred_fk_constraints(sp.deferred_fk_len);
                        ctx.truncate_pending_notifications(sp.pending_notify_len);
                        if let Some(conn) = ctx.conn_txn.as_mut() {
                            let _ = rollback_to_savepoint_with_index_undo(
                                txn, conn, sp.wal, storage, bloom,
                            );
                        }
                    }
                    Err(e)
                }
                OnErrorMode::Ignore => {
                    ctx.discard_pending_inserts();
                    ctx.discard_clustered_insert_batch();
                    ctx.close_all_cursors();
                    ctx.clear_deferred_fk_constraints();
                    ctx.clear_pending_notifications();
                    let conn = ctx.conn_txn.take().expect("conn_txn: checked by is_some() guard");
                    let _ = rollback_with_index_undo(txn, conn, storage, bloom);
                    Err(e)
                }
                _ => {
                    if let Some(sp) = sp_opt {
                        ctx.truncate_deferred_fk_constraints(sp.deferred_fk_len);
                        ctx.truncate_pending_notifications(sp.pending_notify_len);
                        if let Some(conn) = ctx.conn_txn.as_mut() {
                            let _ = rollback_to_savepoint_with_index_undo(
                                txn, conn, sp.wal, storage, bloom,
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
                ctx.conn_txn = Some(begin_session_txn_with_isolation(txn, level, ctx)?);
                ctx.in_explicit_txn = true;
                Ok(QueryResult::Empty)
            }
            Stmt::Checkpoint => dispatch_ctx(
                Stmt::Checkpoint,
                &ExecutionContext::new(storage, txn, bloom, lock_mgr),
                ctx,
            ),
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
                ctx.conn_txn = Some(begin_session_txn(txn, ctx)?);
                // NOTE: `in_explicit_txn` is NOT set here — this is an implicit
                // autocommit transaction. Single-statement INSERTs use the existing
                // multi-row batch path inside execute_insert_ctx, not the staging buffer.
                let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
                match dispatch_ctx(other, &exec_ctx2, ctx) {
                    Ok(result) => {
                        let tid = ctx.conn_txn.as_ref().expect("conn_txn: set by begin() on preceding line").txn_id;
                        ctx.pending_deferred_txn_id = commit_active_txn(txn, storage, bloom, ctx)?;
                        txn.release_immediate_committed_frees(storage, tid)?;
                        txn.drain_committed_page_batches(storage)?;
                        if let Some(lm) = lock_mgr { lm.release_all_for_txn(tid); }
                        Ok(result)
                    }
                    Err(e) => {
                        ctx.clear_deferred_fk_constraints();
                        ctx.clear_pending_notifications();
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
                ctx.conn_txn = Some(begin_session_txn(txn, ctx)?);
                ctx.in_explicit_txn = true;
                Ok(QueryResult::Empty)
            }
            Stmt::Checkpoint => dispatch_ctx(
                Stmt::Checkpoint,
                &ExecutionContext::new(storage, txn, bloom, lock_mgr),
                ctx,
            ),
            Stmt::Commit => {
                ctx.warn(1592, "There is no active transaction");
                Ok(QueryResult::Empty)
            }
            Stmt::Rollback => {
                ctx.warn(1592, "There is no active transaction");
                Ok(QueryResult::Empty)
            }
            Stmt::Select(_) => {
                ctx.conn_txn = Some(begin_session_txn(txn, ctx)?);
                let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
                match dispatch_ctx(stmt, &exec_ctx2, ctx) {
                    Ok(result) => {
                        let tid = ctx
                            .conn_txn
                            .as_ref()
                            .expect("conn_txn: set by begin() on preceding line")
                            .txn_id;
                        commit_active_txn(txn, storage, bloom, ctx)?;
                        if let Some(lm) = lock_mgr {
                            lm.release_all_for_txn(tid);
                        }
                        Ok(result)
                    }
                    Err(e) => {
                        ctx.clear_deferred_fk_constraints();
                        let conn = ctx
                            .conn_txn
                            .take()
                            .expect("conn_txn: set by begin() on preceding line");
                        let tid = conn.txn_id;
                        let _ = rollback_with_index_undo(txn, conn, storage, bloom);
                        if let Some(lm) = lock_mgr {
                            lm.release_all_for_txn(tid);
                        }
                        Err(e)
                    }
                }
            }
            other if is_ddl(&other) => {
                ctx.conn_txn = Some(begin_session_txn(txn, ctx)?);
                let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
                match dispatch_ctx(other, &exec_ctx2, ctx) {
                    Ok(result) => {
                        ctx.pending_deferred_txn_id = commit_active_txn(txn, storage, bloom, ctx)?;
                        Ok(result)
                    }
                    Err(e) => {
                        ctx.clear_deferred_fk_constraints();
                        let conn = ctx.conn_txn.take().expect("conn_txn: set by begin() on preceding line");
                        let _ = rollback_with_index_undo(txn, conn, storage, bloom);
                        Err(e)
                    }
                }
            }
            other => {
                ctx.conn_txn = Some(begin_session_txn(txn, ctx)?);
                let sp_opt: Option<SessionSavepoint> = if ctx.on_error == OnErrorMode::Savepoint
                    || ctx.on_error == OnErrorMode::Ignore
                {
                    Some(SessionSavepoint {
                        wal: txn.savepoint(
                            ctx.conn_txn
                                .as_ref()
                                .expect("conn_txn: set by begin() on preceding line"),
                        ),
                        deferred_fk_len: ctx.deferred_fk_constraint_ids.len(),
                        pending_notify_len: ctx.pending_notification_len(),
                    })
                } else {
                    None
                };
                let exec_ctx2 = ExecutionContext::new(storage, txn, bloom, lock_mgr);
                match dispatch_ctx(other, &exec_ctx2, ctx) {
                    Ok(result) => Ok(result),
                    Err(e) => match ctx.on_error {
                        OnErrorMode::Ignore if crate::session::is_ignorable_on_error(&e) => {
                            if let Some(sp) = sp_opt {
                                ctx.truncate_deferred_fk_constraints(sp.deferred_fk_len);
                                ctx.truncate_pending_notifications(sp.pending_notify_len);
                                if let Some(conn) = ctx.conn_txn.as_mut() {
                                    let _ = rollback_to_savepoint_with_index_undo(
                                        txn, conn, sp.wal, storage, bloom,
                                    );
                                }
                            }
                            Err(e)
                        }
                        OnErrorMode::Savepoint => {
                            if let Some(sp) = sp_opt {
                                ctx.truncate_deferred_fk_constraints(sp.deferred_fk_len);
                                ctx.truncate_pending_notifications(sp.pending_notify_len);
                                if let Some(conn) = ctx.conn_txn.as_mut() {
                                    let _ = rollback_to_savepoint_with_index_undo(
                                        txn, conn, sp.wal, storage, bloom,
                                    );
                                }
                            }
                            Err(e)
                        }
                        _ => {
                            ctx.clear_deferred_fk_constraints();
                            ctx.clear_pending_notifications();
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

fn commit_active_txn(
    txn: &TxnManager,
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<Option<axiomdb_core::TxnId>, DbError> {
    let conn = ctx
        .conn_txn
        .take()
        .expect("conn_txn: active transaction required");
    if let Err(e) = crate::fk_enforcement::validate_deferred_foreign_keys(
        &ctx.deferred_fk_constraint_ids,
        storage,
        txn,
        &conn,
        bloom,
    ) {
        ctx.clear_deferred_fk_constraints();
        let _ = rollback_with_index_undo(txn, conn, storage, bloom);
        return Err(e);
    }
    ctx.clear_deferred_fk_constraints();
    let commit = txn.commit(conn)?;
    ctx.flush_pending_notifications();
    Ok(commit)
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
            | Stmt::CreateMaterializedView(_)
            | Stmt::CreateAggregate(_)
            | Stmt::CreateSequence(_)
            | Stmt::CreateEnumType(_)
            | Stmt::CreateCompositeType(_)
            | Stmt::CreateDatabase(_)
            | Stmt::DropTable(_)
            | Stmt::DropMaterializedView(_)
            | Stmt::DropAggregate(_)
            | Stmt::DropSequence(_)
            | Stmt::DropEnumType(_)
            | Stmt::DropCompositeType(_)
            | Stmt::DropDatabase(_)
            | Stmt::CreateIndex(_)
            | Stmt::DropIndex(_)
            | Stmt::RefreshMaterializedView(_)
            | Stmt::AlterTable(_)
            | Stmt::TruncateTable(_)
    )
}
