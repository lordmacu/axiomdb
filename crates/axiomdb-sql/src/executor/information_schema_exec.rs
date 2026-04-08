// ── INFORMATION_SCHEMA executor (4.20c) ──────────────────────────────────────
//
// Intercepts SELECT statements whose FROM source is an information_schema
// virtual table, generates rows from catalog metadata, and applies the
// standard WHERE / GROUP BY / ORDER BY / LIMIT pipeline.

/// Execute a SELECT against a virtual INFORMATION_SCHEMA table.
///
/// Called from `execute_select` when `from_table_ref.schema` is
/// `"information_schema"`. The `stmt` already has columns resolved by the
/// analyzer (using synthetic `ColumnDef`s from `crate::information_schema`),
/// so WHERE, ORDER BY, and projection work without any special treatment.
fn execute_information_schema_select(
    stmt: SelectStmt,
    from_table_ref: TableRef,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: Option<&ConnectionTxn>,
    default_database: &str,
) -> Result<QueryResult, DbError> {
    let table_name_lower = from_table_ref.name.to_ascii_lowercase();

    // Generate virtual rows from the catalog.
    let (derived_cols, derived_rows) =
        generate_is_rows(&table_name_lower, storage, txn, conn_txn, default_database)?;

    // Apply WHERE filter.
    let mut combined_rows: Vec<Row> = Vec::new();
    for values in derived_rows {
        if let Some(ref wc) = stmt.where_clause {
            let mut temp_ctx = SessionContext::new();
            let temp_bloom = crate::bloom::BloomRegistry::new();
            let mut runner = ExecSubqueryRunner {
                storage,
                txn,
                bloom: &temp_bloom,
                ctx: &mut temp_ctx,
                outer_row: &values,
            };
            if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
                continue;
            }
        }
        combined_rows.push(values);
    }

    // GROUP BY / aggregation (uncommon but supported).
    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
    }

    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    combined_rows = apply_order_by(combined_rows, &resolved_ob)?;

    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = combined_rows
        .iter()
        .map(|v| project_row(&stmt.columns, v))
        .collect::<Result<Vec<_>, _>>()?;

    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    if stmt.calc_found_rows {
        set_found_rows(rows.len() as u64);
    }
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Generates the column metadata and all virtual rows for a given IS table.
///
/// Returns `(column_metas, rows)`.  `column_metas` are used by
/// `build_derived_output_columns` to satisfy `SELECT *` expansion and column
/// name inference.
fn generate_is_rows(
    table_name: &str,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: Option<&ConnectionTxn>,
    default_database: &str,
) -> Result<(Vec<ColumnMeta>, Vec<Row>), DbError> {
    let snap = conn_txn
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());

    // Build column metadata from the IS schema definition.
    let col_schema = crate::information_schema::is_table_cols(table_name).ok_or_else(|| {
        DbError::TableNotFound {
            name: format!("information_schema.{table_name}"),
        }
    })?;
    let derived_cols: Vec<ColumnMeta> = col_schema
        .iter()
        .map(|(name, ct)| ColumnMeta::computed(*name, column_type_to_datatype(*ct)))
        .collect();

    let mut reader = CatalogReader::new(storage, snap)?;

    let rows = match table_name {
        "tables" => generate_is_tables_rows(&mut reader, default_database)?,
        "columns" => generate_is_columns_rows(&mut reader, default_database)?,
        "key_column_usage" => generate_is_key_column_usage_rows(&mut reader, default_database)?,
        "table_constraints" => generate_is_table_constraints_rows(&mut reader, default_database)?,
        "referential_constraints" => {
            generate_is_referential_constraints_rows(&mut reader, default_database)?
        }
        "statistics" => generate_is_statistics_rows(&mut reader, default_database)?,
        _ => {
            return Err(DbError::TableNotFound {
                name: format!("information_schema.{table_name}"),
            })
        }
    };

    Ok((derived_cols, rows))
}

