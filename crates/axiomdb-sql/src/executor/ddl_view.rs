fn execute_create_view(
    stmt: crate::ast::CreateViewStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    _search_path: Option<&[String]>,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt
        .view
        .schema
        .as_deref()
        .unwrap_or("public")
        .to_string();
    let name = &stmt.view.name;

    // Validate column-name count if explicitly provided.
    // (We can only do this after analysis; the SELECT output width is in columns.)
    // The full check is deferred to analyzer — here we just store.

    let snap = txn.active_snapshot(conn_txn);
    let existing = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.get_table_in_database(database, &schema, name)?
    };

    match existing {
        Some(def) if def.is_view() => {
            if stmt.or_replace {
                // Replace the defining query in-place.
                let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
                writer.replace_view_query(def.id, stmt.query_sql)?;
                return Ok(QueryResult::Empty);
            }
            return Err(DbError::InvalidValue {
                reason: format!("view '{name}' already exists"),
            });
        }
        Some(_) => {
            // Exists but is a base table or materialized view.
            return Err(DbError::InvalidValue {
                reason: format!(
                    "object '{name}' already exists as a table or materialized view"
                ),
            });
        }
        None => {}
    }

    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.create_view(&schema, name, stmt.query_sql)?;
    Ok(QueryResult::Empty)
}

fn execute_show_create_view(
    stmt: crate::ast::ShowCreateViewStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    search_path: Option<&[String]>,
    database: &str,
) -> Result<QueryResult, DbError> {
    let def = resolve_relation_def(storage, txn, conn_txn, &stmt.view, search_path, database)?
        .ok_or_else(|| DbError::TableNotFound {
            name: stmt.view.name.clone(),
        })?;

    if !def.is_view() {
        return Err(DbError::InvalidValue {
            reason: format!("'{}' is not a view", stmt.view.name),
        });
    }

    let defining_query = def.defining_query.clone().unwrap_or_default();
    let ddl = format!("CREATE VIEW `{}` AS {}", def.table_name, defining_query);

    let out_cols = vec![
        ColumnMeta::computed("View", DataType::Text),
        ColumnMeta::computed("Create View", DataType::Text),
    ];
    Ok(QueryResult::Rows {
        columns: out_cols,
        rows: vec![vec![
            Value::Text(def.table_name.clone()),
            Value::Text(ddl),
        ]],
    })
}

fn execute_drop_view(
    stmt: crate::ast::DropViewStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    search_path: Option<&[String]>,
    database: &str,
) -> Result<QueryResult, DbError> {
    for view_ref in stmt.views {
        let existing =
            resolve_relation_def(storage, txn, conn_txn, &view_ref, search_path, database)?;
        match existing {
            None => {
                if stmt.if_exists {
                    continue;
                }
                return Err(DbError::InvalidValue {
                    reason: format!("view '{}' not found", view_ref.name),
                });
            }
            Some(def) if !def.is_view() => {
                return Err(DbError::InvalidValue {
                    reason: format!("'{}' is not a view", view_ref.name),
                });
            }
            Some(def) => {
                let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
                // Views have root_page_id=0 — no physical pages to free.
                writer.delete_table(def.id)?;
            }
        }
    }
    Ok(QueryResult::Empty)
}
