// ── Stored procedure DDL (Phase 16.7) ─────────────────────────────────────────

/// Maps a SQL `DataType` to the catalog `ColumnType` used to store a procedure
/// parameter's type. Note the f32/f64 naming flip: `DataType::Float` is f32
/// (→ `ColumnType::Float32`) and `DataType::Real` is f64 (→ `ColumnType::Float`).
/// Type modifiers (precision/length) are not preserved — only the base type.
fn proc_param_column_type(dt: &axiomdb_types::DataType) -> axiomdb_catalog::ColumnType {
    use axiomdb_catalog::ColumnType as CT;
    use axiomdb_types::DataType as DT;
    match dt {
        DT::Bool => CT::Bool,
        DT::TinyInt => CT::TinyInt,
        DT::SmallInt => CT::SmallInt,
        DT::Int => CT::Int,
        DT::BigInt => CT::BigInt,
        DT::Float => CT::Float32, // f32
        DT::Real => CT::Float,    // f64
        DT::Decimal => CT::Decimal,
        DT::Text => CT::Text,
        DT::Bytes => CT::Bytes,
        DT::Date => CT::Date,
        DT::Timestamp => CT::Timestamp,
        DT::TimestampTz => CT::TimestampTz,
        DT::Uuid => CT::Uuid,
        DT::Json => CT::Json,
        DT::Jsonb => CT::Jsonb,
        DT::Array(_) => CT::Array,
        DT::Range(_) => CT::Range,
        DT::Money => CT::Money,
        DT::Composite(_) => CT::Composite,
        DT::Ltree => CT::Ltree,
        DT::Xml => CT::Xml,
    }
}

fn execute_create_procedure(
    stmt: crate::ast::CreateProcedureStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    default_schema: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt
        .name
        .schema
        .clone()
        .unwrap_or_else(|| default_schema.to_string());
    let name = stmt.name.name.clone();

    // Validate: parameter names must be unique (case-insensitive).
    let mut seen = std::collections::HashSet::with_capacity(stmt.params.len());
    for p in &stmt.params {
        if !seen.insert(p.name.to_ascii_lowercase()) {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "duplicate parameter name '{}' in procedure '{name}'",
                    p.name
                ),
            });
        }
    }

    let params = stmt
        .params
        .iter()
        .map(|p| axiomdb_catalog::ProcParam {
            mode: p.mode,
            name: p.name.clone(),
            data_type: proc_param_column_type(&p.ty),
        })
        .collect();

    let def = axiomdb_catalog::ProcedureDef {
        schema_name: schema.clone(),
        name: name.clone(),
        params,
        language: stmt.language,
        body_sql: stmt.body_sql.clone(),
    };

    // Without OR REPLACE, reject an existing procedure with the same (schema, name).
    if !stmt.or_replace {
        let snap = txn.active_snapshot(conn_txn);
        let mut reader = CatalogReader::new(storage, snap)?;
        if reader.get_procedure(&schema, &name)?.is_some() {
            return Err(DbError::ProcedureAlreadyExists { schema, name });
        }
    }

    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.upsert_procedure(def)?;
    Ok(QueryResult::Empty)
}

fn execute_drop_procedure(
    stmt: crate::ast::DropProcedureStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    default_schema: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt
        .name
        .schema
        .clone()
        .unwrap_or_else(|| default_schema.to_string());
    let name = stmt.name.name.clone();
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    let found = writer.delete_procedure(&schema, &name)?;
    if !found && !stmt.if_exists {
        return Err(DbError::ProcedureNotFound { name });
    }
    Ok(QueryResult::Empty)
}