// ── Row generators ────────────────────────────────────────────────────────────

/// `information_schema.TABLES` — one row per user table.
///
/// Column order matches `IS_TABLES_COLS` in `information_schema.rs`.
fn generate_is_tables_rows(
    reader: &mut CatalogReader,
    _default_database: &str,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = reader.list_tables_in_database(&db.name, "public")?;
        for t in tables {
            rows.push(vec![
                Value::Text("def".into()),             // TABLE_CATALOG
                Value::Text(db.name.clone()),          // TABLE_SCHEMA
                Value::Text(t.table_name.clone()),     // TABLE_NAME
                Value::Text("BASE TABLE".into()),      // TABLE_TYPE
                Value::Text("InnoDB".into()),          // ENGINE
                Value::BigInt(10),                     // VERSION
                Value::Text("Dynamic".into()),         // ROW_FORMAT
                Value::Null,                           // TABLE_ROWS
                Value::BigInt(0),                      // AVG_ROW_LENGTH
                Value::BigInt(0),                      // DATA_LENGTH
                Value::BigInt(0),                      // MAX_DATA_LENGTH
                Value::BigInt(0),                      // INDEX_LENGTH
                Value::BigInt(0),                      // DATA_FREE
                Value::Null,                           // AUTO_INCREMENT
                Value::Null,                           // CREATE_TIME
                Value::Null,                           // UPDATE_TIME
                Value::Null,                           // CHECK_TIME
                Value::Text("utf8mb4_0900_ai_ci".into()), // TABLE_COLLATION
                Value::Null,                           // CHECKSUM
                Value::Text("".into()),                // CREATE_OPTIONS
                Value::Text("".into()),                // TABLE_COMMENT
            ]);
        }
    }

    Ok(rows)
}

/// `information_schema.COLUMNS` — one row per column of every user table.
///
/// Column order matches `IS_COLUMNS_COLS` in `information_schema.rs`.
fn generate_is_columns_rows(
    reader: &mut CatalogReader,
    _default_database: &str,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = reader.list_tables_in_database(&db.name, "public")?;
        for t in tables {
            let columns = reader.list_columns(t.id)?;
            for col in &columns {
                let data_type_str = column_type_to_is_data_type(col.col_type);
                let col_type_str = column_type_to_column_type_str(col.col_type);
                let is_nullable = if col.nullable { "YES" } else { "NO" };
                let extra = if col.auto_increment { "auto_increment" } else { "" };
                let char_max_len = match col.col_type {
                    ColumnType::Text => Value::BigInt(65535),
                    _ => Value::Null,
                };
                let num_prec = match col.col_type {
                    ColumnType::Int => Value::BigInt(10),
                    ColumnType::BigInt => Value::BigInt(19),
                    ColumnType::Float => Value::BigInt(12),
                    _ => Value::Null,
                };
                rows.push(vec![
                    Value::Text("def".into()),                         // TABLE_CATALOG
                    Value::Text(db.name.clone()),                      // TABLE_SCHEMA
                    Value::Text(t.table_name.clone()),                 // TABLE_NAME
                    Value::Text(col.name.clone()),                     // COLUMN_NAME
                    Value::BigInt((col.col_idx as i64) + 1),          // ORDINAL_POSITION
                    Value::Null,                                       // COLUMN_DEFAULT
                    Value::Text(is_nullable.into()),                   // IS_NULLABLE
                    Value::Text(data_type_str.into()),                 // DATA_TYPE
                    char_max_len,                                      // CHARACTER_MAXIMUM_LENGTH
                    Value::Null,                                       // CHARACTER_OCTET_LENGTH
                    num_prec,                                          // NUMERIC_PRECISION
                    Value::Null,                                       // NUMERIC_SCALE
                    Value::Null,                                       // DATETIME_PRECISION
                    Value::Null,                                       // CHARACTER_SET_NAME
                    Value::Null,                                       // COLLATION_NAME
                    Value::Text(col_type_str.into()),                  // COLUMN_TYPE
                    Value::Text("".into()),                            // COLUMN_KEY
                    Value::Text(extra.into()),                         // EXTRA
                    Value::Text("select,insert,update,references".into()), // PRIVILEGES
                    Value::Text("".into()),                            // COLUMN_COMMENT
                    Value::Text("".into()),                            // GENERATION_EXPRESSION
                    Value::Null,                                       // SRS_ID
                ]);
            }
        }
    }

    Ok(rows)
}

