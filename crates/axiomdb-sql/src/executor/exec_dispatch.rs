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
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let bloom = exec_ctx.bloom();

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
        Stmt::Select(mut s) => {
            let into_outfile = s.into_outfile.take();
            let conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_select_ctx(s, exec_ctx, Some(&conn), ctx);
            ctx.conn_txn = Some(conn);
            handle_into_outfile(r, into_outfile)
        }
        Stmt::SetOp { first, rest } => {
            let conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_set_op(first, rest, exec_ctx, Some(&conn), ctx);
            ctx.conn_txn = Some(conn);
            r
        }
        Stmt::Insert(s) => {
            let table_ref = s.table.clone();
            let mut conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_insert_ctx(s, exec_ctx, &mut conn, ctx);
            ctx.conn_txn = Some(conn);
            let result =
                r.map_err(|e| translate_exclusion_violation_ctx(e, exec_ctx, ctx, &table_ref))?;
            run_statement_triggers_for_result(
                TriggerEvent::Insert,
                &table_ref,
                result,
                exec_ctx,
                ctx,
            )
        }
        Stmt::Merge(s) => {
            let table_ref = s.target.clone();
            let mut conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_merge_ctx(s, exec_ctx, &mut conn, ctx);
            ctx.conn_txn = Some(conn);
            r.map_err(|e| translate_exclusion_violation_ctx(e, exec_ctx, ctx, &table_ref))
        }
        Stmt::Update(s) => {
            let table_ref = s.table.clone();
            let mut conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_update_ctx(s, exec_ctx, &mut conn, ctx);
            ctx.conn_txn = Some(conn);
            let result =
                r.map_err(|e| translate_exclusion_violation_ctx(e, exec_ctx, ctx, &table_ref))?;
            run_statement_triggers_for_result(
                TriggerEvent::Update,
                &table_ref,
                result,
                exec_ctx,
                ctx,
            )
        }
        Stmt::Delete(s) => {
            let table_ref = s.table.clone();
            let mut conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_delete_ctx(s, exec_ctx, &mut conn, ctx);
            ctx.conn_txn = Some(conn);
            let result = r?;
            run_statement_triggers_for_result(
                TriggerEvent::Delete,
                &table_ref,
                result,
                exec_ctx,
                ctx,
            )
        }
        // Phase 20.5: COPY FROM / TO
        Stmt::CopyFrom(s) => {
            let mut conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_copy_from(s, exec_ctx, &mut conn, ctx);
            ctx.conn_txn = Some(conn);
            r
        }
        Stmt::CopyTo(s) => {
            let conn_opt = ctx.conn_txn.take();
            let r = execute_copy_to(s, exec_ctx, conn_opt.as_ref(), ctx);
            ctx.conn_txn = conn_opt;
            r
        }
        Stmt::CreateTable(mut s) => {
            ctx.invalidate_all();
            if s.persistence == axiomdb_catalog::TablePersistence::Temporary {
                if s.table.database.is_some() || s.table.schema.is_some() {
                    return Err(DbError::InvalidValue {
                        reason: "temporary tables cannot specify an explicit database or schema"
                            .into(),
                    });
                }
                s.table.schema = Some(ctx.ensure_temp_schema());
            } else if s.table.schema.is_none() {
                s.table.schema = Some(ctx.default_create_schema().to_string());
            }
            let db = ddl_database(&s.table.database, ctx);
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
        Stmt::DropSchema(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            execute_drop_schema(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::CreateServer(s) => {
            ctx.invalidate_all();
            execute_create_server(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
            )
        }
        Stmt::DropServer(s) => {
            ctx.invalidate_all();
            execute_drop_server(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
            )
        }
        Stmt::CreateForeignTable(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            execute_create_foreign_table(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::DropForeignTable(s) => {
            ctx.invalidate_all();
            execute_drop_foreign_table(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
            )
        }
        Stmt::DropTable(s) => {
            ctx.invalidate_all();
            let search_path = ctx.search_path.clone();
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
                Some(search_path.as_slice()),
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
        Stmt::Checkpoint => {
            txn.checkpoint(storage)?;
            Ok(QueryResult::Empty)
        }
        Stmt::Explain(inner) => execute_explain(*inner, exec_ctx, ctx),
        Stmt::Vacuum(s) => crate::vacuum::execute_vacuum(s, exec_ctx, ctx),
        Stmt::Listen(s) => execute_listen(s, ctx),
        Stmt::Unlisten(s) => execute_unlisten(s, ctx),
        Stmt::Notify(s) => execute_notify(s, ctx),
        Stmt::DeclareCursor(s) => execute_declare_cursor(s, exec_ctx, ctx),
        Stmt::FetchCursor(s) => execute_fetch_cursor(s, ctx),
        Stmt::CloseCursor(s) => execute_close_cursor(s, ctx),
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
        Stmt::ShowSchemas(s) => execute_show_schemas(s, storage, txn, ctx),
        Stmt::ShowTables(s) => {
            let db = ctx.effective_database().to_string();
            let search_path = ctx.search_path.clone();
            let default_schema = ctx.default_create_schema().to_string();
            execute_show_tables(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
                Some(default_schema.as_str()),
                &db,
            )
        }
        Stmt::ShowColumns(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let search_path = ctx.search_path.clone();
            execute_show_columns(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
                &db,
            )
        }
        Stmt::ShowIndex(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let search_path = ctx.search_path.clone();
            execute_show_index(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
                &db,
            )
        }
        Stmt::ShowNotifications => execute_show_notifications(ctx),
        Stmt::ShowCreateTable(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let search_path = ctx.search_path.clone();
            execute_show_create_table(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
                &db,
            )
        }
        Stmt::ShowCreateTrigger(s) => {
            let db = ddl_database(&s.table.database, ctx);
            execute_show_create_trigger(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::ShowCreateView(s) => {
            let db = ddl_database(&s.view.database, ctx);
            let search_path = ctx.search_path.clone();
            execute_show_create_view(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
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
        Stmt::CreateTableLike(mut s) => {
            ctx.invalidate_all();
            if s.persistence == axiomdb_catalog::TablePersistence::Temporary {
                if s.new_table.database.is_some() || s.new_table.schema.is_some() {
                    return Err(DbError::InvalidValue {
                        reason: "temporary tables cannot specify an explicit database or schema"
                            .into(),
                    });
                }
                s.new_table.schema = Some(ctx.ensure_temp_schema());
            } else if s.new_table.schema.is_none() {
                s.new_table.schema = Some(ctx.default_create_schema().to_string());
            }
            let search_path = ctx.search_path.clone();
            let db = ddl_database(&s.new_table.database, ctx);
            execute_create_table_like(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
                &db,
            )
        }
        // G5.6: CREATE TABLE ... AS SELECT
        Stmt::CreateTableAsSelect(mut s) => {
            ctx.invalidate_all();
            if s.persistence == axiomdb_catalog::TablePersistence::Temporary {
                if s.new_table.database.is_some() || s.new_table.schema.is_some() {
                    return Err(DbError::InvalidValue {
                        reason: "temporary tables cannot specify an explicit database or schema"
                            .into(),
                    });
                }
                s.new_table.schema = Some(ctx.ensure_temp_schema());
            } else if s.new_table.schema.is_none() {
                s.new_table.schema = Some(ctx.default_create_schema().to_string());
            }
            execute_create_table_as_select(s, exec_ctx, ctx)
        }
        Stmt::CreateMaterializedView(mut s) => {
            ctx.invalidate_all();
            if s.view.schema.is_none() {
                s.view.schema = Some(ctx.default_create_schema().to_string());
            }
            execute_create_materialized_view(s, exec_ctx, ctx)
        }
        Stmt::CreateView(mut s) => {
            ctx.invalidate_all();
            if s.view.schema.is_none() {
                s.view.schema = Some(ctx.default_create_schema().to_string());
            }
            let db = ddl_database(&s.view.database, ctx);
            let search_path = ctx.search_path.clone();
            execute_create_view(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
                &db,
            )
        }
        Stmt::CreateTrigger(s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            execute_create_trigger(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::CreateAggregate(s) => {
            ctx.invalidate_all();
            let schema = ctx.default_create_schema().to_string();
            execute_create_aggregate(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &schema,
            )
        }
        Stmt::CreateSequence(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            let default_schema = ctx.default_create_schema().to_string();
            execute_create_sequence(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
                &default_schema,
            )
        }
        Stmt::CreateEnumType(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            let default_schema = ctx.default_create_schema().to_string();
            execute_create_enum_type(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
                &default_schema,
            )
        }
        Stmt::DropEnumType(s) => {
            ctx.invalidate_all();
            let default_schema = ctx.default_create_schema().to_string();
            execute_drop_enum_type(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &default_schema,
            )
        }
        Stmt::DropMaterializedView(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            let search_path = ctx.search_path.clone();
            execute_drop_materialized_view(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
                &db,
            )
        }
        Stmt::DropView(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            let search_path = ctx.search_path.clone();
            execute_drop_view(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
                &db,
            )
        }
        Stmt::RefreshMaterializedView(s) => {
            ctx.invalidate_all();
            execute_refresh_materialized_view(s, exec_ctx, ctx)
        }
        Stmt::DropTrigger(s) => {
            ctx.invalidate_all();
            let db = ddl_database(&s.table.database, ctx);
            execute_drop_trigger(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
            )
        }
        Stmt::DropAggregate(s) => {
            ctx.invalidate_all();
            let schema = ctx.default_create_schema().to_string();
            execute_drop_aggregate(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &schema,
            )
        }
        Stmt::DropSequence(s) => {
            ctx.invalidate_all();
            let db = ctx.effective_database().to_string();
            let default_schema = ctx.default_create_schema().to_string();
            execute_drop_sequence(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                &db,
                &default_schema,
            )
        }
        // 5.9f: SHOW TABLE STATUS — needs database context
        Stmt::ShowTableStatus(s) => {
            let db = ctx.effective_database().to_string();
            let search_path = ctx.search_path.clone();
            execute_show_table_status(
                s,
                storage,
                txn,
                ctx.conn_txn
                    .as_mut()
                    .expect("conn_txn must be set before dispatch_ctx"),
                Some(search_path.as_slice()),
                &db,
            )
        }
        // 5.9g: SHOW ENGINES / CHARSET / COLLATION — static, no ctx needed
        Stmt::ShowEngines => Ok(execute_show_engines()),
        Stmt::ShowCharset => Ok(execute_show_charset()),
        Stmt::ShowCollation => Ok(execute_show_collation()),
        // SHOW WARNINGS / SHOW ERRORS — return per-session warning list (5.9e)
        Stmt::ShowWarnings { limit } => {
            let warnings = ctx.warnings.to_vec();
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
                    ctx.set_search_path(vec!["public".to_string()]);
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
            ctx.set_search_path(schemas);
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
