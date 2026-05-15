// ── COPY FROM executor ────────────────────────────────────────────────────────
//
// Parses a server-side file (CSV, JSON, or JSONL) and bulk-inserts the rows
// into the target table by routing through execute_insert_ctx.
// Included directly into executor/mod.rs — all names from that module's scope
// are available here.

use std::io::BufRead;
use std::path::Path;

fn execute_copy_from(
    stmt: crate::ast::CopyFromStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    use crate::ast::{CopyFormat, InsertSource, InsertStmt, TableRef};

    let format = resolve_copy_format(&stmt.options, &stmt.path);
    let use_header = stmt.options.header.unwrap_or(format == CopyFormat::Csv);
    let delimiter = stmt.options.delimiter.unwrap_or(',');
    let null_str = stmt
        .options
        .null_str
        .as_deref()
        .unwrap_or(r"\N")
        .to_string();

    let file = std::fs::File::open(&stmt.path)
        .map_err(|e| DbError::Io(std::io::Error::new(e.kind(), format!("COPY FROM: cannot open '{}': {e}", stmt.path))))?;

    let (columns, value_rows) = match format {
        CopyFormat::Csv => parse_csv_file(file, use_header, delimiter, &null_str, &stmt.path)?,
        CopyFormat::Json => parse_json_file(file, &stmt.path)?,
        CopyFormat::Jsonl => parse_jsonl_file(file, &stmt.path)?,
        CopyFormat::Parquet => {
            return Err(DbError::NotImplemented {
                feature: "COPY FROM FORMAT PARQUET (use READ_PARQUET() TVF to read Parquet files)"
                    .into(),
            });
        }
    };

    if value_rows.is_empty() {
        return Ok(QueryResult::Affected {
            count: 0,
            last_insert_id: None,
        });
    }

    // Convert Vec<Vec<Value>> → Vec<Vec<Expr::Literal>> → InsertStmt.
    // execute_insert_ctx handles auto-increment, FK enforcement, and triggers.
    let expr_rows: Vec<Vec<Expr>> = value_rows
        .into_iter()
        .map(|row| row.into_iter().map(Expr::Literal).collect())
        .collect();

    let cols = if columns.is_empty() {
        None // HEADER FALSE — positional, let executor use schema order
    } else {
        Some(columns)
    };

    let insert = InsertStmt {
        table: TableRef::simple(stmt.table.clone()),
        columns: cols,
        source: InsertSource::Values(expr_rows),
        ignore: false,
        replace: false,
        returning: vec![],
        on_duplicate_update: None,
        on_conflict: None,
    };

    execute_insert_ctx(insert, exec_ctx, conn_txn, ctx)
}

// ── CSV ───────────────────────────────────────────────────────────────────────

fn parse_csv_file(
    file: std::fs::File,
    use_header: bool,
    delimiter: char,
    null_str: &str,
    path: &str,
) -> Result<(Vec<String>, Vec<Vec<axiomdb_types::Value>>), DbError> {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delimiter as u8)
        .has_headers(use_header)
        .flexible(false)
        .from_reader(file);

    if !use_header {
        return parse_csv_no_header(rdr, null_str, path);
    }

    let columns: Vec<String> = rdr
        .headers()
        .map_err(|e| DbError::InvalidValue {
            reason: format!("COPY FROM: {path}: CSV header error: {e}"),
        })?
        .iter()
        .map(|s| s.trim().to_string())
        .collect();

    let mut rows: Vec<Vec<axiomdb_types::Value>> = Vec::new();
    for (row_idx, result) in rdr.records().enumerate() {
        let record = result.map_err(|e| DbError::InvalidValue {
            reason: format!(
                "COPY FROM: {path}: line {}: CSV parse error: {e}",
                row_idx + 2
            ),
        })?;
        let row: Vec<axiomdb_types::Value> = record
            .iter()
            .map(|field| copy_csv_field_to_value(field, null_str))
            .collect();
        rows.push(row);
    }

    Ok((columns, rows))
}

fn parse_csv_no_header(
    mut rdr: csv::Reader<std::fs::File>,
    null_str: &str,
    path: &str,
) -> Result<(Vec<String>, Vec<Vec<axiomdb_types::Value>>), DbError> {
    let mut rows: Vec<Vec<axiomdb_types::Value>> = Vec::new();
    let mut col_count: Option<usize> = None;

    for (row_idx, result) in rdr.records().enumerate() {
        let record = result.map_err(|e| DbError::InvalidValue {
            reason: format!(
                "COPY FROM: {path}: line {}: CSV parse error: {e}",
                row_idx + 1
            ),
        })?;

        let n = record.len();
        match col_count {
            None => col_count = Some(n),
            Some(expected) if n != expected => {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "COPY FROM: {path}: line {}: expected {expected} columns, got {n}",
                        row_idx + 1
                    ),
                });
            }
            _ => {}
        }

        let row: Vec<axiomdb_types::Value> = record
            .iter()
            .map(|field| copy_csv_field_to_value(field, null_str))
            .collect();
        rows.push(row);
    }

    // Empty columns list signals schema-order (columns=None in InsertStmt).
    Ok((vec![], rows))
}

