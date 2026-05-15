// ── READ_PARQUET executor ─────────────────────────────────────────────────────
//
// Materializes a Parquet file into Vec<Row> and feeds it through the standard
// SELECT pipeline. Schema was validated at bind time (analyzer_bind.rs).
// Included into executor/mod.rs — all mod.rs names are in scope here.

/// Load all rows from a Parquet file. Returns (col_names, rows).
fn read_parquet_rows(path: &str) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::Field;
    use parquet::schema::types::Type;

    let file = std::fs::File::open(path).map_err(|e| {
        DbError::Io(std::io::Error::new(
            e.kind(),
            format!("READ_PARQUET: cannot open '{path}': {e}"),
        ))
    })?;
    let reader = SerializedFileReader::new(file).map_err(|e| DbError::InvalidValue {
        reason: format!("READ_PARQUET: cannot read '{path}': {e}"),
    })?;

    let schema = reader.metadata().file_metadata().schema_descr_ptr();
    let root = schema.root_schema();
    let col_names: Vec<String> = match root {
        Type::GroupType { fields, .. } => fields.iter().map(|f| f.name().to_string()).collect(),
        Type::PrimitiveType { .. } => vec!["value".to_string()],
    };

    let mut rows: Vec<Vec<Value>> = Vec::new();
    let iter = reader.get_row_iter(None).map_err(|e| DbError::InvalidValue {
        reason: format!("READ_PARQUET: row iter error for '{path}': {e}"),
    })?;

    for row_result in iter {
        let row = row_result.map_err(|e| DbError::InvalidValue {
            reason: format!("READ_PARQUET: row read error in '{path}': {e}"),
        })?;
        let n = col_names.len();
        let mut values: Vec<Value> = Vec::with_capacity(n);
        for (_, field) in row.get_column_iter() {
            values.push(parquet_field_to_value(field));
        }
        while values.len() < n {
            values.push(Value::Null);
        }
        rows.push(values);
    }

    Ok((col_names, rows))
}

fn parquet_field_to_value(field: &parquet::record::Field) -> Value {
    use parquet::record::Field;
    match field {
        Field::Null => Value::Null,
        Field::Bool(b) => Value::Bool(*b),
        Field::Byte(n) => Value::Int(*n as i32),
        Field::Short(n) => Value::Int(*n as i32),
        Field::Int(n) => Value::Int(*n),
        Field::Long(n) => Value::BigInt(*n),
        Field::UByte(n) => Value::Int(*n as i32),
        Field::UShort(n) => Value::Int(*n as i32),
        Field::UInt(n) => Value::BigInt(*n as i64),
        Field::ULong(n) => Value::BigInt(*n as i64),
        Field::Float(f) => Value::Real(*f as f64),
        Field::Double(f) => Value::Real(*f),
        Field::Decimal(d) => {
            let scale = d.scale() as u8;
            let src = d.data();
            let mut buf = [0u8; 16];
            let start = 16usize.saturating_sub(src.len());
            if !src.is_empty() && src[0] & 0x80 != 0 {
                buf[..start].fill(0xff);
            }
            buf[start..].copy_from_slice(src);
            let mantissa = i128::from_be_bytes(buf);
            Value::Decimal(mantissa, scale)
        }
        Field::Str(s) => Value::Text(s.clone()),
        Field::Bytes(b) => Value::Text(String::from_utf8_lossy(b.data()).into_owned()),
        Field::Date(days) => Value::Date(*days),
        Field::TimestampMillis(ts) => Value::Timestamp(ts * 1_000),
        Field::TimestampMicros(ts) => Value::Timestamp(*ts),
        Field::Float16(f) => Value::Real(f32::from(*f) as f64),
        Field::Group(_) | Field::ListInternal(_) | Field::MapInternal(_) => Value::Null,
    }
}

/// Execute `SELECT … FROM READ_PARQUET('path') …`.
/// Mirrors the GENERATE_SERIES TVF pattern in select_core.rs.
fn execute_select_read_parquet_source(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    let rp = match stmt.from.take() {
        Some(FromClause::ReadParquet(rp)) => *rp,
        _ => unreachable!("execute_select_read_parquet_source called with non-ReadParquet FROM"),
    };

    let (col_names, derived_rows) = read_parquet_rows(&rp.path)?;

    let final_names: Vec<String> = if rp.column_aliases.is_empty() {
        col_names
    } else {
        rp.column_aliases.clone()
    };
    let alias = rp.alias.unwrap_or_else(|| "read_parquet".to_string());

    let derived_cols: Vec<ColumnMeta> = final_names
        .iter()
        .map(|name| ColumnMeta {
            name: name.clone(),
            data_type: DataType::Text,
            nullable: true,
            table_name: Some(alias.clone()),
        })
        .collect();

    if !stmt.joins.is_empty() {
        let mut temp_ctx = SessionContext::new();
        let temp_bloom = crate::bloom::BloomRegistry::new();
        let temp_exec_ctx = ExecutionContext::new(storage, txn, &temp_bloom, None);
        let first_source = join_source_schema_from_derived(&alias, derived_cols);
        return execute_select_with_joins_first_materialized(
            stmt,
            first_source,
            derived_rows,
            &temp_exec_ctx,
            conn_txn,
            &mut temp_ctx,
        );
    }

    let mut combined_rows: Vec<Row> = Vec::new();
    let mut sq_cache: SubqueryCache = HashMap::new();
    let mut in_set_cache: InSetCache = HashMap::new();
    let mut corr_cache: CorrelatedCache = HashMap::new();
    for values in derived_rows {
        if let Some(ref wc) = stmt.where_clause {
            let mut temp_ctx2 = SessionContext::new();
            let temp_bloom2 = crate::bloom::BloomRegistry::new();
            let mut runner = ExecSubqueryRunner {
                storage,
                txn,
                bloom: &temp_bloom2,
                ctx: &mut temp_ctx2,
                outer_row: &values,
                cache: Some(&mut sq_cache),
                in_set_cache: Some(&mut in_set_cache),
                correlated_cache: Some(&mut corr_cache),
                materialized: None,
            };
            if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
                continue;
            }
        }
        combined_rows.push(values);
    }

    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
    }

    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    if !stmt.distinct_on.is_empty() {
        combined_rows =
            apply_distinct_on(combined_rows, &stmt.distinct_on, &resolved_ob, &stmt.columns)?;
    } else if !resolved_ob.is_empty()
        && stmt.limit.is_some()
        && !stmt.distinct
        && !stmt.calc_found_rows
    {
        let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
        let top_n = offset_n + limit_n.unwrap_or(usize::MAX).min(usize::MAX - offset_n);
        combined_rows = apply_order_by_top_n(combined_rows, &resolved_ob, top_n)?;
    } else {
        combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
    }

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
