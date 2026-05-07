// ── SHOW TABLES / SHOW COLUMNS / DESCRIBE (4.20) ─────────────────────────────

fn execute_show_databases(
    _stmt: ShowDatabasesStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.database_exists(&stmt.name)? {
        return Err(DbError::DatabaseAlreadyExists { name: stmt.name });
    }
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.create_database(&stmt.name, stmt.collation.clone())?;
    // Every new database gets a `public` schema.
    writer.create_schema(&stmt.name, "public")?;
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_create_schema(
    stmt: crate::ast::CreateSchemaStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.schema_exists(database, &stmt.name)? {
        if stmt.if_not_exists {
            return Ok(QueryResult::Empty);
        }
        return Err(DbError::SchemaAlreadyExists { name: stmt.name });
    }
    CatalogWriter::new(storage, txn, conn_txn)?.create_schema(database, &stmt.name)?;
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_drop_database(
    stmt: DropDatabaseStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
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
        let conn = ctx
            .conn_txn
            .as_mut()
            .expect("conn_txn for drop_database tables");
        drop_table_fully(storage, txn, conn, table.id)?;
    }
    {
        let conn = ctx
            .conn_txn
            .as_mut()
            .expect("conn_txn for drop_database bindings");
        CatalogWriter::new(storage, txn, conn)?
            .drop_table_database_bindings_for_database(&stmt.name)?;
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    search_path: Option<&[String]>,
    default_schema: Option<&str>,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let mut seen = std::collections::HashSet::new();
    let mut tables = Vec::new();
    if let Some(schema) = stmt.schema.as_deref() {
        tables = reader.list_tables_in_database(database, schema)?;
    } else if let Some(search_path) = search_path {
        for schema in search_path {
            for table in reader.list_tables_in_database(database, schema)? {
                if seen.insert(table.table_name.clone()) {
                    tables.push(table);
                }
            }
        }
    } else {
        tables = reader.list_tables_in_database(database, "public")?;
    }

    let schema = stmt
        .schema
        .as_deref()
        .or(default_schema)
        .unwrap_or("public");
    let col_name = format!("Tables_in_{schema}");
    if stmt.full {
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

fn show_table_type_name(table: &axiomdb_catalog::TableDef) -> &'static str {
    if table.is_view() {
        "VIEW"
    } else if table.is_materialized_view() {
        "MATERIALIZED VIEW"
    } else {
        "BASE TABLE"
    }
}

pub(crate) fn collation_display_name(canonical: Option<&str>) -> &'static str {
    match canonical.unwrap_or("es") {
        "binary" => "utf8mb4_bin",
        "es" => "utf8mb4_general_ci",
        _ => "utf8mb4_general_ci",
    }
}

pub(crate) fn effective_table_collation(
    table: &axiomdb_catalog::TableDef,
    database_default_collation: Option<&str>,
) -> &'static str {
    collation_display_name(
        table
            .default_collation
            .as_deref()
            .or(database_default_collation),
    )
}

pub(crate) fn effective_column_collation(
    column: &axiomdb_catalog::ColumnDef,
    table: &axiomdb_catalog::TableDef,
    database_default_collation: Option<&str>,
) -> Option<&'static str> {
    match column.col_type {
        ColumnType::Text => Some(collation_display_name(
            column
                .collation
                .as_deref()
                .or(table.default_collation.as_deref())
                .or(database_default_collation),
        )),
        _ => None,
    }
}

