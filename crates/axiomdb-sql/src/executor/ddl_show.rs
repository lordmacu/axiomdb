// ── SHOW TABLES / SHOW COLUMNS / DESCRIBE (4.20) ─────────────────────────────

fn execute_show_databases(
    _stmt: ShowDatabasesStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let databases = reader.list_databases()?;
    let rows: Vec<Row> = databases
        .into_iter()
        .map(|db| vec![Value::Text(db.name)])
        .collect();
    Ok(QueryResult::Rows {
        columns: vec![ColumnMeta::computed("Database", DataType::Text)],
        rows,
    })
}

fn execute_use_database(
    stmt: UseDatabaseStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(ctx.conn_txn.as_ref().expect("conn_txn for use_database"));
    let mut reader = CatalogReader::new(storage, snap)?;
    if !reader.database_exists(&stmt.name)? {
        return Err(DbError::DatabaseNotFound { name: stmt.name });
    }
    ctx.set_current_database(stmt.name);
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_create_database(
    stmt: CreateDatabaseStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.database_exists(&stmt.name)? {
        return Err(DbError::DatabaseAlreadyExists { name: stmt.name });
    }
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.create_database(&stmt.name)?;
    // Every new database gets a `public` schema.
    writer.create_schema(&stmt.name, "public")?;
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_create_schema(
    stmt: crate::ast::CreateSchemaStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.schema_exists(database, &stmt.name)? {
        if stmt.if_not_exists {
            return Ok(QueryResult::Empty);
        }
        return Err(DbError::SchemaAlreadyExists {
            name: stmt.name,
        });
    }
    CatalogWriter::new(storage, txn, conn_txn)?.create_schema(database, &stmt.name)?;
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_drop_database(
    stmt: DropDatabaseStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    if ctx.selected_database() == Some(stmt.name.as_str()) {
        return Err(DbError::ActiveDatabaseDrop { name: stmt.name });
    }

    let snap = txn.active_snapshot(ctx.conn_txn.as_ref().expect("conn_txn for drop_database"));
    let tables = {
        let mut reader = CatalogReader::new(storage, snap)?;
        if !reader.database_exists(&stmt.name)? {
            if stmt.if_exists {
                return Ok(QueryResult::Affected {
                    count: 0,
                    last_insert_id: None,
                });
            }
            return Err(DbError::DatabaseNotFound { name: stmt.name });
        }
        reader.list_tables_owned_by_database(&stmt.name)?
    };

    for table in tables {
        let conn = ctx.conn_txn.as_mut().expect("conn_txn for drop_database tables");
        drop_table_fully(storage, txn, conn, table.id)?;
    }
    {
        let conn = ctx.conn_txn.as_mut().expect("conn_txn for drop_database bindings");
        CatalogWriter::new(storage, txn, conn)?.drop_table_database_bindings_for_database(&stmt.name)?;
        let conn = ctx.conn_txn.as_mut().expect("conn_txn for drop_database");
        let _ = CatalogWriter::new(storage, txn, conn)?.drop_database(&stmt.name)?;
    }
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_show_tables(
    stmt: crate::ast::ShowTablesStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let tables = reader.list_tables_in_database(database, schema)?;

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

fn execute_show_columns(
    stmt: crate::ast::ShowColumnsStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;

    let table_def =
        reader
            .get_table_in_database(database, schema, &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
            })?;
    let columns = reader.list_columns(table_def.id)?;

    let out_cols = vec![
        ColumnMeta::computed("Field", DataType::Text),
        ColumnMeta::computed("Type", DataType::Text),
        ColumnMeta::computed("Null", DataType::Text),
        ColumnMeta::computed("Key", DataType::Text),
        ColumnMeta::computed("Default", DataType::Text),
        ColumnMeta::computed("Extra", DataType::Text),
    ];

    let rows: Vec<Row> = columns
        .iter()
        .map(|c| {
            let type_str = column_type_to_sql_name(c.col_type);
            let null_str = if c.nullable { "YES" } else { "NO" };
            let extra = if c.auto_increment {
                "auto_increment"
            } else {
                ""
            };
            vec![
                Value::Text(c.name.clone()),
                Value::Text(type_str.into()),
                Value::Text(null_str.into()),
                Value::Text("".into()), // Key — deferred
                Value::Null,            // Default — deferred
                Value::Text(extra.into()),
            ]
        })
        .collect();

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// `SHOW INDEX FROM table` / `SHOW INDEXES FROM table` / `SHOW KEYS FROM table`
///
/// Returns one row per indexed column (matching MySQL's `SHOW INDEX` output).
/// Columns: Table, Non_unique, Key_name, Seq_in_index, Column_name,
///          Collation, Cardinality, Sub_part, Packed, Null, Index_type,
///          Comment, Index_comment, Visible.
pub(crate) fn execute_show_index(
    stmt: crate::ast::ShowIndexStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;

    let table_def =
        reader
            .get_table_in_database(database, schema, &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
            })?;
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

    let table_name = &stmt.table.name;
    let mut rows: Vec<Row> = Vec::new();

    // Emit one row per indexed column.
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
                Value::Text(table_name.clone()),
                non_unique.clone(),
                Value::Text(key_name.clone()),
                Value::Int((seq + 1) as i32),
                Value::Text(col_name),
                Value::Text("A".into()),    // Collation: Ascending
                Value::Int(0),              // Cardinality: unknown (stats deferred)
                Value::Null,                // Sub_part
                Value::Null,                // Packed
                Value::Text(nullable_flag.into()),
                Value::Text("BTREE".into()),
                Value::Text("".into()),     // Comment
                Value::Text("".into()),     // Index_comment
                Value::Text("YES".into()),  // Visible
            ]);
        }
    }

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Returns the SQL type name string for display in SHOW COLUMNS / DESCRIBE.
fn column_type_to_sql_name(ct: ColumnType) -> &'static str {
    match ct {
        ColumnType::Bool => "BOOL",
        ColumnType::Int => "INT",
        ColumnType::BigInt => "BIGINT",
        ColumnType::Float => "REAL",
        ColumnType::Text => "TEXT",
        ColumnType::Bytes => "BYTES",
        ColumnType::Timestamp => "TIMESTAMP",
        ColumnType::Uuid => "UUID",
    }
}