/// `information_schema.KEY_COLUMN_USAGE` — one row per indexed column.
///
/// Covers PRIMARY KEY and unique index columns, plus FK columns if present.
/// Column order matches `IS_KEY_COLUMN_USAGE_COLS`.
fn generate_is_key_column_usage_rows(
    reader: &mut CatalogReader,
    _default_database: &str,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = reader.list_tables_in_database(&db.name, "public")?;
        for t in tables {
            let columns = reader.list_columns(t.id)?;
            let indexes = reader.list_indexes(t.id)?;
            for idx in &indexes {
                if !idx.is_primary && !idx.is_unique {
                    continue;
                }
                let constraint_name = if idx.is_primary {
                    "PRIMARY".to_string()
                } else {
                    idx.name.clone()
                };
                for (seq, ic) in idx.columns.iter().enumerate() {
                    let col_name = columns
                        .iter()
                        .find(|c| c.col_idx == ic.col_idx as u16)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    rows.push(vec![
                        Value::Text("def".into()),             // CONSTRAINT_CATALOG
                        Value::Text(db.name.clone()),          // CONSTRAINT_SCHEMA
                        Value::Text(constraint_name.clone()),  // CONSTRAINT_NAME
                        Value::Text("def".into()),             // TABLE_CATALOG
                        Value::Text(db.name.clone()),          // TABLE_SCHEMA
                        Value::Text(t.table_name.clone()),     // TABLE_NAME
                        Value::Text(col_name),                 // COLUMN_NAME
                        Value::BigInt((seq + 1) as i64),       // ORDINAL_POSITION
                        Value::Null,                           // POSITION_IN_UNIQUE_CONSTRAINT
                        Value::Null,                           // REFERENCED_TABLE_SCHEMA
                        Value::Null,                           // REFERENCED_TABLE_NAME
                        Value::Null,                           // REFERENCED_COLUMN_NAME
                    ]);
                }
            }
        }
    }

    Ok(rows)
}

/// `information_schema.TABLE_CONSTRAINTS` — one row per PK / unique / check constraint.
///
/// Column order matches `IS_TABLE_CONSTRAINTS_COLS`.
fn generate_is_table_constraints_rows(
    reader: &mut CatalogReader,
    _default_database: &str,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = reader.list_tables_in_database(&db.name, "public")?;
        for t in tables {
            let indexes = reader.list_indexes(t.id)?;
            for idx in &indexes {
                let (constraint_name, constraint_type) = if idx.is_primary {
                    ("PRIMARY".to_string(), "PRIMARY KEY")
                } else if idx.is_unique {
                    (idx.name.clone(), "UNIQUE")
                } else {
                    continue;
                };
                rows.push(vec![
                    Value::Text("def".into()),              // CONSTRAINT_CATALOG
                    Value::Text(db.name.clone()),           // CONSTRAINT_SCHEMA
                    Value::Text(constraint_name),           // CONSTRAINT_NAME
                    Value::Text(db.name.clone()),           // TABLE_SCHEMA
                    Value::Text(t.table_name.clone()),      // TABLE_NAME
                    Value::Text(constraint_type.into()),    // CONSTRAINT_TYPE
                    Value::Text("YES".into()),              // ENFORCED
                ]);
            }
        }
    }

    Ok(rows)
}

