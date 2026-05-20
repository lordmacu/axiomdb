fn execute_create_trigger(
    stmt: CreateTriggerStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
    let resolved = resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?;
    let mut table_def = resolved.def;
    if table_def.is_materialized_view() {
        return Err(DbError::InvalidValue {
            reason: "triggers are only supported on base tables".into(),
        });
    }
    if table_def
        .triggers
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case(&stmt.name))
    {
        return Err(DbError::TriggerAlreadyExists {
            name: stmt.name,
            table: table_def.table_name,
        });
    }
    let ordinal = table_def
        .triggers
        .iter()
        .map(|t| t.ordinal)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    table_def.triggers.push(axiomdb_catalog::TriggerDef {
        name: stmt.name,
        event: match stmt.event {
            TriggerEvent::Insert => axiomdb_catalog::TriggerEvent::Insert,
            TriggerEvent::Update => axiomdb_catalog::TriggerEvent::Update,
            TriggerEvent::Delete => axiomdb_catalog::TriggerEvent::Delete,
        },
        body_sql: stmt.body_sql,
        ordinal,
    });
    table_def.triggers.sort_by_key(|t| t.ordinal);

    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.update_table_triggers(table_def.id, table_def.triggers)?;
    writer.bump_table_schema_version(table_def.id)?;
    Ok(QueryResult::Empty)
}

fn execute_drop_trigger(
    stmt: DropTriggerStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
    let resolved = resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?;
    let mut table_def = resolved.def;
    let Some(pos) = table_def
        .triggers
        .iter()
        .position(|t| t.name.eq_ignore_ascii_case(&stmt.name))
    else {
        return Err(DbError::TriggerNotFound {
            name: stmt.name,
            table: table_def.table_name,
        });
    };
    table_def.triggers.remove(pos);
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.update_table_triggers(table_def.id, table_def.triggers)?;
    writer.bump_table_schema_version(table_def.id)?;
    Ok(QueryResult::Empty)
}

fn execute_show_create_trigger(
    stmt: ShowCreateTriggerStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
    let resolved = resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?;
    let table_def = resolved.def;
    let trigger = table_def
        .triggers
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(&stmt.name))
        .ok_or_else(|| DbError::TriggerNotFound {
            name: stmt.name.clone(),
            table: table_def.table_name.clone(),
        })?;
    let event = match trigger.event {
        axiomdb_catalog::TriggerEvent::Insert => "INSERT",
        axiomdb_catalog::TriggerEvent::Update => "UPDATE",
        axiomdb_catalog::TriggerEvent::Delete => "DELETE",
    };
    Ok(QueryResult::Rows {
        columns: vec![
            ColumnMeta::computed("Trigger", DataType::Text),
            ColumnMeta::computed("SQL Original Statement", DataType::Text),
        ],
        rows: vec![vec![
            Value::Text(trigger.name.clone()),
            Value::Text(format!(
                "CREATE TRIGGER {} AFTER {} ON {} FOR EACH STATEMENT AS {}",
                trigger.name, event, table_def.table_name, trigger.body_sql
            )),
        ]],
    })
}

fn run_statement_triggers_for_result(
    event: TriggerEvent,
    table: &TableRef,
    result: QueryResult,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let QueryResult::Affected { count, .. } = &result else {
        return Ok(result);
    };

    // Fast path: read triggers from the session cache — no catalog HeapChain
    // scan needed. Trigger definitions only change via DDL, which always
    // invalidates the session cache (SchemaCache::invalidate). The table was
    // just resolved in execute_insert_ctx so the cache entry is current.
    let database: String = table
        .database
        .as_deref()
        .unwrap_or_else(|| ctx.effective_database())
        .to_string();
    let pick = |rt: &ResolvedTable| (rt.def.table_name.clone(), rt.def.triggers.clone());
    let cached_info: Option<(String, Vec<axiomdb_catalog::TriggerDef>)> =
        if let Some(schema) = table.schema.as_deref() {
            ctx.get_table(&database, schema, &table.name).map(pick)
        } else {
            let n = ctx.search_path.len();
            let mut found = None;
            for i in 0..n {
                let schema = ctx.search_path[i].clone();
                if let Some(rt) = ctx.get_table(&database, &schema, &table.name) {
                    found = Some(pick(rt));
                    break;
                }
            }
            found
        };

    let (table_name, all_triggers) = if let Some(info) = cached_info {
        info
    } else {
        // Cache miss — fall back to full catalog probe.
        let conn = ctx.conn_txn.take().expect("conn_txn set during DML");
        let resolved = resolve_table_cached(
            exec_ctx.storage(),
            exec_ctx.coord(),
            ctx,
            Some(&conn),
            table,
        )?;
        ctx.conn_txn = Some(conn);
        (resolved.def.table_name, resolved.def.triggers)
    };

    let matching: Vec<_> = all_triggers
        .into_iter()
        .filter(|t| {
            matches!(
                (event, t.event),
                (TriggerEvent::Insert, axiomdb_catalog::TriggerEvent::Insert)
                    | (TriggerEvent::Update, axiomdb_catalog::TriggerEvent::Update)
                    | (TriggerEvent::Delete, axiomdb_catalog::TriggerEvent::Delete)
            )
        })
        .collect();
    for trigger in matching {
        run_one_statement_trigger(&trigger, &table_name, *count, exec_ctx, ctx)?;
    }
    Ok(result)
}

fn run_one_statement_trigger(
    trigger: &axiomdb_catalog::TriggerDef,
    table_name: &str,
    row_count: u64,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<(), DbError> {
    let event_name = match trigger.event {
        axiomdb_catalog::TriggerEvent::Insert => "INSERT",
        axiomdb_catalog::TriggerEvent::Update => "UPDATE",
        axiomdb_catalog::TriggerEvent::Delete => "DELETE",
    };
    let sql = trigger
        .body_sql
        .replace("@@trigger_name", &format!("'{}'", trigger.name.replace('\'', "''")))
        .replace("@@trigger_table", &format!("'{}'", table_name.replace('\'', "''")))
        .replace("@@trigger_event", &format!("'{}'", event_name))
        .replace("@@trigger_row_count", &row_count.to_string());
    let parsed = crate::parser::parse_with_sql_mode(
        &sql,
        None,
        crate::session::SqlModeFlags {
            ansi_quotes: ctx.ansi_quotes,
        },
    )?;
    let analyzed = crate::analyzer::analyze_with_defaults(
        parsed,
        exec_ctx.storage(),
        exec_ctx.coord().active_snapshot(
            ctx.conn_txn
                .as_ref()
                .expect("conn_txn set during trigger execution"),
        ),
        ctx.effective_database(),
        ctx.default_create_schema(),
    )?;
    let Stmt::Select(select) = analyzed else {
        return Err(DbError::InvalidValue {
            reason: format!("trigger '{}' body must be a SELECT", trigger.name),
        });
    };
    let conn = ctx.conn_txn.take().expect("conn_txn set during trigger execution");
    let result = execute_select_ctx(select, exec_ctx, Some(&conn), ctx);
    ctx.conn_txn = Some(conn);
    let result = result?;
    if let QueryResult::Rows { rows, .. } = result {
        if let Some(first) = rows.first() {
            return Err(DbError::TriggerValidationFailed {
                trigger: trigger.name.clone(),
                table: table_name.to_string(),
                message: trigger_failure_message(first),
            });
        }
    }
    Ok(())
}

fn trigger_failure_message(row: &[Value]) -> String {
    if row.is_empty() {
        return "validation query returned rows".into();
    }
    row.iter()
        .take(3)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
