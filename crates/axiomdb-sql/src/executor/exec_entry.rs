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
    let exec_ctx = ExecutionContext::new(storage, txn, bloom);
    match stmt {
        Stmt::Select(s) => {
            let conn = ctx.conn_txn.take();
            let r = execute_select_ctx(s, &exec_ctx, conn.as_ref(), ctx);
            ctx.conn_txn = conn;
            r
        }
        Stmt::ShowTables(mut s) => {
            if s.schema.is_none() {
                s.schema = Some(ctx.current_schema().to_string());
            }
            let db = ctx.effective_database();
            let schema = s.schema.as_deref().unwrap_or("public");
            let snap = txn.snapshot();
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
            let snap = txn.snapshot();
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
    // If a transaction was previously started via `execute(BEGIN, ...)`, retrieve the
    // stored `ConnectionTxn` from the thread-local and pass it to `dispatch`.
    let stored_conn = EXECUTE_CONN.with(|cell| cell.borrow_mut().take());
    if let Some(mut conn) = stored_conn {
        match stmt {
            Stmt::Commit => {
                let tid = conn.txn_id;
                txn.commit(conn)?;
                txn.release_immediate_committed_frees(storage, tid)?;
                txn.drain_committed_page_batches(storage)?;
                return Ok(QueryResult::Empty);
            }
            Stmt::Rollback => {
                let _ = txn.rollback(conn, storage);
                return Ok(QueryResult::Empty);
            }
            other => {
                let result = dispatch(other, storage, txn, &mut conn);
                // Put the conn back for the next call in the same explicit transaction.
                EXECUTE_CONN.with(|cell| *cell.borrow_mut() = Some(conn));
                return result;
            }
        }
    }

    // No existing explicit transaction — autocommit or BEGIN.
    match stmt {
        Stmt::Begin => {
            // Store the ConnectionTxn in the thread-local so subsequent execute()
            // calls within the same explicit transaction can retrieve it.
            let conn = txn.begin()?;
            EXECUTE_CONN.with(|cell| *cell.borrow_mut() = Some(conn));
            Ok(QueryResult::Empty)
        }
        Stmt::Commit => Err(DbError::NoActiveTransaction),
        Stmt::Rollback => Err(DbError::NoActiveTransaction),
        other => {
            let mut conn = txn.begin()?;
            let tid = conn.txn_id;
            match dispatch(other, storage, txn, &mut conn) {
                Ok(result) => {
                    txn.commit(conn)?;
                    txn.release_immediate_committed_frees(storage, tid)?;
                    txn.drain_committed_page_batches(storage)?;
                    Ok(result)
                }
                Err(e) => {
                    let _ = txn.rollback(conn, storage);
                    Err(e)
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
    conn_txn: ConnectionTxn,
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    // Collect index insert undos BEFORE rollback (rollback consumes the undo log).
    let index_undos = txn.collect_index_undos(&conn_txn);
    let mut current_roots = load_current_index_roots(txn, &conn_txn, storage, &index_undos)?;
    // conn_txn needed for CatalogWriter; we need it by ref until after the loop.
    // We'll re-borrow it after the loop for the actual rollback.
    // Split: collect mutations, then apply rollback.
    let mut root_updates: Vec<(u32, u64)> = Vec::new();
    for undo in &index_undos {
        let (index_id, fallback_root) = match undo {
            IndexUndoRecord::DeleteInserted {
                index_id,
                root_page_id,
                ..
            }
            | IndexUndoRecord::RestoreDeleted {
                index_id,
                root_page_id,
                ..
            } => (*index_id, *root_page_id),
        };
        let current_root = current_roots
            .get(&index_id)
            .copied()
            .unwrap_or(fallback_root);
        let root = std::sync::atomic::AtomicU64::new(current_root);

        match undo {
            IndexUndoRecord::DeleteInserted { key, .. } => {
                // Best-effort: if the key is already absent (idempotent), ignore the error.
                let _ = BTree::delete_in(storage, &root, key);
                bloom.mark_dirty(index_id);
            }
            IndexUndoRecord::RestoreDeleted {
                key,
                rid,
                fillfactor,
                ..
            } => {
                BTree::insert_in(storage, &root, key, *rid, *fillfactor)?;
                bloom.add(index_id, key);
            }
        }

        let new_root = root.load(std::sync::atomic::Ordering::Acquire);
        current_roots.insert(index_id, new_root);
        if new_root != current_root {
            root_updates.push((index_id, new_root));
        }
    }
    // Apply index root updates to catalog — needs a short-lived mutable conn for CatalogWriter.
    // We use conn_txn for this (it still has the WAL scratch buffer).
    // We can't use conn_txn after rollback() consumes it, so do catalog updates first.
    {
        // Temporarily reborrow conn_txn as mut for catalog writes.
        // Since we have ownership of conn_txn, create a temporary copy of state for catalog.
        // Actually: conn_txn is owned by this function. Pass it by &mut to CatalogWriter,
        // then pass by value to rollback(). Split into two phases:
        let mut borrowed_conn = conn_txn;
        for (index_id, new_root) in &root_updates {
            if let Ok(mut cw) = CatalogWriter::new(storage, txn, &mut borrowed_conn) {
                let _ = cw.update_index_root(*index_id, *new_root);
            }
        }
        txn.rollback(borrowed_conn, storage)
    }
}

/// Like [`rollback_with_index_undo`] but for savepoint rollback.
fn rollback_to_savepoint_with_index_undo(
    txn: &mut TxnManager,
    conn_txn: &mut ConnectionTxn,
    sp: Savepoint,
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    let index_undos = txn.collect_index_undos_since(conn_txn, &sp);
    let mut current_roots = load_current_index_roots(txn, conn_txn, storage, &index_undos)?;
    let mut root_updates: Vec<(u32, u64)> = Vec::new();
    for undo in &index_undos {
        let (index_id, fallback_root) = match undo {
            IndexUndoRecord::DeleteInserted {
                index_id,
                root_page_id,
                ..
            }
            | IndexUndoRecord::RestoreDeleted {
                index_id,
                root_page_id,
                ..
            } => (*index_id, *root_page_id),
        };
        let current_root = current_roots
            .get(&index_id)
            .copied()
            .unwrap_or(fallback_root);
        let root = std::sync::atomic::AtomicU64::new(current_root);

        match undo {
            IndexUndoRecord::DeleteInserted { key, .. } => {
                let _ = BTree::delete_in(storage, &root, key);
                bloom.mark_dirty(index_id);
            }
            IndexUndoRecord::RestoreDeleted {
                key,
                rid,
                fillfactor,
                ..
            } => {
                BTree::insert_in(storage, &root, key, *rid, *fillfactor)?;
                bloom.add(index_id, key);
            }
        }

        let new_root = root.load(std::sync::atomic::Ordering::Acquire);
        current_roots.insert(index_id, new_root);
        if new_root != current_root {
            root_updates.push((index_id, new_root));
        }
    }
    for (index_id, new_root) in root_updates {
        if let Ok(mut cw) = CatalogWriter::new(storage, txn, conn_txn) {
            let _ = cw.update_index_root(index_id, new_root);
        }
    }
    txn.rollback_to_savepoint(conn_txn, sp, storage)
}

fn load_current_index_roots(
    txn: &TxnManager,
    conn_txn: &ConnectionTxn,
    storage: &dyn StorageEngine,
    index_undos: &[IndexUndoRecord],
) -> Result<std::collections::HashMap<u32, u64>, DbError> {
    let mut roots = std::collections::HashMap::new();
    if index_undos.is_empty() {
        return Ok(roots);
    }

    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    for undo in index_undos {
        let (index_id, fallback_root) = match undo {
            IndexUndoRecord::DeleteInserted {
                index_id,
                root_page_id,
                ..
            }
            | IndexUndoRecord::RestoreDeleted {
                index_id,
                root_page_id,
                ..
            } => (*index_id, *root_page_id),
        };
        if roots.contains_key(&index_id) {
            continue;
        }
        let root = reader
            .get_index_by_id(index_id)?
            .map(|idx| idx.root_page_id)
            .unwrap_or(fallback_root);
        roots.insert(index_id, root);
    }
    Ok(roots)
}
