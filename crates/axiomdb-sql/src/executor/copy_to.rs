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
