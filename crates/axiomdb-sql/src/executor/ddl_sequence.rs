fn execute_create_sequence(
    stmt: crate::ast::CreateSequenceStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
    default_schema: &str,
) -> Result<QueryResult, DbError> {
    validate_sequence_options(&stmt)?;
    let schema = stmt.sequence.schema.as_deref().unwrap_or(default_schema);
    let name = &stmt.sequence.name;
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;

    if reader.get_sequence(schema, name)?.is_some() {
        if stmt.if_not_exists {
            return Ok(QueryResult::Empty);
        }
        return Err(DbError::InvalidValue {
            reason: format!("sequence '{name}' already exists"),
        });
    }

    if reader
        .get_table_in_database(database, schema, name)?
        .is_some()
    {
        return Err(DbError::InvalidValue {
            reason: format!("object '{name}' already exists as a table, view, or materialized view"),
        });
    }

    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.create_sequence(axiomdb_catalog::SequenceDef {
        schema_name: schema.to_string(),
        name: name.clone(),
        last_value: stmt.start_value,
        start_value: stmt.start_value,
        increment: stmt.increment,
        min_value: stmt.min_value,
        max_value: stmt.max_value,
        cycle: stmt.cycle,
        cache_size: stmt.cache_size,
        is_called: false,
    })?;
    Ok(QueryResult::Empty)
}

fn execute_create_enum_type(
    stmt: crate::ast::CreateEnumTypeStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
    default_schema: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.enum_type.schema.as_deref().unwrap_or(default_schema);
    let name = &stmt.enum_type.name;
    if stmt.enum_type.database.is_some() {
        return Err(DbError::InvalidValue {
            reason: format!("enum type '{name}' cannot be qualified with a database"),
        });
    }

    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader.get_enum_type(schema, name)?.is_some() {
        return Err(DbError::InvalidValue {
            reason: format!("enum type '{schema}.{name}' already exists"),
        });
    }
    if reader
        .get_table_in_database(database, schema, name)?
        .is_some()
    {
        return Err(DbError::InvalidValue {
            reason: format!("object '{name}' already exists as a table, view, or materialized view"),
        });
    }
    if reader.get_sequence(schema, name)?.is_some() {
        return Err(DbError::InvalidValue {
            reason: format!("object '{name}' already exists as a sequence"),
        });
    }

    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.create_enum_type(axiomdb_catalog::EnumTypeDef {
        schema_name: schema.to_string(),
        name: name.clone(),
        labels: stmt.labels,
    })?;
    Ok(QueryResult::Empty)
}

fn execute_drop_sequence(
    stmt: crate::ast::DropSequenceStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
    default_schema: &str,
) -> Result<QueryResult, DbError> {
    for seq_ref in stmt.sequences {
        let schema = seq_ref.schema.as_deref().unwrap_or(default_schema);
        let snap = txn.active_snapshot(conn_txn);
        let mut reader = CatalogReader::new(storage, snap)?;
        if reader
            .get_table_in_database(database, schema, &seq_ref.name)?
            .is_some()
        {
            return Err(DbError::InvalidValue {
                reason: format!("'{}' is not a sequence", seq_ref.name),
            });
        }

        let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
        let deleted = writer.delete_sequence(schema, &seq_ref.name)?;
        if !deleted && !stmt.if_exists {
            return Err(DbError::InvalidValue {
                reason: format!("sequence '{}' not found", seq_ref.name),
            });
        }
    }
    Ok(QueryResult::Empty)
}

fn validate_sequence_options(stmt: &crate::ast::CreateSequenceStmt) -> Result<(), DbError> {
    if stmt.increment == 0 {
        return Err(DbError::InvalidValue {
            reason: "CREATE SEQUENCE INCREMENT BY cannot be 0".into(),
        });
    }
    if stmt.min_value > stmt.max_value {
        return Err(DbError::InvalidValue {
            reason: "CREATE SEQUENCE MINVALUE cannot exceed MAXVALUE".into(),
        });
    }
    if stmt.start_value < stmt.min_value || stmt.start_value > stmt.max_value {
        return Err(DbError::InvalidValue {
            reason: "CREATE SEQUENCE START value must be within MINVALUE and MAXVALUE".into(),
        });
    }
    if stmt.cache_size != 1 {
        return Err(DbError::NotImplemented {
            feature: "CREATE SEQUENCE CACHE values greater than 1".into(),
        });
    }
    Ok(())
}
