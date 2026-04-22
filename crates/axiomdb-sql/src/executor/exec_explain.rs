// ── Dispatch ─────────────────────────────────────────────────────────────────

/// Routes a statement to its handler. Called both inside `autocommit` and
/// directly when an explicit transaction is already active.
fn dispatch(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
) -> Result<QueryResult, DbError> {
    match stmt {
        Stmt::Select(s) => execute_select(s, storage, txn, Some(conn_txn)),
        Stmt::SetOp { .. } => Err(DbError::NotImplemented {
            feature: "UNION/INTERSECT/EXCEPT in legacy dispatch — use session-aware path".into(),
        }),
        Stmt::Insert(s) => {
            let table_ref = s.table.clone();
            execute_insert(s, storage, txn, conn_txn).map_err(|e| {
                translate_exclusion_violation_legacy(
                    e,
                    storage,
                    txn,
                    conn_txn,
                    &table_ref,
                    DEFAULT_DATABASE_NAME,
                )
            })
        }
        Stmt::Merge(_) => Err(DbError::NotImplemented {
            feature: "MERGE execution".into(),
        }),
        Stmt::Update(s) => {
            let table_ref = s.table.clone();
            execute_update(s, storage, txn, conn_txn).map_err(|e| {
                translate_exclusion_violation_legacy(
                    e,
                    storage,
                    txn,
                    conn_txn,
                    &table_ref,
                    DEFAULT_DATABASE_NAME,
                )
            })
        }
        Stmt::Delete(s) => execute_delete(s, storage, txn, conn_txn),
        Stmt::CreateTable(s) => {
            execute_create_table(s, storage, txn, conn_txn, DEFAULT_DATABASE_NAME)
        }
        Stmt::CreateDatabase(s) => execute_create_database(s, storage, txn, conn_txn),
        Stmt::CreateSchema(s) => {
            execute_create_schema(s, storage, txn, conn_txn, DEFAULT_DATABASE_NAME)
        }
        Stmt::DropTable(s) => {
            execute_drop_table(s, storage, txn, conn_txn, None, DEFAULT_DATABASE_NAME)
        }
        Stmt::DropDatabase(_) => Err(DbError::NotImplemented {
            feature: "DROP DATABASE requires session context".into(),
        }),
        Stmt::CreateIndex(s) => {
            let noop_bloom = crate::bloom::BloomRegistry::new();
            execute_create_index(
                s,
                storage,
                txn,
                conn_txn,
                &noop_bloom,
                DEFAULT_DATABASE_NAME,
            )
        }
        Stmt::DropIndex(s) => {
            let noop_bloom = crate::bloom::BloomRegistry::new();
            execute_drop_index(
                s,
                storage,
                txn,
                conn_txn,
                &noop_bloom,
                DEFAULT_DATABASE_NAME,
            )
        }
        Stmt::Begin => Err(DbError::TransactionAlreadyActive {
            txn_id: conn_txn.txn_id,
        }),
        Stmt::Commit => Err(DbError::NotImplemented {
            feature: "COMMIT in dispatch() requires session context — use execute_with_ctx".into(),
        }),
        Stmt::Rollback => Err(DbError::NotImplemented {
            feature: "ROLLBACK in dispatch() requires session context — use execute_with_ctx"
                .into(),
        }),
        Stmt::Set(_) => Ok(QueryResult::Empty),
        Stmt::UseDatabase(_) => Err(DbError::NotImplemented {
            feature: "USE requires session context".into(),
        }),
        Stmt::TruncateTable(s) => {
            execute_truncate(s, storage, txn, conn_txn, DEFAULT_DATABASE_NAME)
        }
        Stmt::AlterTable(s) => {
            execute_alter_table(s, storage, txn, conn_txn, DEFAULT_DATABASE_NAME)
        }
        Stmt::ShowDatabases(s) => execute_show_databases(s, storage, txn, conn_txn),
        Stmt::ShowTables(s) => {
            execute_show_tables(s, storage, txn, conn_txn, None, None, DEFAULT_DATABASE_NAME)
        }
        Stmt::ShowColumns(s) => {
            execute_show_columns(s, storage, txn, conn_txn, None, DEFAULT_DATABASE_NAME)
        }
        Stmt::ShowIndex(s) => {
            execute_show_index(s, storage, txn, conn_txn, None, DEFAULT_DATABASE_NAME)
        }
        Stmt::ShowCreateTable(s) => execute_show_create_table(
            s,
            storage,
            txn,
            conn_txn,
            None,
            DEFAULT_DATABASE_NAME,
        ),
        Stmt::RenameTable(s) => execute_rename_table(s, storage, txn, conn_txn, DEFAULT_DATABASE_NAME),
        Stmt::Analyze(_) => Err(DbError::NotImplemented {
            feature: "ANALYZE requires session context — use execute_with_ctx".into(),
        }),
        Stmt::Checkpoint => {
            txn.checkpoint(storage)?;
            Ok(QueryResult::Empty)
        }
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
        Stmt::DeclareCursor(_) | Stmt::FetchCursor(_) | Stmt::CloseCursor(_) => {
            Err(DbError::NotImplemented {
                feature: "SQL cursors require session context — use execute_with_ctx".into(),
            })
        }
        Stmt::Noop => Ok(QueryResult::Empty),
        // G5.1: CALL / DO — execute as Noop (no session context needed)
        Stmt::Call { .. } | Stmt::Do { .. } => Ok(QueryResult::Empty),
        // G5.5: CREATE TABLE LIKE
        Stmt::CreateTableLike(s) => {
            execute_create_table_like(s, storage, txn, conn_txn, None, DEFAULT_DATABASE_NAME)
        }
        // G5.6: CREATE TABLE AS SELECT — requires session context (cannot run via dispatch)
        Stmt::CreateTableAsSelect(_) => Err(DbError::NotImplemented {
            feature: "CREATE TABLE AS SELECT requires session context — use execute_with_ctx".into(),
        }),
        // 5.9f: SHOW TABLE STATUS
        Stmt::ShowTableStatus(s) => {
            execute_show_table_status(s, storage, txn, conn_txn, None, DEFAULT_DATABASE_NAME)
        }
        // 5.9g: SHOW ENGINES / CHARSET / COLLATION
        Stmt::ShowEngines => Ok(execute_show_engines()),
        Stmt::ShowCharset => Ok(execute_show_charset()),
        Stmt::ShowCollation => Ok(execute_show_collation()),
        // SHOW VARIABLES / SHOW STATUS — intercepted at wire level; minimal fallback here.
        Stmt::ShowVariables | Stmt::ShowStatus => Ok(QueryResult::Rows {
            columns: vec![
                ColumnMeta::computed("Variable_name", DataType::Text),
                ColumnMeta::computed("Value", DataType::Text),
            ],
            rows: vec![],
        }),
        // SHOW WARNINGS / SHOW ERRORS — no session context, return empty result set (5.9e)
        Stmt::ShowWarnings { .. } | Stmt::ShowErrors { .. } => Ok(show_warnings_result(&[])),
    }
}

