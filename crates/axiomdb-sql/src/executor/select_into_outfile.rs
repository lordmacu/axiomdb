use std::io::Write as IoWrite;

/// Write `rows` to the file described by `outfile` using MySQL INTO OUTFILE formatting.
/// Returns the number of rows written.
pub(crate) fn write_into_outfile(
    outfile: &IntoOutfile,
    rows: &[Vec<Value>],
) -> Result<u64, DbError> {
    let mut f = std::fs::File::create(&outfile.path).map_err(|e| {
        DbError::Io(std::io::Error::new(
            e.kind(),
            format!("INTO OUTFILE '{}': {e}", outfile.path),
        ))
    })?;

    for row in rows {
        let mut first = true;
        for val in row {
            if !first {
                IoWrite::write_fmt(&mut f, format_args!("{}", outfile.field_sep))
                    .map_err(io_err)?;
            }
            first = false;
            let s = outfile_field_str(val);
            if let Some(enc) = outfile.enclosure {
                let escaped = s.replace(enc, &format!("{enc}{enc}"));
                IoWrite::write_fmt(&mut f, format_args!("{enc}{escaped}{enc}"))
                    .map_err(io_err)?;
            } else {
                IoWrite::write_fmt(&mut f, format_args!("{s}")).map_err(io_err)?;
            }
        }
        IoWrite::write_fmt(&mut f, format_args!("{}", outfile.line_term)).map_err(io_err)?;
    }
    IoWrite::flush(&mut f).map_err(io_err)?;
    Ok(rows.len() as u64)
}

/// Post-process a SELECT result: if `into_outfile` is set, write the file and return
/// `Affected(N)`; otherwise forward the original result unchanged.
pub(crate) fn handle_into_outfile(
    result: Result<QueryResult, DbError>,
    into_outfile: Option<IntoOutfile>,
) -> Result<QueryResult, DbError> {
    let Some(outfile) = into_outfile else {
        return result;
    };
    match result? {
        QueryResult::Rows { rows, .. } => {
            let count = write_into_outfile(&outfile, &rows)?;
            Ok(QueryResult::Affected {
                count,
                last_insert_id: None,
            })
        }
        other => Ok(other),
    }
}

fn io_err(e: std::io::Error) -> DbError {
    DbError::Io(std::io::Error::new(
        e.kind(),
        format!("INTO OUTFILE write error: {e}"),
    ))
}

fn outfile_field_str(v: &Value) -> String {
    match v {
        Value::Null => r"\N".to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
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
                format!("{int_part}.{frac_part:0>width$}", width = scale as usize)
            }
        }
        Value::Text(s) => s.clone(),
        Value::Json(s) => s.clone(),
        Value::Jsonb(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Bytes(b) => b.iter().map(|byte| format!("{byte:02x}")).collect(),
        Value::Timestamp(ts) => {
            let secs = ts / 1_000_000;
            let us = (ts % 1_000_000).unsigned_abs();
            format!("{secs}.{us:06}")
        }
        Value::TimestampTz(ts) => {
            let secs = ts / 1_000_000;
            let us = (ts % 1_000_000).unsigned_abs();
            format!("{secs}.{us:06}+00")
        }
        Value::Date(days) => {
            use chrono::NaiveDate;
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
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
            let arr: Vec<serde_json::Value> = elems.iter().map(val_to_json).collect();
            serde_json::to_string(&arr).unwrap_or_default()
        }
        Value::Range(rv) => rv.to_display_string(),
        Value::Money(m, s, c) => Value::Money(*m, *s, *c).to_string(),
        Value::Composite(fields) => Value::Composite(fields.clone()).to_string(),
        Value::Ltree(s) | Value::Xml(s) => s.clone(),
    }
}

fn val_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::Value::Number((*n).into()),
        Value::BigInt(n) => serde_json::Value::Number((*n).into()),
        Value::Text(s) => serde_json::Value::String(s.clone()),
        other => serde_json::Value::String(outfile_field_str(other)),
    }
}
