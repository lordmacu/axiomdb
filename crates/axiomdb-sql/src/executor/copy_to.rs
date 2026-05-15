// ── COPY TO executor ──────────────────────────────────────────────────────────
//
// Scans the target table (all rows) and writes them to a server-side file in
// CSV, JSON, or JSONL format.
// Included directly into executor/mod.rs — all names from that module's scope
// are available here.

fn execute_copy_to(
    stmt: crate::ast::CopyToStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    use crate::ast::CopyFormat;

    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();

    let table_ref = crate::ast::TableRef::simple(stmt.table.clone());
    let resolved = resolve_table_cached(storage, txn, ctx, conn_txn, &table_ref)?;

    // Snapshot: read within the caller's transaction if active.
    let snap = if let Some(ct) = conn_txn {
        txn.active_snapshot(ct)
    } else {
        txn.snapshot()
    };

    // Full table scan — all rows, no filtering.
    let pairs = TableEngine::scan_table(
        storage,
        &resolved.def,
        &resolved.columns,
        snap,
        None,
    )?;

    let col_names: Vec<String> = resolved.columns.iter().map(|c| c.name.clone()).collect();
    let rows: Vec<Vec<Value>> = pairs.into_iter().map(|(_, row)| row).collect();
    let count = rows.len() as u64;

    let format = resolve_copy_format(&stmt.options, &stmt.path);
    let use_header = stmt.options.header.unwrap_or(format == CopyFormat::Csv);
    let delimiter = stmt.options.delimiter.unwrap_or(',');

    let file = std::fs::File::create(&stmt.path).map_err(|e| {
        DbError::Io(std::io::Error::new(
            e.kind(),
            format!("COPY TO: cannot create '{}': {e}", stmt.path),
        ))
    })?;
    let mut writer = std::io::BufWriter::new(file);

    match format {
        CopyFormat::Csv => write_csv(&mut writer, &col_names, &rows, use_header, delimiter)?,
        CopyFormat::Json => write_json_array(&mut writer, &col_names, &rows)?,
        CopyFormat::Jsonl => write_jsonl(&mut writer, &col_names, &rows)?,
        CopyFormat::Parquet => {
            drop(writer);
            let compression = stmt.options.compression.as_ref().map(|c| match c {
                crate::ast::ParquetCompression::Snappy => {
                    parquet::basic::Compression::SNAPPY
                }
                crate::ast::ParquetCompression::Uncompressed => {
                    parquet::basic::Compression::UNCOMPRESSED
                }
            });
            return write_parquet(&stmt.path, &col_names, &rows, compression);
        }
    }

    {
        use std::io::Write;
        writer.flush().map_err(|e| {
            DbError::Io(std::io::Error::new(
                e.kind(),
                format!("COPY TO: flush '{}': {e}", stmt.path),
            ))
        })?;
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── CSV writer ────────────────────────────────────────────────────────────────

fn write_csv<W: std::io::Write>(
    w: &mut W,
    col_names: &[String],
    rows: &[Vec<Value>],
    header: bool,
    delimiter: char,
) -> Result<(), DbError> {
    let mut wtr = csv::WriterBuilder::new()
        .delimiter(delimiter as u8)
        .has_headers(false) // we write the header manually if requested
        .from_writer(w);

    if header {
        wtr.write_record(col_names)
            .map_err(|e| copy_to_csv_err(e.to_string()))?;
    }

    for row in rows {
        let fields: Vec<String> = row.iter().map(value_to_csv_field).collect();
        wtr.write_record(&fields)
            .map_err(|e| copy_to_csv_err(e.to_string()))?;
    }

    wtr.flush().map_err(|e| copy_to_csv_err(e.to_string()))?;
    Ok(())
}

fn value_to_csv_field(v: &Value) -> String {
    match v {
        Value::Null => r"\N".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => format!("{f}"),
        Value::Decimal(m, s) => {
            if *s == 0 {
                m.to_string()
            } else {
                let scale = *s as u32;
                let div = 10i128.pow(scale);
                let int_part = m / div;
                let frac_part = (m % div).unsigned_abs();
                format!("{int_part}.{frac_part:0>scale$}", scale = scale as usize)
            }
        }
        Value::Text(s) => s.clone(),
        Value::Json(s) => s.clone(),
        Value::Jsonb(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Bytes(b) => b.iter().map(|byte| format!("{byte:02x}")).collect(),
        Value::Timestamp(ts) => {
            let secs = ts / 1_000_000;
            let microsecs = (ts % 1_000_000).unsigned_abs();
            format!("{secs}.{microsecs:06}")
        }
        Value::Date(days) => {
            let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            if let Some(d) = epoch.checked_add_days(chrono::Days::new((*days).max(0) as u64)) {
                d.to_string()
            } else {
                days.to_string()
            }
        }
        Value::Uuid(bytes) => {
            let u = u128::from_be_bytes(*bytes);
            format!(
                "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                (u >> 96) as u32,
                (u >> 80) as u16,
                (u >> 64) as u16,
                (u >> 48) as u16,
                u & 0xffff_ffff_ffff
            )
        }
        Value::Array(elems) => {
            let json_arr: Vec<serde_json::Value> =
                elems.iter().map(value_to_json_val).collect();
            serde_json::to_string(&json_arr).unwrap_or_default()
        }
    }
}

fn copy_to_csv_err(msg: String) -> DbError {
    DbError::Io(std::io::Error::other(format!("COPY TO CSV write error: {msg}")))
}

// ── JSON array writer ─────────────────────────────────────────────────────────

fn write_json_array<W: std::io::Write>(
    w: &mut W,
    col_names: &[String],
    rows: &[Vec<Value>],
) -> Result<(), DbError> {
    let json_rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let obj: serde_json::Map<String, serde_json::Value> = col_names
                .iter()
                .zip(row.iter())
                .map(|(name, val)| (name.clone(), value_to_json_val(val)))
                .collect();
            serde_json::Value::Object(obj)
        })
        .collect();

    let s = serde_json::to_string_pretty(&serde_json::Value::Array(json_rows))
        .map_err(|e| DbError::Io(std::io::Error::other(format!("COPY TO JSON: {e}"))))?;
    w.write_all(s.as_bytes()).map_err(|e| {
        DbError::Io(std::io::Error::new(e.kind(), format!("COPY TO JSON write: {e}")))
    })?;
    Ok(())
}

// ── JSONL writer ──────────────────────────────────────────────────────────────

fn write_jsonl<W: std::io::Write>(
    w: &mut W,
    col_names: &[String],
    rows: &[Vec<Value>],
) -> Result<(), DbError> {
    for row in rows {
        let obj: serde_json::Map<String, serde_json::Value> = col_names
            .iter()
            .zip(row.iter())
            .map(|(name, val)| (name.clone(), value_to_json_val(val)))
            .collect();
        let line = serde_json::to_string(&serde_json::Value::Object(obj))
            .map_err(|e| DbError::Io(std::io::Error::other(format!("COPY TO JSONL: {e}"))))?;
        w.write_all(line.as_bytes()).map_err(|e| {
            DbError::Io(std::io::Error::new(e.kind(), format!("COPY TO JSONL write: {e}")))
        })?;
        w.write_all(b"\n").map_err(|e| {
            DbError::Io(std::io::Error::new(e.kind(), format!("COPY TO JSONL newline: {e}")))
        })?;
    }
    Ok(())
}

// ── Value → serde_json::Value ─────────────────────────────────────────────────

fn value_to_json_val(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::Value::Number((*n).into()),
        Value::BigInt(n) => serde_json::Value::Number((*n).into()),
        Value::Real(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Decimal(_, _) => serde_json::Value::String(value_to_csv_field(v)),
        Value::Text(s) => serde_json::Value::String(s.clone()),
        Value::Json(s) => serde_json::from_str(s).unwrap_or(serde_json::Value::String(s.clone())),
        Value::Jsonb(b) => {
            let s = String::from_utf8_lossy(b).into_owned();
            serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
        }
        Value::Bytes(b) => {
            serde_json::Value::String(b.iter().map(|byte| format!("{byte:02x}")).collect())
        }
        Value::Timestamp(_) => serde_json::Value::String(value_to_csv_field(v)),
        Value::Date(_) => serde_json::Value::String(value_to_csv_field(v)),
        Value::Uuid(_) => serde_json::Value::String(value_to_csv_field(v)),
        Value::Array(elems) => {
            serde_json::Value::Array(elems.iter().map(value_to_json_val).collect())
        }
    }
}

// ── Parquet writer ────────────────────────────────────────────────────────────

fn write_parquet(
    path: &str,
    col_names: &[String],
    rows: &[Vec<Value>],
    compression: Option<parquet::basic::Compression>,
) -> Result<QueryResult, DbError> {
    use parquet::basic::{Compression, ConvertedType, LogicalType, Repetition, Type as Physical};
    use parquet::column::writer::ColumnWriter;
    use parquet::data_type::{
        BoolType, ByteArray, ByteArrayType, DoubleType, FloatType, Int32Type, Int64Type,
    };
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::types::Type;
    use std::sync::Arc;

    let compression = compression.unwrap_or(Compression::UNCOMPRESSED);
    let count = rows.len() as u64;

    // ── Infer schema from first non-null row ─────────────────────────────────
    #[derive(Clone, Copy)]
    enum ColKind {
        Bool,
        Int32,
        Int64,
        Double,
        ByteArray,
    }
    let n_cols = col_names.len();
    let mut kinds: Vec<ColKind> = vec![ColKind::ByteArray; n_cols];
    'outer: for row in rows {
        for (i, val) in row.iter().enumerate() {
            if matches!(kinds[i], ColKind::ByteArray) && !matches!(val, Value::Null) {
                kinds[i] = match val {
                    Value::Bool(_) => ColKind::Bool,
                    Value::Int(_) => ColKind::Int32,
                    Value::BigInt(_) | Value::Timestamp(_) | Value::Decimal(_, _) => ColKind::Int64,
                    Value::Date(_) => ColKind::Int32,
                    Value::Real(_) => ColKind::Double,
                    _ => ColKind::ByteArray,
                };
            }
        }
        // If all columns have been resolved, stop scanning.
        if kinds.iter().all(|k| !matches!(k, ColKind::ByteArray)) {
            break 'outer;
        }
    }

    // ── Build Parquet schema ─────────────────────────────────────────────────
    let mut fields: Vec<Arc<Type>> = Vec::with_capacity(n_cols);
    for (i, name) in col_names.iter().enumerate() {
        let field = match kinds[i] {
            ColKind::Bool => Type::primitive_type_builder(name, Physical::BOOLEAN)
                .with_repetition(Repetition::OPTIONAL)
                .build(),
            ColKind::Int32 => Type::primitive_type_builder(name, Physical::INT32)
                .with_repetition(Repetition::OPTIONAL)
                .build(),
            ColKind::Int64 => Type::primitive_type_builder(name, Physical::INT64)
                .with_repetition(Repetition::OPTIONAL)
                .build(),
            ColKind::Double => Type::primitive_type_builder(name, Physical::DOUBLE)
                .with_repetition(Repetition::OPTIONAL)
                .build(),
            ColKind::ByteArray => Type::primitive_type_builder(name, Physical::BYTE_ARRAY)
                .with_repetition(Repetition::OPTIONAL)
                .with_converted_type(ConvertedType::UTF8)
                .build(),
        }
        .map_err(|e| DbError::Io(std::io::Error::other(format!("Parquet schema error: {e}"))))?;
        fields.push(Arc::new(field));
    }
    let schema = Arc::new(
        Type::group_type_builder("schema")
            .with_fields(fields)
            .build()
            .map_err(|e| {
                DbError::Io(std::io::Error::other(format!(
                    "Parquet root schema error: {e}"
                )))
            })?,
    );

    // ── Create writer ────────────────────────────────────────────────────────
    let file = std::fs::File::create(path).map_err(|e| {
        DbError::Io(std::io::Error::new(
            e.kind(),
            format!("COPY TO PARQUET: cannot create '{path}': {e}"),
        ))
    })?;
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(compression)
            .build(),
    );
    let mut file_writer = SerializedFileWriter::new(file, schema, props).map_err(|e| {
        DbError::Io(std::io::Error::other(format!(
            "COPY TO PARQUET: writer init error: {e}"
        )))
    })?;

    if !rows.is_empty() {
        let mut row_group = file_writer.next_row_group().map_err(|e| {
            DbError::Io(std::io::Error::other(format!(
                "COPY TO PARQUET: row group error: {e}"
            )))
        })?;

        for col_idx in 0..n_cols {
            let Some(mut scw) = row_group.next_column().map_err(|e| {
                DbError::Io(std::io::Error::other(format!(
                    "COPY TO PARQUET: column writer error col {col_idx}: {e}"
                )))
            })?
            else {
                break;
            };

            let pq_err = |e: parquet::errors::ParquetError| {
                DbError::Io(std::io::Error::other(format!(
                    "COPY TO PARQUET: write error col {col_idx}: {e}"
                )))
            };

            // def_level=1 → value present, def_level=0 → null.
            match kinds[col_idx] {
                ColKind::Bool => {
                    let mut vals: Vec<bool> = Vec::new();
                    let mut defs: Vec<i16> = Vec::new();
                    for row in rows {
                        match &row[col_idx] {
                            Value::Null => defs.push(0),
                            Value::Bool(b) => {
                                vals.push(*b);
                                defs.push(1);
                            }
                            other => {
                                vals.push(!matches!(other, Value::Int(0)));
                                defs.push(1);
                            }
                        }
                    }
                    if let ColumnWriter::BoolColumnWriter(ref mut w) = scw.untyped() {
                        w.write_batch(&vals, Some(&defs), None).map_err(pq_err)?;
                    }
                }
                ColKind::Int32 => {
                    let mut vals: Vec<i32> = Vec::new();
                    let mut defs: Vec<i16> = Vec::new();
                    for row in rows {
                        match &row[col_idx] {
                            Value::Null => defs.push(0),
                            Value::Int(n) => {
                                vals.push(*n);
                                defs.push(1);
                            }
                            Value::Date(d) => {
                                vals.push(*d);
                                defs.push(1);
                            }
                            Value::Bool(b) => {
                                vals.push(*b as i32);
                                defs.push(1);
                            }
                            _ => defs.push(0),
                        }
                    }
                    if let ColumnWriter::Int32ColumnWriter(ref mut w) = scw.untyped() {
                        w.write_batch(&vals, Some(&defs), None).map_err(pq_err)?;
                    }
                }
                ColKind::Int64 => {
                    let mut vals: Vec<i64> = Vec::new();
                    let mut defs: Vec<i16> = Vec::new();
                    for row in rows {
                        match &row[col_idx] {
                            Value::Null => defs.push(0),
                            Value::BigInt(n) => {
                                vals.push(*n);
                                defs.push(1);
                            }
                            Value::Timestamp(ts) => {
                                vals.push(*ts);
                                defs.push(1);
                            }
                            Value::Decimal(m, _) => {
                                vals.push(*m as i64);
                                defs.push(1);
                            }
                            Value::Int(n) => {
                                vals.push(*n as i64);
                                defs.push(1);
                            }
                            _ => defs.push(0),
                        }
                    }
                    if let ColumnWriter::Int64ColumnWriter(ref mut w) = scw.untyped() {
                        w.write_batch(&vals, Some(&defs), None).map_err(pq_err)?;
                    }
                }
                ColKind::Double => {
                    let mut vals: Vec<f64> = Vec::new();
                    let mut defs: Vec<i16> = Vec::new();
                    for row in rows {
                        match &row[col_idx] {
                            Value::Null => defs.push(0),
                            Value::Real(f) => {
                                vals.push(*f);
                                defs.push(1);
                            }
                            Value::Int(n) => {
                                vals.push(*n as f64);
                                defs.push(1);
                            }
                            Value::BigInt(n) => {
                                vals.push(*n as f64);
                                defs.push(1);
                            }
                            _ => defs.push(0),
                        }
                    }
                    if let ColumnWriter::DoubleColumnWriter(ref mut w) = scw.untyped() {
                        w.write_batch(&vals, Some(&defs), None).map_err(pq_err)?;
                    }
                }
                ColKind::ByteArray => {
                    let mut vals: Vec<ByteArray> = Vec::new();
                    let mut defs: Vec<i16> = Vec::new();
                    for row in rows {
                        match &row[col_idx] {
                            Value::Null => defs.push(0),
                            other => {
                                let s = value_to_csv_field(other);
                                vals.push(ByteArray::from(s.into_bytes()));
                                defs.push(1);
                            }
                        }
                    }
                    if let ColumnWriter::ByteArrayColumnWriter(ref mut w) = scw.untyped() {
                        w.write_batch(&vals, Some(&defs), None).map_err(pq_err)?;
                    }
                }
            }

            scw.close().map_err(|e| {
                DbError::Io(std::io::Error::other(format!(
                    "COPY TO PARQUET: close col {col_idx}: {e}"
                )))
            })?;
        }

        row_group.close().map_err(|e| {
            DbError::Io(std::io::Error::other(format!(
                "COPY TO PARQUET: row group close error: {e}"
            )))
        })?;
    }

    file_writer.close().map_err(|e| {
        DbError::Io(std::io::Error::other(format!(
            "COPY TO PARQUET: file close error: {e}"
        )))
    })?;

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}
