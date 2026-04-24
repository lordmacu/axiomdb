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
    temp_schema: Option<&str>,
) -> Result<QueryResult, DbError> {
    let table_name_lower = from_table_ref.name.to_ascii_lowercase();

    // Generate virtual rows from the catalog.
    let (derived_cols, derived_rows) =
        generate_is_rows(
            &table_name_lower,
            storage,
            txn,
            conn_txn,
            default_database,
            temp_schema,
        )?;

    // Apply WHERE filter.
    let mut combined_rows: Vec<Row> = Vec::new();
    let mut sq_cache_is: SubqueryCache = HashMap::new();
    let mut in_set_cache_is: InSetCache = HashMap::new();
    let mut corr_cache_is: CorrelatedCache = HashMap::new();
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
                cache: Some(&mut sq_cache_is),
                in_set_cache: Some(&mut in_set_cache_is),
                correlated_cache: Some(&mut corr_cache_is),
                materialized: None,
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
    let mut rows = project_rows_with_window_support(&stmt.columns, &combined_rows, |expr, row| {
        eval(expr, row)
    })?;

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
    temp_schema: Option<&str>,
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
        "tables" => generate_is_tables_rows(&mut reader, default_database, temp_schema)?,
        "columns" => generate_is_columns_rows(&mut reader, default_database, temp_schema)?,
        "key_column_usage" => {
            generate_is_key_column_usage_rows(&mut reader, default_database, temp_schema)?
        }
        "table_constraints" => {
            generate_is_table_constraints_rows(&mut reader, default_database, temp_schema)?
        }
        "referential_constraints" => {
            generate_is_referential_constraints_rows(&mut reader, default_database, temp_schema)?
        }
        "statistics" => generate_is_statistics_rows(&mut reader, default_database, temp_schema)?,
        _ => {
            return Err(DbError::TableNotFound {
                name: format!("information_schema.{table_name}"),
            })
        }
    };

    Ok((derived_cols, rows))
}

fn visible_is_tables_for_session(
    reader: &mut CatalogReader,
    database: &str,
    temp_schema: Option<&str>,
) -> Result<Vec<axiomdb_catalog::TableDef>, DbError> {
    let tables = reader.list_tables_owned_by_database(database)?;
    Ok(tables
        .into_iter()
        .filter(|table| {
            table.persistence != axiomdb_catalog::TablePersistence::Temporary
                || Some(table.schema_name.as_str()) == temp_schema
        })
        .collect())
}

fn is_collation_display_name(canonical: Option<&str>) -> &'static str {
    match canonical.unwrap_or("es") {
        "binary" => "utf8mb4_bin",
        "es" => "utf8mb4_general_ci",
        _ => "utf8mb4_general_ci",
    }
}

fn is_effective_table_collation(
    table: &axiomdb_catalog::TableDef,
    database_default_collation: Option<&str>,
) -> &'static str {
    is_collation_display_name(
        table
            .default_collation
            .as_deref()
            .or(database_default_collation),
    )
}

fn is_effective_column_collation(
    column: &axiomdb_catalog::ColumnDef,
    table: &axiomdb_catalog::TableDef,
    database_default_collation: Option<&str>,
) -> Option<&'static str> {
    match column.col_type {
        ColumnType::Text => Some(is_collation_display_name(
            column
                .collation
                .as_deref()
                .or(table.default_collation.as_deref())
                .or(database_default_collation),
        )),
        _ => None,
    }
}

// ── Row generators ────────────────────────────────────────────────────────────

