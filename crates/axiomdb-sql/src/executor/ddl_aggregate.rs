fn execute_create_aggregate(
    stmt: CreateAggregateStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    schema: &str,
) -> Result<QueryResult, DbError> {
    if stmt.arg_types.len() != 1 {
        return Err(DbError::NotImplemented {
            feature: "CREATE AGGREGATE with non-unary signatures".into(),
        });
    }

    let helper_kind = crate::custom_aggregate::resolve_custom_aggregate_helper(
        &stmt.sfunc,
        &stmt.stype,
        stmt.finalfunc.as_deref(),
    )?;

    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if reader
        .get_aggregate(schema, &stmt.name, stmt.arg_types.len())?
        .is_some()
    {
        return Err(DbError::InvalidValue {
            reason: format!(
                "aggregate '{}({} arg)' already exists in schema '{}'",
                stmt.name,
                stmt.arg_types.len(),
                schema
            ),
        });
    }

    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.create_aggregate(axiomdb_catalog::AggregateDef {
        schema_name: schema.to_string(),
        name: stmt.name,
        arg_types: stmt.arg_types,
        sfunc: stmt.sfunc,
        stype: stmt.stype,
        finalfunc: stmt.finalfunc,
        helper_kind,
    })?;
    Ok(QueryResult::Empty)
}

fn execute_drop_aggregate(
    stmt: DropAggregateStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    schema: &str,
) -> Result<QueryResult, DbError> {
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    let deleted = writer.delete_aggregate(schema, &stmt.name, stmt.arg_types.len())?;
    if !deleted {
        return Err(DbError::InvalidValue {
            reason: format!(
                "aggregate '{}({} arg)' not found in schema '{}'",
                stmt.name,
                stmt.arg_types.len(),
                schema
            ),
        });
    }
    Ok(QueryResult::Empty)
}
