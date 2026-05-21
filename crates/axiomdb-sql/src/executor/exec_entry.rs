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
/// Snapshot for a statement given its (optional) connection transaction:
/// `active_snapshot` includes the connection's own uncommitted writes;
/// `snapshot` is the latest committed view when there is no open transaction.
fn snapshot_for(
    txn: &TxnManager,
    conn: Option<&ConnectionTxn>,
) -> axiomdb_core::TransactionSnapshot {
    match conn {
        Some(c) => txn.active_snapshot(c),
        None => txn.snapshot(),
    }
}

pub fn execute_read_only_with_ctx(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let exec_ctx = ExecutionContext::new(storage, txn, bloom, None);
    match stmt {
        Stmt::Select(mut s) => {
            let into_outfile = s.into_outfile.take();
            let conn = ctx.conn_txn.take();
            let prev_snap = ctx.eval_snapshot.replace(snapshot_for(txn, conn.as_ref()));
            let r = execute_select_ctx(s, &exec_ctx, conn.as_ref(), ctx);
            ctx.eval_snapshot = prev_snap;
            ctx.conn_txn = conn;
            handle_into_outfile(r, into_outfile)
        }
        Stmt::SetOp { first, rest } => {
            let conn = ctx.conn_txn.take();
            let prev_snap = ctx.eval_snapshot.replace(snapshot_for(txn, conn.as_ref()));
            let r = execute_set_op(first, rest, &exec_ctx, conn.as_ref(), ctx);
            ctx.eval_snapshot = prev_snap;
            ctx.conn_txn = conn;
            r
        }
        Stmt::ShowDatabases(_) => {
            let snap = ctx
                .conn_txn
                .as_ref()
                .map(|conn| txn.active_snapshot(conn))
                .unwrap_or_else(|| txn.snapshot());
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
        Stmt::ShowTables(s) => {
            let db = ctx.effective_database();
            let snap = ctx
                .conn_txn
                .as_ref()
                .map(|conn| txn.active_snapshot(conn))
                .unwrap_or_else(|| txn.snapshot());
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
            let mut seen = std::collections::HashSet::new();
            let mut tables = Vec::new();
            if let Some(schema) = s.schema.as_deref() {
                tables = reader.list_tables_in_database(db, schema)?;
            } else {
                for schema in &ctx.search_path {
                    for table in reader.list_tables_in_database(db, schema)? {
                        if seen.insert(table.table_name.clone()) {
                            tables.push(table);
                        }
                    }
                }
            }
            let schema = s
                .schema
                .as_deref()
                .unwrap_or_else(|| ctx.default_create_schema());
            let col_name = format!("Tables_in_{schema}");
            if s.full {
                let out_cols = vec![
                    ColumnMeta::computed(col_name, DataType::Text),
                    ColumnMeta::computed("Table_type", DataType::Text),
                ];
                let rows: Vec<Row> = tables
                    .into_iter()
                    .map(|t| {
                        let table_type = show_table_type_name(&t).to_string();
                        vec![
                            Value::Text(t.table_name),
                            Value::Text(table_type),
                        ]
                    })
                    .collect();
                Ok(QueryResult::Rows {
                    columns: out_cols,
                    rows,
                })
            } else {
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
        }
        Stmt::ShowColumns(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let snap = ctx
                .conn_txn
                .as_ref()
                .map(|conn| txn.active_snapshot(conn))
                .unwrap_or_else(|| txn.snapshot());
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
            let table_def = if let Some(schema) = s.table.schema.as_deref() {
                reader
                    .get_table_in_database(&db, schema, &s.table.name)?
                    .ok_or_else(|| DbError::TableNotFound {
                        name: s.table.name.clone(),
                    })?
            } else {
                let mut found = None;
                for schema in &ctx.search_path {
                    if let Some(def) = reader.get_table_in_database(&db, schema, &s.table.name)? {
                        found = Some(def);
                        break;
                    }
                }
                found.ok_or_else(|| DbError::TableNotFound {
                    name: s.table.name.clone(),
                })?
            };
            let columns = reader.list_columns(table_def.id)?;
            let database_collation = reader.get_database(&db)?.and_then(|db| db.default_collation);
            let base_cols = vec![
                ColumnMeta::computed("Field", DataType::Text),
                ColumnMeta::computed("Type", DataType::Text),
                ColumnMeta::computed("Null", DataType::Text),
                ColumnMeta::computed("Key", DataType::Text),
                ColumnMeta::computed("Default", DataType::Text),
                ColumnMeta::computed("Extra", DataType::Text),
            ];
            let out_cols = if s.full {
                let mut cols = base_cols;
                cols.push(ColumnMeta::computed("Collation", DataType::Text));
                cols.push(ColumnMeta::computed("Privileges", DataType::Text));
                cols.push(ColumnMeta::computed("Comment", DataType::Text));
                cols
            } else {
                base_cols
            };
            let rows: Vec<Row> = columns
                .iter()
                .map(|c| {
                    let type_str = column_sql_type_display(c);
                    let null_str = if c.nullable { "YES" } else { "NO" };
                    // Phase 24.1c: identity columns take precedence.
                    let extra = match c.identity_kind {
                        axiomdb_catalog::IdentityKind::Always => "GENERATED ALWAYS AS IDENTITY",
                        axiomdb_catalog::IdentityKind::ByDefault => "GENERATED BY DEFAULT AS IDENTITY",
                        axiomdb_catalog::IdentityKind::None => {
                            if c.auto_increment {
                                "auto_increment"
                            } else {
                                ""
                            }
                        }
                    };
                    let mut row = vec![
                        Value::Text(c.name.clone()),
                        Value::Text(type_str),
                        Value::Text(null_str.into()),
                        Value::Text("".into()),
                        Value::Null,
                        Value::Text(extra.to_owned()),
                    ];
                    if s.full {
                        let coll = effective_column_collation(
                            c,
                            &table_def,
                            database_collation.as_deref(),
                        )
                            .map(|name| Value::Text(name.to_string()))
                            .unwrap_or(Value::Null);
                        row.push(coll);
                        row.push(Value::Text("select,insert,update,references".into()));
                        row.push(Value::Text("".into()));
                    }
                    row
                })
                .collect();
            Ok(QueryResult::Rows {
                columns: out_cols,
                rows,
            })
        }
        Stmt::ShowIndex(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let snap = ctx
                .conn_txn
                .as_ref()
                .map(|conn| txn.active_snapshot(conn))
                .unwrap_or_else(|| txn.snapshot());
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
            let table_def = if let Some(schema) = s.table.schema.as_deref() {
                reader
                    .get_table_in_database(&db, schema, &s.table.name)?
                    .ok_or_else(|| DbError::TableNotFound {
                        name: s.table.name.clone(),
                    })?
            } else {
                let mut found = None;
                for schema in &ctx.search_path {
                    if let Some(def) = reader.get_table_in_database(&db, schema, &s.table.name)? {
                        found = Some(def);
                        break;
                    }
                }
                found.ok_or_else(|| DbError::TableNotFound {
                    name: s.table.name.clone(),
                })?
            };
            let col_defs = reader.list_columns(table_def.id)?;
            let indexes = reader.list_indexes(table_def.id)?;
            let out_cols = vec![
                ColumnMeta::computed("Table", DataType::Text),
                ColumnMeta::computed("Non_unique", DataType::Int),
                ColumnMeta::computed("Key_name", DataType::Text),
                ColumnMeta::computed("Seq_in_index", DataType::Int),
                ColumnMeta::computed("Column_name", DataType::Text),
                ColumnMeta::computed("Collation", DataType::Text),
                ColumnMeta::computed("Cardinality", DataType::Int),
                ColumnMeta::computed("Sub_part", DataType::Text),
                ColumnMeta::computed("Packed", DataType::Text),
                ColumnMeta::computed("Null", DataType::Text),
                ColumnMeta::computed("Index_type", DataType::Text),
                ColumnMeta::computed("Comment", DataType::Text),
                ColumnMeta::computed("Index_comment", DataType::Text),
                ColumnMeta::computed("Visible", DataType::Text),
            ];
            let mut rows: Vec<Row> = Vec::new();
            for idx in &indexes {
                let key_name = if idx.is_primary {
                    "PRIMARY".to_string()
                } else {
                    idx.name.clone()
                };
                let non_unique = if idx.is_unique || idx.is_primary {
                    Value::Int(0)
                } else {
                    Value::Int(1)
                };
                for (seq, ic) in idx.columns.iter().enumerate() {
                    let col_name = col_defs
                        .iter()
                        .find(|c| c.col_idx == ic.col_idx)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| format!("col_{}", ic.col_idx));
                    let nullable_flag = col_defs
                        .iter()
                        .find(|c| c.col_idx == ic.col_idx)
                        .map(|c| if c.nullable { "YES" } else { "" })
                        .unwrap_or("YES");
                    rows.push(vec![
                        Value::Text(s.table.name.clone()),
                        non_unique.clone(),
                        Value::Text(key_name.clone()),
                        Value::Int((seq + 1) as i32),
                        Value::Text(col_name),
                        Value::Text("A".into()),
                        Value::Int(0),
                        Value::Null,
                        Value::Null,
                        Value::Text(nullable_flag.into()),
                        Value::Text("BTREE".into()),
                        Value::Text("".into()),
                        Value::Text("".into()),
                        Value::Text("YES".into()),
                    ]);
                }
            }
            Ok(QueryResult::Rows {
                columns: out_cols,
                rows,
            })
        }
        Stmt::ShowCreateTable(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let snap = ctx
                .conn_txn
                .as_ref()
                .map(|conn| txn.active_snapshot(conn))
                .unwrap_or_else(|| txn.snapshot());
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
            let table_def = if let Some(schema) = s.table.schema.as_deref() {
                reader
                    .get_table_in_database(&db, schema, &s.table.name)?
                    .ok_or_else(|| DbError::TableNotFound {
                        name: s.table.name.clone(),
                    })?
            } else {
                let mut found = None;
                for schema in &ctx.search_path {
                    if let Some(def) = reader.get_table_in_database(&db, schema, &s.table.name)? {
                        found = Some(def);
                        break;
                    }
                }
                found.ok_or_else(|| DbError::TableNotFound {
                    name: s.table.name.clone(),
                })?
            };
            let columns = reader.list_columns(table_def.id)?;
            let indexes = reader.list_indexes(table_def.id)?;
            let database_collation = reader.get_database(&db)?.and_then(|db| db.default_collation);
            if table_def.is_materialized_view() {
                let defining_query =
                    table_def
                        .defining_query
                        .clone()
                        .ok_or_else(|| DbError::Internal {
                            message: format!(
                                "materialized view '{}' is missing its defining query",
                                table_def.table_name
                            ),
                        })?;
                let ddl = format!(
                    "CREATE MATERIALIZED VIEW `{}` AS {}",
                    table_def.table_name, defining_query
                );
                return Ok(QueryResult::Rows {
                    columns: vec![
                        ColumnMeta::computed("View", DataType::Text),
                        ColumnMeta::computed("Create View", DataType::Text),
                    ],
                    rows: vec![vec![
                        Value::Text(table_def.table_name.clone()),
                        Value::Text(ddl),
                    ]],
                });
            }
            let create_prefix = match table_def.persistence {
                axiomdb_catalog::TablePersistence::Permanent => "CREATE TABLE",
                axiomdb_catalog::TablePersistence::Temporary => "CREATE TEMPORARY TABLE",
                axiomdb_catalog::TablePersistence::Unlogged => "CREATE UNLOGGED TABLE",
            };
            let mut ddl = format!("{create_prefix} `{}` (\n", table_def.table_name);
            for col in &columns {
                let type_str = column_sql_type_display(col);
                let null_str = if col.nullable { "" } else { " NOT NULL" };
                // Phase 24.1c: identity syntax takes precedence.
                let extra = match col.identity_kind {
                    axiomdb_catalog::IdentityKind::Always => " GENERATED ALWAYS AS IDENTITY",
                    axiomdb_catalog::IdentityKind::ByDefault => " GENERATED BY DEFAULT AS IDENTITY",
                    axiomdb_catalog::IdentityKind::None => {
                        if col.auto_increment {
                            " AUTO_INCREMENT"
                        } else {
                            ""
                        }
                    }
                };
                let collate = effective_column_collation(
                    col,
                    &table_def,
                    database_collation.as_deref(),
                )
                .map(|name| format!(" COLLATE {name}"))
                .unwrap_or_default();
                ddl.push_str(&format!(
                    "  `{}` {}{}{}{},\n",
                    col.name, type_str, collate, null_str, extra
                ));
            }
            if let Some(pk) = indexes.iter().find(|i| i.is_primary) {
                let pk_cols: Vec<String> = pk
                    .columns
                    .iter()
                    .filter_map(|ic| columns.iter().find(|c| c.col_idx == ic.col_idx))
                    .map(|c| format!("`{}`", c.name))
                    .collect();
                if !pk_cols.is_empty() {
                    ddl.push_str(&format!("  PRIMARY KEY ({}),\n", pk_cols.join(", ")));
                }
            }
            for idx in indexes.iter().filter(|i| !i.is_primary) {
                let unique_kw = if idx.is_unique { "UNIQUE " } else { "" };
                let idx_cols: Vec<String> = idx
                    .columns
                    .iter()
                    .filter_map(|ic| columns.iter().find(|c| c.col_idx == ic.col_idx))
                    .map(|c| format!("`{}`", c.name))
                    .collect();
                if !idx_cols.is_empty() {
                    ddl.push_str(&format!(
                        "  {}KEY `{}` ({}),\n",
                        unique_kw,
                        idx.name,
                        idx_cols.join(", ")
                    ));
                }
            }
            if ddl.ends_with(",\n") {
                ddl.truncate(ddl.len() - 2);
                ddl.push('\n');
            }
            ddl.push_str(&format!(
                ") ENGINE=InnoDB COLLATE={}",
                effective_table_collation(&table_def, database_collation.as_deref())
            ));
            Ok(QueryResult::Rows {
                columns: vec![
                    ColumnMeta::computed("Table", DataType::Text),
                    ColumnMeta::computed("Create Table", DataType::Text),
                ],
                rows: vec![vec![
                    Value::Text(table_def.table_name.clone()),
                    Value::Text(ddl),
                ]],
            })
        }
        Stmt::ShowCreateTrigger(s) => {
            let db = ddl_database(&s.table.database, ctx);
            let snap = ctx
                .conn_txn
                .as_ref()
                .map(|conn| txn.active_snapshot(conn))
                .unwrap_or_else(|| txn.snapshot());
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
            let table_def = if let Some(schema) = s.table.schema.as_deref() {
                reader
                    .get_table_in_database(&db, schema, &s.table.name)?
                    .ok_or_else(|| DbError::TableNotFound {
                        name: s.table.name.clone(),
                    })?
            } else {
                let mut found = None;
                for schema in &ctx.search_path {
                    if let Some(def) = reader.get_table_in_database(&db, schema, &s.table.name)? {
                        found = Some(def);
                        break;
                    }
                }
                found.ok_or_else(|| DbError::TableNotFound {
                    name: s.table.name.clone(),
                })?
            };
            let trigger = table_def
                .triggers
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(&s.name))
                .ok_or_else(|| DbError::TriggerNotFound {
                    name: s.name.clone(),
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
        Stmt::ShowTableStatus(s) => {
            let db = ctx.effective_database().to_string();
            let snap = ctx
                .conn_txn
                .as_ref()
                .map(|conn| txn.active_snapshot(conn))
                .unwrap_or_else(|| txn.snapshot());
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
            let mut seen = std::collections::HashSet::new();
            let mut tables = Vec::new();
            if let Some(schema) = s.schema.as_deref() {
                tables = reader.list_tables_in_database(&db, schema)?;
            } else {
                for schema in &ctx.search_path {
                    for table in reader.list_tables_in_database(&db, schema)? {
                        if seen.insert(table.table_name.clone()) {
                            tables.push(table);
                        }
                    }
                }
            }
            let out_cols = vec![
                ColumnMeta::computed("Name", DataType::Text),
                ColumnMeta::computed("Engine", DataType::Text),
                ColumnMeta::computed("Version", DataType::Int),
                ColumnMeta::computed("Row_format", DataType::Text),
                ColumnMeta::computed("Rows", DataType::BigInt),
                ColumnMeta::computed("Avg_row_length", DataType::BigInt),
                ColumnMeta::computed("Data_length", DataType::BigInt),
                ColumnMeta::computed("Max_data_length", DataType::BigInt),
                ColumnMeta::computed("Index_length", DataType::BigInt),
                ColumnMeta::computed("Data_free", DataType::BigInt),
                ColumnMeta::computed("Auto_increment", DataType::BigInt),
                ColumnMeta::computed("Create_time", DataType::Text),
                ColumnMeta::computed("Update_time", DataType::Text),
                ColumnMeta::computed("Check_time", DataType::Text),
                ColumnMeta::computed("Collation", DataType::Text),
                ColumnMeta::computed("Checksum", DataType::Text),
                ColumnMeta::computed("Create_options", DataType::Text),
                ColumnMeta::computed("Comment", DataType::Text),
            ];
            let mut rows: Vec<Row> = Vec::new();
            for table in tables {
                if let Some(pat) = &s.like_pattern {
                    if !sql_like_match(&table.table_name, pat) {
                        continue;
                    }
                }
                let stats = reader.list_stats(table.id).unwrap_or_default();
                let row_count = stats.first().map(|st| st.row_count).unwrap_or(0) as i64;
                rows.push(vec![
                    Value::Text(table.table_name),
                    Value::Text("InnoDB".into()),
                    Value::Int(10),
                    Value::Text("Dynamic".into()),
                    Value::BigInt(row_count),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Text("utf8mb4_general_ci".into()),
                    Value::Null,
                    Value::Text("".into()),
                    Value::Text("".into()),
                ]);
            }
            Ok(QueryResult::Rows {
                columns: out_cols,
                rows,
            })
        }
        Stmt::ShowEngines => Ok(execute_show_engines()),
        Stmt::ShowCharset => Ok(execute_show_charset()),
        Stmt::ShowCollation => Ok(execute_show_collation()),
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
        Stmt::ShowNotifications => {
            let rows = ctx
                .drain_notifications()
                .into_iter()
                .map(|notif| vec![Value::Text(notif.channel), Value::Text(notif.payload)])
                .collect();
            Ok(QueryResult::Rows {
                columns: vec![
                    ColumnMeta::computed("channel", DataType::Text),
                    ColumnMeta::computed("payload", DataType::Text),
                ],
                rows,
            })
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
        Stmt::ShowVariables | Stmt::ShowStatus => Ok(QueryResult::Rows {
            columns: vec![
                ColumnMeta::computed("Variable_name", DataType::Text),
                ColumnMeta::computed("Value", DataType::Text),
            ],
            rows: vec![],
        }),
        Stmt::ShowCreateView(s) => {
            let db = ddl_database(&s.view.database, ctx);
            let snap = ctx
                .conn_txn
                .as_ref()
                .map(|conn| txn.active_snapshot(conn))
                .unwrap_or_else(|| txn.snapshot());
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
            let def = if let Some(schema) = s.view.schema.as_deref() {
                reader
                    .get_table_in_database(&db, schema, &s.view.name)?
                    .ok_or_else(|| DbError::TableNotFound {
                        name: s.view.name.clone(),
                    })?
            } else {
                let mut found = None;
                for schema in &ctx.search_path {
                    if let Some(d) = reader.get_table_in_database(&db, schema, &s.view.name)? {
                        found = Some(d);
                        break;
                    }
                }
                found.ok_or_else(|| DbError::TableNotFound {
                    name: s.view.name.clone(),
                })?
            };
            if !def.is_view() {
                return Err(DbError::InvalidValue {
                    reason: format!("'{}' is not a view", s.view.name),
                });
            }
            let defining_query = def.defining_query.clone().unwrap_or_default();
            let ddl = format!("CREATE VIEW `{}` AS {}", def.table_name, defining_query);
            Ok(QueryResult::Rows {
                columns: vec![
                    ColumnMeta::computed("View", DataType::Text),
                    ColumnMeta::computed("Create View", DataType::Text),
                ],
                rows: vec![vec![
                    Value::Text(def.table_name.clone()),
                    Value::Text(ddl),
                ]],
            })
        }
        Stmt::ShowSchemas(s) => execute_show_schemas(s, storage, txn, ctx),
        _ => Err(DbError::NotImplemented {
            feature: "read-only executor does not handle this statement type".into(),
        }),
    }
}

pub fn execute(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
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
        Stmt::Checkpoint => {
            txn.checkpoint(storage)?;
            Ok(QueryResult::Empty)
        }
        // Phase 20.7: BACKUP/RESTORE — handled outside the autocommit wrapper
        // like CHECKPOINT (they run their own checkpoint internally).
        // Wired in step 7; stub returns NotImplemented until then.
        Stmt::Backup(b) => execute_backup(b, storage, txn),
        Stmt::Restore(r) => execute_restore(r),
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
    txn: &TxnManager,
    conn_txn: ConnectionTxn,
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
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
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    sp: Savepoint,
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
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
