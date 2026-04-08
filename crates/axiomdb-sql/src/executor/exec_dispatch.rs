/// Routes a statement to its handler using a `SessionContext` for schema caching.
///
/// `ctx.conn_txn` remains the single source of truth for the active connection
/// transaction. Session-aware handlers read or temporarily take it from `ctx`
/// as needed, while legacy helpers that still require an explicit
/// `&mut ConnectionTxn` borrow it from `ctx.conn_txn.as_mut()`.
fn dispatch_ctx(
    stmt: Stmt,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // SAFETY: see ExecutionContext::storage_mut / coord_mut / bloom_mut doc comments.
    let storage = unsafe { exec_ctx.storage_mut() };
    let txn = unsafe { exec_ctx.coord_mut() };
    let bloom = unsafe { exec_ctx.bloom_mut() };

    // Flush staged inserts before any non-INSERT barrier statement.
    // INSERT statements handle same-table vs. different-table flush internally.
    if !matches!(stmt, Stmt::Insert(_)) {
        flush_pending_inserts_ctx(exec_ctx, ctx)?;
    }
    // ── Phase 40.4b: extract conn_txn via take() for DML calls ────────────
    // DML _ctx functions now receive conn_txn as an explicit parameter.
    // take() separates the mutable borrow so both conn_txn and ctx can be
    // passed without violating the borrow-checker. Restored after each call.
    let result = match stmt {
        Stmt::Select(s) => {
            let conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_select_ctx(s, exec_ctx, Some(&conn), ctx);
            ctx.conn_txn = Some(conn);
            r
        }
        Stmt::Insert(s) => {
            let mut conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_insert_ctx(s, exec_ctx, &mut conn, ctx);
            ctx.conn_txn = Some(conn);
            r
        }
        Stmt::Update(s) => {
            let mut conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_update_ctx(s, exec_ctx, &mut conn, ctx);
            ctx.conn_txn = Some(conn);
            r
        }
        Stmt::Delete(s) => {
            let mut conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_delete_ctx(s, exec_ctx, &mut conn, ctx);
            ctx.conn_txn = Some(conn);
            r
        }
        Stmt::CreateTable(mut s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            if s.table.schema.is_none() {
                s.table.schema = Some(ctx.current_schema().to_string());
            }
            execute_create_table(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::CreateDatabase(s) => {
            ctx.invalidate_all();
            execute_create_database(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
            )
        }
        Stmt::CreateSchema(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            execute_create_schema(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::DropTable(s) => {
            ctx.invalidate_all();
            let db = s
                .tables
                .first()
                .and_then(|t| t.database.as_deref())
                .unwrap_or(ctx.effective_database())
                .to_string();
            execute_drop_table(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::DropDatabase(s) => {
            ctx.invalidate_all();
            execute_drop_database(s, storage, txn, ctx)
        }
        Stmt::CreateIndex(s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            execute_create_index(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                bloom,
                &db,
            )
        }
        Stmt::DropIndex(s) => {
            ctx.invalidate_all();
            let db = s
                .table
                .as_ref()
                .and_then(|t| t.database.as_deref())
                .unwrap_or(ctx.effective_database())
                .to_string();
            execute_drop_index(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                bloom,
                &db,
            )
        }
        Stmt::AlterTable(s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            execute_alter_table(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::Analyze(s) => execute_analyze(s, exec_ctx, ctx),
        Stmt::Explain(inner) => execute_explain(*inner, exec_ctx, ctx),
        Stmt::Vacuum(s) => crate::vacuum::execute_vacuum(s, exec_ctx, ctx),
        Stmt::Set(s) => execute_set_ctx(s, ctx),
        Stmt::UseDatabase(s) => execute_use_database(s, storage, txn, ctx),
        Stmt::Noop => Ok(QueryResult::Empty),
        Stmt::ShowDatabases(s) => execute_show_databases(
            s,
            storage,
            txn,
            ctx.conn_txn
                .as_mut()
                .expect("conn_txn must be set before dispatch_ctx"),
        ),
        Stmt::ShowTables(mut s) => {
            if s.schema.is_none() {
                s.schema = Some(ctx.current_schema().to_string());
            }
            let db = ctx.effective_database().to_string();
            execute_show_tables(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::ShowColumns(s) => {
            let db = ddl_database(&s.table.database, ctx);
            execute_show_columns(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::ShowIndex(s) => {
            let db = ddl_database(&s.table.database, ctx);
            execute_show_index(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::ShowCreateTable(s) => {
            let db = ddl_database(&s.table.database, ctx);
            execute_show_create_table(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::RenameTable(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            execute_rename_table(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::TruncateTable(s) => {
            let db = ddl_database(&s.table.database, ctx);
            execute_truncate(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        // G5.1: CALL / DO — execute as Noop
        Stmt::Call { .. } | Stmt::Do { .. } => Ok(QueryResult::Empty),
        // G5.5: CREATE TABLE ... LIKE
        Stmt::CreateTableLike(s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.new_table.database, ctx);
            execute_create_table_like(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        // G5.6: CREATE TABLE ... AS SELECT
        Stmt::CreateTableAsSelect(s) => {
            ctx.invalidate_all();
            execute_create_table_as_select(s, exec_ctx, ctx)
        }
        // 5.9f: SHOW TABLE STATUS — needs database context
        Stmt::ShowTableStatus(mut s) => {
            if s.schema.is_none() {
                s.schema = Some(ctx.current_schema().to_string());
            }
            let db = ctx.effective_database().to_string();
            execute_show_table_status(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        // 5.9g: SHOW ENGINES / CHARSET / COLLATION — static, no ctx needed
        Stmt::ShowEngines => Ok(execute_show_engines()),
        Stmt::ShowCharset => Ok(execute_show_charset()),
        Stmt::ShowCollation => Ok(execute_show_collation()),
        // SHOW WARNINGS / SHOW ERRORS — return per-session warning list (5.9e)
        Stmt::ShowWarnings { limit } => {
            let warnings: Vec<_> = ctx.warnings.iter().cloned().collect();
            let mut result = show_warnings_result(&warnings);
            if let Some(n) = limit {
                if let QueryResult::Rows { ref mut rows, .. } = result {
                    rows.truncate(n as usize);
                }
            }
            Ok(result)
        }
        Stmt::ShowErrors { limit } => {
            let errors: Vec<_> = ctx
                .warnings
                .iter()
                .filter(|w| w.level == "Error")
                .cloned()
                .collect();
            let mut result = show_warnings_result(&errors);
            if let Some(n) = limit {
                if let QueryResult::Rows { ref mut rows, .. } = result {
                    rows.truncate(n as usize);
                }
            }
            Ok(result)
        }
        // SHOW VARIABLES / SHOW STATUS — intercepted at wire level; empty fallback.
        Stmt::ShowVariables | Stmt::ShowStatus => Ok(QueryResult::Rows {
            columns: vec![
                ColumnMeta::computed("Variable_name", DataType::Text),
                ColumnMeta::computed("Value", DataType::Text),
            ],
            rows: vec![],
        }),
        other => dispatch(
            other,
            storage,
            txn,
            ctx.conn_txn
                .as_mut()
                .expect("conn_txn must be set before dispatch_ctx"),
        ),
    };
    result
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
            SetValue::Default => {
                ctx.strict_mode = true;
                ctx.ansi_quotes = false;
            }
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
                ctx.ansi_quotes = crate::session::sql_mode_has_ansi_quotes(&normalized);
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
        // MySQL dump compatibility variables — accepted and silently ignored.
        // mysqldump wraps every import with SET FOREIGN_KEY_CHECKS=0; ... SET FOREIGN_KEY_CHECKS=1
        // and SET UNIQUE_CHECKS=0 / 1. AxiomDB always enforces these constraints
        // (consistent with InnoDB behaviour when data is valid), so setting them
        // to 0 is a no-op here. The variables are accepted to avoid parse errors.
        "foreign_key_checks"
        | "unique_checks"
        | "sql_notes"
        | "time_zone"
        | "character_set_client"
        | "character_set_results"
        | "character_set_connection"
        | "collation_connection"
        | "completion_type"
        | "group_concat_max_len"
        | "net_write_timeout"
        | "net_read_timeout"
        | "wait_timeout"
        | "interactive_timeout" => {
            // Accept the value; do nothing.
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