// ── EXPLAIN ─────────────────────────────────────────────────────────────────

/// Executes EXPLAIN: runs the planner on the inner SELECT but does NOT
/// execute the query. Returns the query plan as a result set in MySQL format.
fn execute_explain(
    inner: Stmt,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    match inner {
        Stmt::Select(s) => explain_select(s, exec_ctx, ctx),
        Stmt::SetOp { first, rest } => {
            // EXPLAIN set-op chain: show every branch's plan.
            let columns = explain_columns();
            let mut rows = Vec::new();
            rows.push(vec![
                Value::Text("SET OP branch 1 (first)".into()),
                Value::Text(format!("{:?}", first.from)),
                Value::Null, Value::Null, Value::Null, Value::Null,
            ]);
            for (i, tail) in rest.into_iter().enumerate() {
                let op = match tail.kind {
                    SetOpKind::Union => if tail.all { "UNION ALL" } else { "UNION" },
                    SetOpKind::Intersect => if tail.all { "INTERSECT ALL" } else { "INTERSECT" },
                    SetOpKind::Except => if tail.all { "EXCEPT ALL" } else { "EXCEPT" },
                };
                rows.push(vec![
                    Value::Text(format!("{op} branch {}", i + 2)),
                    Value::Text(format!("{:?}", tail.select.from)),
                    Value::Null, Value::Null, Value::Null, Value::Null,
                ]);
            }
            Ok(QueryResult::Rows { columns, rows })
        }
        other => {
            // For non-SELECT, just show the statement type.
            let type_name = match &other {
                Stmt::Insert(_) => "INSERT",
                Stmt::Merge(_) => "MERGE",
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
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
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
        crate::ast::FromClause::JsonTable(_) => {
            return Err(DbError::Other(
                "EXPLAIN for JSON_TABLE FROM not yet supported".into(),
            ))
        }
        crate::ast::FromClause::JsonbSrf(_) => {
            return Err(DbError::Other(
                "EXPLAIN for JSONB SRF FROM not yet supported".into(),
            ))
        }
        crate::ast::FromClause::Values(_) => {
            return Err(DbError::Other(
                "EXPLAIN for VALUES FROM not yet supported".into(),
            ))
        }
        crate::ast::FromClause::RecursiveCte(_) => {
            return Err(DbError::Other(
                "EXPLAIN for recursive CTE FROM not yet supported".into(),
            ))
        }
    };

    let conn = ctx.conn_txn.take();
    let resolved = resolve_table_cached(storage, txn, ctx, conn.as_ref(), from_table_ref);
    ctx.conn_txn = conn;
    let resolved = resolved?;
    let snap = ctx
        .conn_txn
        .as_ref()
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());

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
        crate::planner::AccessMethod::GinScan { index_def, query_terms } => {
            // GIN scan: estimate ~10% of rows pass the containment filter.
            let est = (row_count / 10).max(1);
            (
                "gin",
                Value::Text(index_def.name.clone()),
                Value::Int(query_terms.len() as i32),
                Value::Null,
                Value::Int(est as i32),
                "Using GIN index; Using where",
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

/// Builds the 3-column result set for SHOW WARNINGS / SHOW ERRORS (5.9e).
///
/// MySQL protocol: connectors call `SHOW WARNINGS` after every DML statement.
/// They expect a result set with columns Level/Code/Message, NOT an OK packet.
/// An empty row list is valid and means "no warnings".
pub(crate) fn show_warnings_result(warnings: &[crate::session::SqlWarning]) -> QueryResult {
    let columns = vec![
        ColumnMeta::computed("Level", DataType::Text),
        ColumnMeta::computed("Code", DataType::Int),
        ColumnMeta::computed("Message", DataType::Text),
    ];
    let rows: Vec<Vec<Value>> = warnings
        .iter()
        .map(|w| {
            vec![
                Value::Text(w.level.into()),
                Value::Int(w.code as i32),
                Value::Text(w.message.clone()),
            ]
        })
        .collect();
    QueryResult::Rows { columns, rows }
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