fn execute_show_columns(
    stmt: crate::ast::ShowColumnsStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    search_path: Option<&[String]>,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let table_def = if let Some(schema) = stmt.table.schema.as_deref() {
        reader
            .get_table_in_database(database, schema, &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
            })?
    } else if let Some(search_path) = search_path {
        let mut found = None;
        for schema in search_path {
            if let Some(def) = reader.get_table_in_database(database, schema, &stmt.table.name)? {
                found = Some(def);
                break;
            }
        }
        found.ok_or_else(|| DbError::TableNotFound {
            name: stmt.table.name.clone(),
        })?
    } else {
        reader
            .get_table_in_database(database, "public", &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
            })?
    };
    let columns = reader.list_columns(table_def.id)?;
    let database_collation = reader
        .get_database(database)?
        .and_then(|db| db.default_collation);

    let base_cols = vec![
        ColumnMeta::computed("Field", DataType::Text),
        ColumnMeta::computed("Type", DataType::Text),
        ColumnMeta::computed("Null", DataType::Text),
        ColumnMeta::computed("Key", DataType::Text),
        ColumnMeta::computed("Default", DataType::Text),
        ColumnMeta::computed("Extra", DataType::Text),
    ];

    let out_cols = if stmt.full {
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
            let extra = if c.auto_increment {
                "auto_increment"
            } else {
                ""
            };
            let mut row = vec![
                Value::Text(c.name.clone()),
                Value::Text(type_str),
                Value::Text(null_str.into()),
                Value::Text("".into()), // Key — deferred
                Value::Null,            // Default — deferred
                Value::Text(extra.into()),
            ];
            if stmt.full {
                let coll = effective_column_collation(c, &table_def, database_collation.as_deref())
                    .map(|name| Value::Text(name.into()))
                    .unwrap_or(Value::Null);
                row.push(coll);
                row.push(Value::Text("select,insert,update,references".into()));
                row.push(Value::Text("".into())); // Comment
            }
            row
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    search_path: Option<&[String]>,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let table_def = if let Some(schema) = stmt.table.schema.as_deref() {
        reader
            .get_table_in_database(database, schema, &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
            })?
    } else if let Some(search_path) = search_path {
        let mut found = None;
        for schema in search_path {
            if let Some(def) = reader.get_table_in_database(database, schema, &stmt.table.name)? {
                found = Some(def);
                break;
            }
        }
        found.ok_or_else(|| DbError::TableNotFound {
            name: stmt.table.name.clone(),
        })?
    } else {
        reader
            .get_table_in_database(database, "public", &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
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
                Value::Text("A".into()), // Collation: Ascending
                Value::Int(0),           // Cardinality: unknown (stats deferred)
                Value::Null,             // Sub_part
                Value::Null,             // Packed
                Value::Text(nullable_flag.into()),
                Value::Text("BTREE".into()),
                Value::Text("".into()),    // Comment
                Value::Text("".into()),    // Index_comment
                Value::Text("YES".into()), // Visible
            ]);
        }
    }

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// `SHOW CREATE TABLE t` — reconstruct the DDL for a table from catalog data.
///
/// Returns a two-column result set: `Table`, `Create Table` — matching MySQL output.
/// The reconstructed DDL covers: column types, NOT NULL, AUTO_INCREMENT, PRIMARY KEY,
/// named unique / non-unique indexes, and the storage engine notation.
fn execute_show_create_table(
    stmt: crate::ast::ShowCreateTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    search_path: Option<&[String]>,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let table_def = if let Some(schema) = stmt.table.schema.as_deref() {
        reader
            .get_table_in_database(database, schema, &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
            })?
    } else if let Some(search_path) = search_path {
        let mut found = None;
        for schema in search_path {
            if let Some(def) = reader.get_table_in_database(database, schema, &stmt.table.name)? {
                found = Some(def);
                break;
            }
        }
        found.ok_or_else(|| DbError::TableNotFound {
            name: stmt.table.name.clone(),
        })?
    } else {
        reader
            .get_table_in_database(database, "public", &stmt.table.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: stmt.table.name.clone(),
            })?
    };

    let columns = reader.list_columns(table_def.id)?;
    let indexes = reader.list_indexes(table_def.id)?;
    let database_collation = reader
        .get_database(database)?
        .and_then(|db| db.default_collation);
    if table_def.is_view() {
        return Err(DbError::InvalidValue {
            reason: format!(
                "'{}' is a view — use SHOW CREATE VIEW",
                table_def.table_name
            ),
        });
    }
    if table_def.is_materialized_view() {
        let defining_query = table_def
            .defining_query
            .clone()
            .ok_or_else(|| DbError::Internal {
                message: format!(
                    "materialized view '{}' is missing its defining query",
                    table_def.table_name
                ),
            })?;
        let out_cols = vec![
            ColumnMeta::computed("View", DataType::Text),
            ColumnMeta::computed("Create View", DataType::Text),
        ];
        return Ok(QueryResult::Rows {
            columns: out_cols,
            rows: vec![vec![
                Value::Text(table_def.table_name.clone()),
                Value::Text(format!(
                    "CREATE MATERIALIZED VIEW `{}` AS {}",
                    table_def.table_name, defining_query
                )),
            ]],
        });
    }

    let create_prefix = match table_def.persistence {
        axiomdb_catalog::TablePersistence::Permanent => "CREATE TABLE",
        axiomdb_catalog::TablePersistence::Temporary => "CREATE TEMPORARY TABLE",
        axiomdb_catalog::TablePersistence::Unlogged => "CREATE UNLOGGED TABLE",
    };
    let mut ddl = format!("{create_prefix} `{}` (\n", table_def.table_name);

    // Columns
    for col in &columns {
        let type_str = column_sql_type_display(col);
        let null_str = if col.nullable { "" } else { " NOT NULL" };
        let extra = if col.auto_increment {
            " AUTO_INCREMENT"
        } else {
            ""
        };
        let collate = if col.enum_type_name.is_none() {
            effective_column_collation(col, &table_def, database_collation.as_deref())
                .map(|name| format!(" COLLATE {name}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        ddl.push_str(&format!(
            "  `{}` {}{}{}{},\n",
            col.name, type_str, collate, null_str, extra
        ));
    }

    // PRIMARY KEY
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

    // Secondary indexes
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

    // Remove trailing comma+newline from last line
    if ddl.ends_with(",\n") {
        ddl.truncate(ddl.len() - 2);
        ddl.push('\n');
    }

    let engine = "InnoDB";
    ddl.push_str(&format!(
        ") ENGINE={} COLLATE={}",
        engine,
        effective_table_collation(&table_def, database_collation.as_deref())
    ));

    let out_cols = vec![
        ColumnMeta::computed("Table", DataType::Text),
        ColumnMeta::computed("Create Table", DataType::Text),
    ];
    Ok(QueryResult::Rows {
        columns: out_cols,
        rows: vec![vec![
            Value::Text(table_def.table_name.clone()),
            Value::Text(ddl),
        ]],
    })
}

