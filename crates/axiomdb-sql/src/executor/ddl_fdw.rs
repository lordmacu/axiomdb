// ── CREATE / DROP SERVER and CREATE / DROP FOREIGN TABLE (Phase 22b.2) ────────

fn options_to_json(opts: &[(String, String)]) -> String {
    // Build a simple JSON object from key-value pairs.
    let pairs: Vec<String> = opts
        .iter()
        .map(|(k, v)| {
            let k_esc = k.replace('\\', "\\\\").replace('"', "\\\"");
            let v_esc = v.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{k_esc}\":\"{v_esc}\"")
        })
        .collect();
    format!("{{{}}}", pairs.join(","))
}

fn execute_create_server(
    stmt: crate::ast::CreateServerStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;

    if reader.get_foreign_server(&stmt.name)?.is_some() {
        if stmt.if_not_exists {
            return Ok(QueryResult::Empty);
        }
        return Err(DbError::InvalidValue {
            reason: format!("server '{}' already exists", stmt.name),
        });
    }
    drop(reader);

    let options_json = options_to_json(&stmt.options);
    let def = ForeignServerDef {
        name: stmt.name,
        fdw_name: stmt.fdw_name,
        options: options_json,
    };
    CatalogWriter::new(storage, txn, conn_txn)?.upsert_foreign_server(def)?;
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_drop_server(
    stmt: crate::ast::DropServerStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let deleted = CatalogWriter::new(storage, txn, conn_txn)?.delete_foreign_server(&stmt.name)?;
    if !deleted && !stmt.if_exists {
        return Err(DbError::InvalidValue {
            reason: format!("server '{}' does not exist", stmt.name),
        });
    }
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_create_foreign_table(
    stmt: crate::ast::CreateForeignTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let table_name = &stmt.table.name;

    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;

    // Ensure the referenced server exists.
    if reader.get_foreign_server(&stmt.server_name)?.is_none() {
        return Err(DbError::InvalidValue {
            reason: format!("server '{}' does not exist", stmt.server_name),
        });
    }

    if reader.get_foreign_table(schema, table_name)?.is_some() {
        if stmt.if_not_exists {
            return Ok(QueryResult::Empty);
        }
        return Err(DbError::InvalidValue {
            reason: format!("foreign table '{schema}.{table_name}' already exists"),
        });
    }
    let _ = database;

    let new_id = reader.next_foreign_table_id()?;
    drop(reader);

    let columns: Vec<ForeignColumnDef> = stmt
        .columns
        .into_iter()
        .map(|c| ForeignColumnDef {
            name: c.name,
            col_type: c.col_type,
            nullable: c.nullable,
        })
        .collect();

    let options_json = options_to_json(&stmt.options);

    let def = ForeignTableDef {
        table_id: new_id,
        table_name: table_name.clone(),
        schema_name: schema.to_string(),
        server_name: stmt.server_name,
        columns,
        options: options_json,
    };
    CatalogWriter::new(storage, txn, conn_txn)?.insert_foreign_table(def)?;
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}

fn execute_drop_foreign_table(
    stmt: crate::ast::DropForeignTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let deleted =
        CatalogWriter::new(storage, txn, conn_txn)?.delete_foreign_table(schema, &stmt.table.name)?;
    if !deleted && !stmt.if_exists {
        return Err(DbError::InvalidValue {
            reason: format!(
                "foreign table '{}.{}' does not exist",
                schema, stmt.table.name
            ),
        });
    }
    Ok(QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    })
}