/// `information_schema.REFERENTIAL_CONSTRAINTS` — FK constraints.
///
/// Returns empty for now (FK metadata is in `axiom_foreign_keys` but not
/// yet surfaced through the IS layer; 6.6x tracking ticket).
/// Column order matches `IS_REFERENTIAL_CONSTRAINTS_COLS`.
fn generate_is_referential_constraints_rows(
    _reader: &mut CatalogReader,
    _default_database: &str,
) -> Result<Vec<Row>, DbError> {
    Ok(vec![])
}

/// `information_schema.STATISTICS` — one row per indexed column.
///
/// Column order matches `IS_STATISTICS_COLS`.
fn generate_is_statistics_rows(
    reader: &mut CatalogReader,
    _default_database: &str,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = reader.list_tables_in_database(&db.name, "public")?;
        for t in tables {
            let columns = reader.list_columns(t.id)?;
            let indexes = reader.list_indexes(t.id)?;
            for idx in &indexes {
                let index_name = if idx.is_primary {
                    "PRIMARY".to_string()
                } else {
                    idx.name.clone()
                };
                let non_unique: i64 = if idx.is_unique || idx.is_primary { 0 } else { 1 };
                for (seq, ic) in idx.columns.iter().enumerate() {
                    let col_name = columns
                        .iter()
                        .find(|c| c.col_idx == ic.col_idx as u16)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    let nullable = columns
                        .iter()
                        .find(|c| c.col_idx == ic.col_idx as u16)
                        .map(|c| if c.nullable { "YES" } else { "" })
                        .unwrap_or("YES");
                    rows.push(vec![
                        Value::Text("def".into()),             // TABLE_CATALOG
                        Value::Text(db.name.clone()),          // TABLE_SCHEMA
                        Value::Text(t.table_name.clone()),     // TABLE_NAME
                        Value::BigInt(non_unique),             // NON_UNIQUE
                        Value::Text(db.name.clone()),          // INDEX_SCHEMA
                        Value::Text(index_name.clone()),       // INDEX_NAME
                        Value::BigInt((seq + 1) as i64),       // SEQ_IN_INDEX
                        Value::Text(col_name),                 // COLUMN_NAME
                        Value::Text("A".into()),               // COLLATION
                        Value::BigInt(0),                      // CARDINALITY
                        Value::Null,                           // SUB_PART
                        Value::Null,                           // PACKED
                        Value::Text(nullable.into()),          // NULLABLE
                        Value::Text("BTREE".into()),           // INDEX_TYPE
                        Value::Text("".into()),                // COMMENT
                        Value::Text("".into()),                // INDEX_COMMENT
                        Value::Text("YES".into()),             // IS_VISIBLE
                        Value::Null,                           // EXPRESSION
                    ]);
                }
            }
        }
    }

    Ok(rows)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Maps a `ColumnType` to the MySQL `DATA_TYPE` string shown in IS.COLUMNS.
fn column_type_to_is_data_type(ct: ColumnType) -> &'static str {
    match ct {
        ColumnType::Bool => "tinyint",
        ColumnType::Int => "int",
        ColumnType::BigInt => "bigint",
        ColumnType::Float => "double",
        ColumnType::Text => "text",
        ColumnType::Bytes => "blob",
        ColumnType::Timestamp => "datetime",
        ColumnType::Uuid => "varchar",
    }
}

/// Maps a `ColumnType` to the MySQL `COLUMN_TYPE` string shown in IS.COLUMNS.
fn column_type_to_column_type_str(ct: ColumnType) -> &'static str {
    match ct {
        ColumnType::Bool => "tinyint(1)",
        ColumnType::Int => "int",
        ColumnType::BigInt => "bigint",
        ColumnType::Float => "double",
        ColumnType::Text => "text",
        ColumnType::Bytes => "blob",
        ColumnType::Timestamp => "datetime",
        ColumnType::Uuid => "varchar(36)",
    }
}
