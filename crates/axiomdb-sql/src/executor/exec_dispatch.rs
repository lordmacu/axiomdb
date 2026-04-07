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
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
            execute_create_table(s, storage, txn, conn, &db)
        }
        Stmt::CreateDatabase(s) => {
            ctx.invalidate_all();
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
            execute_create_database(s, storage, txn, conn)
        }
        Stmt::CreateSchema(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
            execute_create_schema(s, storage, txn, conn, &db)
        }
        Stmt::DropTable(s) => {
            ctx.invalidate_all();
            let db = s
                .tables
                .first()
                .and_then(|t| t.database.as_deref())
                .unwrap_or(ctx.effective_database())
                .to_string();
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
            execute_drop_table(s, storage, txn, conn, &db)
        }
        Stmt::DropDatabase(s) => {
            ctx.invalidate_all();
            execute_drop_database(s, storage, txn, ctx)
        }
        Stmt::CreateIndex(s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
            execute_create_index(s, storage, txn, conn, bloom, &db)
        }
        Stmt::DropIndex(s) => {
            ctx.invalidate_all();
            let db = s
                .table
                .as_ref()
                .and_then(|t| t.database.as_deref())
                .unwrap_or(ctx.effective_database())
                .to_string();
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
            execute_drop_index(s, storage, txn, conn, bloom, &db)
        }
        Stmt::AlterTable(s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
            execute_alter_table(s, storage, txn, conn, &db)
        }
        Stmt::Analyze(s) => execute_analyze(s, storage, txn, ctx),
        Stmt::Explain(inner) => execute_explain(*inner, storage, txn, bloom, ctx),
        Stmt::Vacuum(s) => crate::vacuum::execute_vacuum(s, storage, txn, bloom, ctx),
        Stmt::Set(s) => execute_set_ctx(s, ctx),
        Stmt::UseDatabase(s) => execute_use_database(s, storage, txn, ctx),
        Stmt::Noop => Ok(QueryResult::Empty),
        Stmt::ShowDatabases(s) => {
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set");
            execute_show_databases(s, storage, txn, conn)
        }
        Stmt::ShowTables(mut s) => {
            // Default to current schema from search_path if not explicit.
            if s.schema.is_none() {
                s.schema = Some(ctx.current_schema().to_string());
            }
            let db = ctx.effective_database().to_string();
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set");
            execute_show_tables(s, storage, txn, conn, &db)
        }
        Stmt::ShowColumns(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set");
            execute_show_columns(s, storage, txn, conn, &db)
        }
        Stmt::ShowIndex(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set");
            execute_show_index(s, storage, txn, conn, &db)
        }
        Stmt::TruncateTable(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set");
            execute_truncate(s, storage, txn, conn, &db)
        }
        other => {
            let conn = ctx.conn_txn.as_mut().expect("conn_txn set for dispatch");
            dispatch(other, storage, txn, conn)
        }
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