/// `RENAME TABLE old TO new [, old2 TO new2 ...]` (4.3h)
fn execute_rename_table(
    stmt: crate::ast::RenameTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = "public";
    for (old_name, new_name) in stmt.pairs {
        let snap = txn.active_snapshot(conn_txn);
        let table_def = CatalogReader::new(storage, snap)?
            .get_table_in_database(database, schema, &old_name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: old_name.clone(),
            })?;
        // Check new name not already in use.
        let snap2 = txn.active_snapshot(conn_txn);
        if CatalogReader::new(storage, snap2)?
            .get_table_in_database(database, schema, &new_name)?
            .is_some()
        {
            return Err(DbError::TableAlreadyExists {
                schema: schema.to_string(),
                name: new_name.clone(),
            });
        }
        CatalogWriter::new(storage, txn, conn_txn)?.rename_table(table_def.id, new_name, schema)?;
    }
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

// ── SHOW TABLE STATUS ────────────────────────────────────────────────────────

/// `SHOW TABLE STATUS [FROM schema] [LIKE pattern]`
///
/// Returns one row per table with MySQL-compatible 18-column layout.
fn execute_show_table_status(
    stmt: crate::ast::ShowTableStatusStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    search_path: Option<&[String]>,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let mut seen = std::collections::HashSet::new();
    let mut tables = Vec::new();
    if let Some(schema) = stmt.schema.as_deref() {
        tables = reader.list_tables_in_database(database, schema)?;
    } else if let Some(search_path) = search_path {
        for schema in search_path {
            for table in reader.list_tables_in_database(database, schema)? {
                if seen.insert(table.table_name.clone()) {
                    tables.push(table);
                }
            }
        }
    } else {
        tables = reader.list_tables_in_database(database, "public")?;
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
        // Apply LIKE filter if requested.
        if let Some(pat) = &stmt.like_pattern {
            if !sql_like_match(&table.table_name, pat) {
                continue;
            }
        }

        // Look up approximate row count from stats.
        let stats = reader.list_stats(table.id).unwrap_or_default();
        let row_count = stats.first().map(|s| s.row_count).unwrap_or(0) as i64;
        let database_collation = reader
            .get_database(database)?
            .and_then(|db| db.default_collation);
        let table_collation =
            effective_table_collation(&table, database_collation.as_deref()).to_string();
        let table_name = table.table_name.clone();

        rows.push(vec![
            Value::Text(table_name),
            Value::Text("InnoDB".into()),
            Value::Int(10),
            Value::Text("Dynamic".into()),
            Value::BigInt(row_count),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::Null, // Auto_increment
            Value::Null, // Create_time
            Value::Null, // Update_time
            Value::Null, // Check_time
            Value::Text(table_collation),
            Value::Null, // Checksum
            Value::Text("".into()),
            Value::Text("".into()),
        ]);
    }

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Simple SQL LIKE pattern matching (% = any sequence, _ = any single char).
fn sql_like_match(s: &str, pattern: &str) -> bool {
    let s = s.as_bytes();
    let p = pattern.as_bytes();
    like_match(s, p)
}

fn like_match(s: &[u8], p: &[u8]) -> bool {
    match (s, p) {
        (_, [b'%', rest @ ..]) => like_match(s, rest) || (!s.is_empty() && like_match(&s[1..], p)),
        ([sc, sr @ ..], [pc, pr @ ..]) if *pc == b'_' || *pc == *sc => like_match(sr, pr),
        ([], []) => true,
        _ => false,
    }
}

// ── SHOW ENGINES / CHARSET / COLLATION ────────────────────────────────────────

/// `SHOW ENGINES` — returns the single supported engine (InnoDB-compatible).
pub(crate) fn execute_show_engines() -> QueryResult {
    let out_cols = vec![
        ColumnMeta::computed("Engine", DataType::Text),
        ColumnMeta::computed("Support", DataType::Text),
        ColumnMeta::computed("Comment", DataType::Text),
        ColumnMeta::computed("Transactions", DataType::Text),
        ColumnMeta::computed("XA", DataType::Text),
        ColumnMeta::computed("Savepoints", DataType::Text),
    ];
    QueryResult::Rows {
        columns: out_cols,
        rows: vec![vec![
            Value::Text("InnoDB".into()),
            Value::Text("DEFAULT".into()),
            Value::Text("Supports transactions, row-level locking, and foreign keys".into()),
            Value::Text("YES".into()),
            Value::Text("YES".into()),
            Value::Text("YES".into()),
        ]],
    }
}

/// `SHOW CHARSET` / `SHOW CHARACTER SET`
pub(crate) fn execute_show_charset() -> QueryResult {
    let out_cols = vec![
        ColumnMeta::computed("Charset", DataType::Text),
        ColumnMeta::computed("Description", DataType::Text),
        ColumnMeta::computed("Default collation", DataType::Text),
        ColumnMeta::computed("Maxlen", DataType::Int),
    ];
    QueryResult::Rows {
        columns: out_cols,
        rows: vec![
            vec![
                Value::Text("utf8mb4".into()),
                Value::Text("UTF-8 Unicode".into()),
                Value::Text("utf8mb4_general_ci".into()),
                Value::Int(4),
            ],
            vec![
                Value::Text("utf8".into()),
                Value::Text("UTF-8 Unicode".into()),
                Value::Text("utf8_general_ci".into()),
                Value::Int(3),
            ],
            vec![
                Value::Text("latin1".into()),
                Value::Text("cp1252 West European".into()),
                Value::Text("latin1_swedish_ci".into()),
                Value::Int(1),
            ],
            vec![
                Value::Text("binary".into()),
                Value::Text("Binary pseudo charset".into()),
                Value::Text("binary".into()),
                Value::Int(1),
            ],
        ],
    }
}

/// `SHOW COLLATION`
pub(crate) fn execute_show_collation() -> QueryResult {
    let out_cols = vec![
        ColumnMeta::computed("Collation", DataType::Text),
        ColumnMeta::computed("Charset", DataType::Text),
        ColumnMeta::computed("Id", DataType::Int),
        ColumnMeta::computed("Default", DataType::Text),
        ColumnMeta::computed("Compiled", DataType::Text),
        ColumnMeta::computed("Sortlen", DataType::Int),
    ];
    QueryResult::Rows {
        columns: out_cols,
        rows: vec![
            vec![
                Value::Text("utf8mb4_general_ci".into()),
                Value::Text("utf8mb4".into()),
                Value::Int(45),
                Value::Text("Yes".into()),
                Value::Text("Yes".into()),
                Value::Int(1),
            ],
            vec![
                Value::Text("utf8mb4_bin".into()),
                Value::Text("utf8mb4".into()),
                Value::Int(46),
                Value::Text("".into()),
                Value::Text("Yes".into()),
                Value::Int(1),
            ],
            vec![
                Value::Text("utf8_general_ci".into()),
                Value::Text("utf8".into()),
                Value::Int(33),
                Value::Text("Yes".into()),
                Value::Text("Yes".into()),
                Value::Int(1),
            ],
            vec![
                Value::Text("latin1_swedish_ci".into()),
                Value::Text("latin1".into()),
                Value::Int(8),
                Value::Text("Yes".into()),
                Value::Text("Yes".into()),
                Value::Int(1),
            ],
            vec![
                Value::Text("binary".into()),
                Value::Text("binary".into()),
                Value::Int(63),
                Value::Text("Yes".into()),
                Value::Text("Yes".into()),
                Value::Int(1),
            ],
        ],
    }
}

/// Returns the SQL type name string for display in SHOW COLUMNS / DESCRIBE.
fn column_type_to_sql_name(ct: ColumnType) -> &'static str {
    match ct {
        ColumnType::Bool => "BOOL",
        ColumnType::Int => "INT",
        ColumnType::BigInt => "BIGINT",
        ColumnType::Float => "REAL",
        ColumnType::Decimal => "DECIMAL",
        ColumnType::Text => "TEXT",
        ColumnType::Json => "JSON",
        ColumnType::Jsonb => "JSONB",
        ColumnType::Bytes => "BYTES",
        ColumnType::Date => "DATE",
        ColumnType::Timestamp => "TIMESTAMP",
        ColumnType::Uuid => "UUID",
    }
}

fn column_sql_type_display(col: &axiomdb_catalog::ColumnDef) -> String {
    col.enum_type_name
        .clone()
        .unwrap_or_else(|| column_type_to_sql_name(col.col_type).into())
}