/// `information_schema.TABLES` — one row per user table.
///
/// Column order matches `IS_TABLES_COLS` in `information_schema.rs`.
fn generate_is_tables_rows(
    reader: &mut CatalogReader,
    _default_database: &str,
    temp_schema: Option<&str>,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = visible_is_tables_for_session(reader, &db.name, temp_schema)?;
        for t in tables {
            rows.push(vec![
                Value::Text("def".into()),                // TABLE_CATALOG
                Value::Text(db.name.clone()),             // TABLE_SCHEMA
                Value::Text(t.table_name.clone()),        // TABLE_NAME
                Value::Text(show_table_type_name(&t).into()), // TABLE_TYPE
                Value::Text("InnoDB".into()),             // ENGINE
                Value::BigInt(10),                        // VERSION
                Value::Text("Dynamic".into()),            // ROW_FORMAT
                Value::Null,                              // TABLE_ROWS
                Value::BigInt(0),                         // AVG_ROW_LENGTH
                Value::BigInt(0),                         // DATA_LENGTH
                Value::BigInt(0),                         // MAX_DATA_LENGTH
                Value::BigInt(0),                         // INDEX_LENGTH
                Value::BigInt(0),                         // DATA_FREE
                Value::Null,                              // AUTO_INCREMENT
                Value::Null,                              // CREATE_TIME
                Value::Null,                              // UPDATE_TIME
                Value::Null,                              // CHECK_TIME
                Value::Text(
                    is_effective_table_collation(&t, db.default_collation.as_deref()).into(),
                ), // TABLE_COLLATION
                Value::Null,                              // CHECKSUM
                Value::Text("".into()),                   // CREATE_OPTIONS
                Value::Text("".into()),                   // TABLE_COMMENT
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
    temp_schema: Option<&str>,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = visible_is_tables_for_session(reader, &db.name, temp_schema)?;
        for t in tables {
            let columns = reader.list_columns(t.id)?;
            for col in &columns {
                let data_type_str = column_type_to_is_data_type(col.col_type);
                let col_type_str = column_type_to_column_type_str(col.col_type);
                let is_nullable = if col.nullable { "YES" } else { "NO" };
                let extra = if col.auto_increment {
                    "auto_increment"
                } else {
                    ""
                };
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
                    Value::Text("def".into()),                             // TABLE_CATALOG
                    Value::Text(db.name.clone()),                          // TABLE_SCHEMA
                    Value::Text(t.table_name.clone()),                     // TABLE_NAME
                    Value::Text(col.name.clone()),                         // COLUMN_NAME
                    Value::BigInt((col.col_idx as i64) + 1),               // ORDINAL_POSITION
                    Value::Null,                                           // COLUMN_DEFAULT
                    Value::Text(is_nullable.into()),                       // IS_NULLABLE
                    Value::Text(data_type_str.into()),                     // DATA_TYPE
                    char_max_len,                     // CHARACTER_MAXIMUM_LENGTH
                    Value::Null,                      // CHARACTER_OCTET_LENGTH
                    num_prec,                         // NUMERIC_PRECISION
                    Value::Null,                      // NUMERIC_SCALE
                    Value::Null,                      // DATETIME_PRECISION
                    is_effective_column_collation(col, &t, db.default_collation.as_deref())
                        .map(|_| Value::Text("utf8mb4".into()))
                        .unwrap_or(Value::Null), // CHARACTER_SET_NAME
                    is_effective_column_collation(col, &t, db.default_collation.as_deref())
                        .map(|name| Value::Text(name.into()))
                        .unwrap_or(Value::Null), // COLLATION_NAME
                    Value::Text(col_type_str.into()), // COLUMN_TYPE
                    Value::Text("".into()),           // COLUMN_KEY
                    Value::Text(extra.into()),        // EXTRA
                    Value::Text("select,insert,update,references".into()), // PRIVILEGES
                    Value::Text("".into()),           // COLUMN_COMMENT
                    Value::Text("".into()),           // GENERATION_EXPRESSION
                    Value::Null,                      // SRS_ID
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
    temp_schema: Option<&str>,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = visible_is_tables_for_session(reader, &db.name, temp_schema)?;
        for t in tables {
            let columns = reader.list_columns(t.id)?;
            let indexes = reader.list_indexes(t.id)?;
            let constraints = reader.list_constraints(t.id)?;
            let exclusion_by_index: std::collections::HashMap<u32, &axiomdb_catalog::ConstraintDef> =
                constraints
                    .iter()
                    .filter(|c| {
                        c.kind == axiomdb_catalog::ConstraintKind::Exclusion
                            && c.owned_index_id != 0
                    })
                    .map(|c| (c.owned_index_id, c))
                    .collect();
            for idx in &indexes {
                if exclusion_by_index.contains_key(&idx.index_id) {
                    continue;
                }
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
                        .find(|c| c.col_idx == ic.col_idx)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    rows.push(vec![
                        Value::Text("def".into()),            // CONSTRAINT_CATALOG
                        Value::Text(db.name.clone()),         // CONSTRAINT_SCHEMA
                        Value::Text(constraint_name.clone()), // CONSTRAINT_NAME
                        Value::Text("def".into()),            // TABLE_CATALOG
                        Value::Text(db.name.clone()),         // TABLE_SCHEMA
                        Value::Text(t.table_name.clone()),    // TABLE_NAME
                        Value::Text(col_name),                // COLUMN_NAME
                        Value::BigInt((seq + 1) as i64),      // ORDINAL_POSITION
                        Value::Null,                          // POSITION_IN_UNIQUE_CONSTRAINT
                        Value::Null,                          // REFERENCED_TABLE_SCHEMA
                        Value::Null,                          // REFERENCED_TABLE_NAME
                        Value::Null,                          // REFERENCED_COLUMN_NAME
                    ]);
                }
            }
            for constraint in constraints
                .iter()
                .filter(|c| c.kind == axiomdb_catalog::ConstraintKind::Exclusion)
            {
                for (seq, elem) in constraint.exclude_elements.iter().enumerate() {
                    let col_name = columns
                        .iter()
                        .find(|c| c.col_idx == elem.col_idx)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    rows.push(vec![
                        Value::Text("def".into()),              // CONSTRAINT_CATALOG
                        Value::Text(db.name.clone()),           // CONSTRAINT_SCHEMA
                        Value::Text(constraint.name.clone()),   // CONSTRAINT_NAME
                        Value::Text("def".into()),              // TABLE_CATALOG
                        Value::Text(db.name.clone()),           // TABLE_SCHEMA
                        Value::Text(t.table_name.clone()),      // TABLE_NAME
                        Value::Text(col_name),                  // COLUMN_NAME
                        Value::BigInt((seq + 1) as i64),        // ORDINAL_POSITION
                        Value::Null,                            // POSITION_IN_UNIQUE_CONSTRAINT
                        Value::Null,                            // REFERENCED_TABLE_SCHEMA
                        Value::Null,                            // REFERENCED_TABLE_NAME
                        Value::Null,                            // REFERENCED_COLUMN_NAME
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
    temp_schema: Option<&str>,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = visible_is_tables_for_session(reader, &db.name, temp_schema)?;
        for t in tables {
            let indexes = reader.list_indexes(t.id)?;
            let constraints = reader.list_constraints(t.id)?;
            let owned_exclusion_index_ids: std::collections::HashSet<u32> = constraints
                .iter()
                .filter(|c| {
                    c.kind == axiomdb_catalog::ConstraintKind::Exclusion
                        && c.owned_index_id != 0
                })
                .map(|c| c.owned_index_id)
                .collect();
            for idx in &indexes {
                if owned_exclusion_index_ids.contains(&idx.index_id) {
                    continue;
                }
                let (constraint_name, constraint_type) = if idx.is_primary {
                    ("PRIMARY".to_string(), "PRIMARY KEY")
                } else if idx.is_unique {
                    (idx.name.clone(), "UNIQUE")
                } else {
                    continue;
                };
                rows.push(vec![
                    Value::Text("def".into()),           // CONSTRAINT_CATALOG
                    Value::Text(db.name.clone()),        // CONSTRAINT_SCHEMA
                    Value::Text(constraint_name),        // CONSTRAINT_NAME
                    Value::Text(db.name.clone()),        // TABLE_SCHEMA
                    Value::Text(t.table_name.clone()),   // TABLE_NAME
                    Value::Text(constraint_type.into()), // CONSTRAINT_TYPE
                    Value::Text("YES".into()),           // ENFORCED
                ]);
            }
            for constraint in constraints
                .iter()
                .filter(|c| c.kind == axiomdb_catalog::ConstraintKind::Exclusion)
            {
                rows.push(vec![
                    Value::Text("def".into()),                 // CONSTRAINT_CATALOG
                    Value::Text(db.name.clone()),              // CONSTRAINT_SCHEMA
                    Value::Text(constraint.name.clone()),      // CONSTRAINT_NAME
                    Value::Text(db.name.clone()),              // TABLE_SCHEMA
                    Value::Text(t.table_name.clone()),         // TABLE_NAME
                    Value::Text("EXCLUSION".into()),           // CONSTRAINT_TYPE
                    Value::Text("YES".into()),                 // ENFORCED
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
    _temp_schema: Option<&str>,
) -> Result<Vec<Row>, DbError> {
    Ok(vec![])
}

/// `information_schema.STATISTICS` — one row per indexed column.
///
/// Column order matches `IS_STATISTICS_COLS`.
fn generate_is_statistics_rows(
    reader: &mut CatalogReader,
    _default_database: &str,
    temp_schema: Option<&str>,
) -> Result<Vec<Row>, DbError> {
    let databases = reader.list_databases()?;
    let mut rows = Vec::new();

    for db in &databases {
        let tables = visible_is_tables_for_session(reader, &db.name, temp_schema)?;
        for t in tables {
            let columns = reader.list_columns(t.id)?;
            let indexes = reader.list_indexes(t.id)?;
            for idx in &indexes {
                let index_name = if idx.is_primary {
                    "PRIMARY".to_string()
                } else {
                    idx.name.clone()
                };
                let non_unique: i64 = if idx.is_unique || idx.is_primary {
                    0
                } else {
                    1
                };
                for (seq, ic) in idx.columns.iter().enumerate() {
                    let col_name = columns
                        .iter()
                        .find(|c| c.col_idx == ic.col_idx)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    let nullable = columns
                        .iter()
                        .find(|c| c.col_idx == ic.col_idx)
                        .map(|c| if c.nullable { "YES" } else { "" })
                        .unwrap_or("YES");
                    rows.push(vec![
                        Value::Text("def".into()),         // TABLE_CATALOG
                        Value::Text(db.name.clone()),      // TABLE_SCHEMA
                        Value::Text(t.table_name.clone()), // TABLE_NAME
                        Value::BigInt(non_unique),         // NON_UNIQUE
                        Value::Text(db.name.clone()),      // INDEX_SCHEMA
                        Value::Text(index_name.clone()),   // INDEX_NAME
                        Value::BigInt((seq + 1) as i64),   // SEQ_IN_INDEX
                        Value::Text(col_name),             // COLUMN_NAME
                        Value::Text("A".into()),           // COLLATION
                        Value::BigInt(0),                  // CARDINALITY
                        Value::Null,                       // SUB_PART
                        Value::Null,                       // PACKED
                        Value::Text(nullable.into()),      // NULLABLE
                        Value::Text("BTREE".into()),       // INDEX_TYPE
                        Value::Text("".into()),            // COMMENT
                        Value::Text("".into()),            // INDEX_COMMENT
                        Value::Text("YES".into()),         // IS_VISIBLE
                        Value::Null,                       // EXPRESSION
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
        ColumnType::Decimal => "decimal",
        ColumnType::Text => "text",
        ColumnType::Json => "json",
        ColumnType::Jsonb => "jsonb",
        ColumnType::Bytes => "blob",
        ColumnType::Date => "date",
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
        ColumnType::Decimal => "decimal",
        ColumnType::Text => "text",
        ColumnType::Json => "json",
        ColumnType::Jsonb => "jsonb",
        ColumnType::Bytes => "blob",
        ColumnType::Date => "date",
        ColumnType::Timestamp => "datetime",
        ColumnType::Uuid => "varchar(36)",
    }
}