fn copy_csv_field_to_value(field: &str, null_str: &str) -> axiomdb_types::Value {
    if field == null_str {
        axiomdb_types::Value::Null
    } else {
        axiomdb_types::Value::Text(field.to_string())
    }
}

// ── JSON (array of objects) ───────────────────────────────────────────────────

fn parse_json_file(
    file: std::fs::File,
    path: &str,
) -> Result<(Vec<String>, Vec<Vec<axiomdb_types::Value>>), DbError> {
    let reader = std::io::BufReader::new(file);
    let root: serde_json::Value =
        serde_json::from_reader(reader).map_err(|e| DbError::InvalidValue {
            reason: format!("COPY FROM: {path}: JSON parse error: {e}"),
        })?;

    let arr = root.as_array().ok_or_else(|| DbError::InvalidValue {
        reason: format!("COPY FROM: {path}: JSON root must be an array"),
    })?;

    if arr.is_empty() {
        return Ok((vec![], vec![]));
    }

    let mut columns: Vec<String> = Vec::new();
    let mut col_index: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();

    for item in arr {
        let obj = item.as_object().ok_or_else(|| DbError::InvalidValue {
            reason: format!("COPY FROM: {path}: each JSON element must be an object"),
        })?;
        for key in obj.keys() {
            let lc = key.to_ascii_lowercase();
            if !col_index.contains_key(&lc) {
                col_index.insert(lc.clone(), columns.len());
                columns.push(lc);
            }
        }
    }

    let mut rows: Vec<Vec<axiomdb_types::Value>> = Vec::with_capacity(arr.len());
    for item in arr {
        let obj = item.as_object().unwrap();
        let mut row = vec![axiomdb_types::Value::Null; columns.len()];
        for (key, val) in obj {
            let lc = key.to_ascii_lowercase();
            if let Some(&idx) = col_index.get(&lc) {
                row[idx] = copy_json_to_value(val);
            }
        }
        rows.push(row);
    }

    Ok((columns, rows))
}

// ── JSONL (one JSON object per line) ─────────────────────────────────────────

fn parse_jsonl_file(
    file: std::fs::File,
    path: &str,
) -> Result<(Vec<String>, Vec<Vec<axiomdb_types::Value>>), DbError> {
    let reader = std::io::BufReader::new(file);

    let mut columns: Vec<String> = Vec::new();
    let mut col_index: hashbrown::HashMap<String, usize> = hashbrown::HashMap::new();
    let mut raw_rows: Vec<Vec<(String, serde_json::Value)>> = Vec::new();

    for (line_idx, line_result) in reader.lines().enumerate() {
        let line = line_result.map_err(DbError::Io)?;
        let line = line.trim();
        if line.is_empty() {
            continue; // skip blank lines
        }
        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).map_err(|e| DbError::InvalidValue {
                reason: format!(
                    "COPY FROM: {path}: line {}: JSON parse error: {e}",
                    line_idx + 1
                ),
            })?;

        let mut row_pairs: Vec<(String, serde_json::Value)> = Vec::new();
        for (key, val) in obj {
            let lc = key.to_ascii_lowercase();
            if !col_index.contains_key(&lc) {
                col_index.insert(lc.clone(), columns.len());
                columns.push(lc.clone());
            }
            row_pairs.push((lc, val));
        }
        raw_rows.push(row_pairs);
    }

    if raw_rows.is_empty() {
        return Ok((vec![], vec![]));
    }

    let mut rows: Vec<Vec<axiomdb_types::Value>> = Vec::with_capacity(raw_rows.len());
    for pairs in raw_rows {
        let mut row = vec![axiomdb_types::Value::Null; columns.len()];
        for (key, val) in pairs {
            if let Some(&idx) = col_index.get(&key) {
                row[idx] = copy_json_to_value(&val);
            }
        }
        rows.push(row);
    }

    Ok((columns, rows))
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Resolve the copy format from options or file extension.
fn resolve_copy_format(opts: &crate::ast::CopyOptions, path: &str) -> crate::ast::CopyFormat {
    use crate::ast::CopyFormat;
    if let Some(ref f) = opts.format {
        return f.clone();
    }
    match Path::new(path).extension().and_then(|s| s.to_str()) {
        Some("csv") => CopyFormat::Csv,
        Some("json") => CopyFormat::Json,
        Some("jsonl") | Some("ndjson") => CopyFormat::Jsonl,
        Some("parquet") => CopyFormat::Parquet,
        _ => CopyFormat::Csv,
    }
}

fn copy_json_to_value(v: &serde_json::Value) -> axiomdb_types::Value {
    match v {
        serde_json::Value::Null => axiomdb_types::Value::Null,
        serde_json::Value::Bool(b) => axiomdb_types::Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    axiomdb_types::Value::Int(i as i32)
                } else {
                    axiomdb_types::Value::BigInt(i)
                }
            } else if let Some(f) = n.as_f64() {
                axiomdb_types::Value::Real(f)
            } else {
                axiomdb_types::Value::Text(n.to_string())
            }
        }
        serde_json::Value::String(s) => axiomdb_types::Value::Text(s.clone()),
        // Arrays and objects serialized back to JSON string for ARRAY/JSON columns.
        other => axiomdb_types::Value::Text(other.to_string()),
    }
}
