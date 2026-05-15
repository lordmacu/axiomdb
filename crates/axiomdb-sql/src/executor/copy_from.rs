// ── COPY FROM executor ────────────────────────────────────────────────────────
//
// Parses a server-side file (CSV, JSON, or JSONL) and bulk-inserts the rows
// into the target table by routing through execute_insert_ctx.
// Included directly into executor/mod.rs — all names from that module's scope
// are available here.
//
// Phase 20.8: CSV and JSONL are streamed in batches of COPY_BATCH_SIZE rows so
// that files larger than available RAM can be imported without OOM.
// JSON array format still loads the full file (JSON arrays are not streamable
// without a full parse tree — use JSONL for files that exceed RAM).

use std::io::BufRead;
use std::path::Path;

const COPY_BATCH_SIZE: usize = 1024;

fn execute_copy_from(
    stmt: crate::ast::CopyFromStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    use crate::ast::CopyFormat;

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

    let total = match format {
        CopyFormat::Csv => stream_copy_csv(
            file, use_header, delimiter, &null_str, &stmt.path, &stmt.table,
            exec_ctx, conn_txn, ctx,
        )?,
        CopyFormat::Jsonl => stream_copy_jsonl(
            file, &stmt.path, &stmt.table,
            exec_ctx, conn_txn, ctx,
        )?,
        CopyFormat::Json => {
            let (columns, value_rows) = parse_json_file(file, &stmt.path)?;
            if value_rows.is_empty() {
                return Ok(QueryResult::Affected { count: 0, last_insert_id: None });
            }
            flush_batch_owned(value_rows, columns, &stmt.table, exec_ctx, conn_txn, ctx)?
        }
        CopyFormat::Parquet => {
            return Err(DbError::NotImplemented {
                feature: "COPY FROM FORMAT PARQUET (use READ_PARQUET() TVF to read Parquet files)"
                    .into(),
            });
        }
    };

    Ok(QueryResult::Affected { count: total, last_insert_id: None })
}

// ── CSV streaming ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn stream_copy_csv(
    file: std::fs::File,
    use_header: bool,
    delimiter: char,
    null_str: &str,
    path: &str,
    table: &str,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<u64, DbError> {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delimiter as u8)
        .has_headers(use_header)
        .flexible(false)
        .from_reader(file);

    let columns: Option<Vec<String>> = if use_header {
        Some(
            rdr.headers()
                .map_err(|e| DbError::InvalidValue {
                    reason: format!("COPY FROM: {path}: CSV header error: {e}"),
                })?
                .iter()
                .map(|s| s.trim().to_string())
                .collect(),
        )
    } else {
        None
    };

    let mut batch: Vec<Vec<axiomdb_types::Value>> = Vec::with_capacity(COPY_BATCH_SIZE);
    let mut total: u64 = 0;
    let mut col_count: Option<usize> = None;
    let base_line = if use_header { 2usize } else { 1usize };

    for (row_idx, result) in rdr.records().enumerate() {
        let record = result.map_err(|e| DbError::InvalidValue {
            reason: format!(
                "COPY FROM: {path}: line {}: CSV parse error: {e}",
                base_line + row_idx
            ),
        })?;

        let n = record.len();
        if use_header {
            // flexible=false already enforces column count for header mode
        } else {
            match col_count {
                None => col_count = Some(n),
                Some(expected) if n != expected => {
                    return Err(DbError::InvalidValue {
                        reason: format!(
                            "COPY FROM: {path}: line {}: expected {expected} columns, got {n}",
                            base_line + row_idx
                        ),
                    });
                }
                _ => {}
            }
        }

        let row: Vec<axiomdb_types::Value> = record
            .iter()
            .map(|field| copy_csv_field_to_value(field, null_str))
            .collect();
        batch.push(row);

        if batch.len() == COPY_BATCH_SIZE {
            total += flush_batch(&mut batch, columns.clone(), table, exec_ctx, conn_txn, ctx)?;
        }
    }
    if !batch.is_empty() {
        total += flush_batch(&mut batch, columns, table, exec_ctx, conn_txn, ctx)?;
    }
    Ok(total)
}

fn copy_csv_field_to_value(field: &str, null_str: &str) -> axiomdb_types::Value {
    if field == null_str {
        axiomdb_types::Value::Null
    } else {
        axiomdb_types::Value::Text(field.to_string())
    }
}

// ── JSONL streaming (schema-first) ────────────────────────────────────────────

fn stream_copy_jsonl(
    file: std::fs::File,
    path: &str,
    table: &str,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<u64, DbError> {
    use crate::ast::TableRef;

    // Resolve table schema once — col_index maps lowercase column name → position.
    let resolved = resolve_table_cached(
        exec_ctx.storage(),
        exec_ctx.coord(),
        ctx,
        Some(conn_txn),
        &TableRef::simple(table.to_string()),
    )?;
    let col_count = resolved.columns.len();
    let column_names: Vec<String> = resolved.columns.iter().map(|c| c.name.clone()).collect();
    let col_index: hashbrown::HashMap<String, usize> = column_names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect();

    let reader = std::io::BufReader::new(file);
    let mut batch: Vec<Vec<axiomdb_types::Value>> = Vec::with_capacity(COPY_BATCH_SIZE);
    let mut total: u64 = 0;

    for (line_idx, line_result) in reader.lines().enumerate() {
        let line = line_result.map_err(DbError::Io)?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).map_err(|e| DbError::InvalidValue {
                reason: format!(
                    "COPY FROM: {path}: line {}: JSON parse error: {e}",
                    line_idx + 1
                ),
            })?;

        let mut row = vec![axiomdb_types::Value::Null; col_count];
        for (key, val) in &obj {
            let lc = key.to_ascii_lowercase();
            if let Some(&idx) = col_index.get(&lc) {
                row[idx] = copy_json_to_value(val);
            }
            // unknown keys silently ignored
        }
        batch.push(row);

        if batch.len() == COPY_BATCH_SIZE {
            total += flush_batch(
                &mut batch,
                Some(column_names.clone()),
                table,
                exec_ctx,
                conn_txn,
                ctx,
            )?;
        }
    }
    if !batch.is_empty() {
        total += flush_batch(&mut batch, Some(column_names), table, exec_ctx, conn_txn, ctx)?;
    }
    Ok(total)
}

// ── JSON (array of objects) — full load ───────────────────────────────────────

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

// ── Batch insert helpers ──────────────────────────────────────────────────────

/// Drains `batch` into an `execute_insert_ctx` call; returns the affected count.
/// `batch` is cleared after a successful flush.
fn flush_batch(
    batch: &mut Vec<Vec<axiomdb_types::Value>>,
    columns: Option<Vec<String>>,
    table: &str,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<u64, DbError> {
    let rows = std::mem::take(batch);
    flush_batch_owned(rows, columns.unwrap_or_default(), table, exec_ctx, conn_txn, ctx)
}

fn flush_batch_owned(
    rows: Vec<Vec<axiomdb_types::Value>>,
    columns: Vec<String>,
    table: &str,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<u64, DbError> {
    use crate::ast::{InsertSource, InsertStmt, TableRef};
    if rows.is_empty() {
        return Ok(0);
    }
    let expr_rows: Vec<Vec<Expr>> = rows
        .into_iter()
        .map(|row| row.into_iter().map(Expr::Literal).collect())
        .collect();
    let cols = if columns.is_empty() { None } else { Some(columns) };
    let insert = InsertStmt {
        table: TableRef::simple(table.to_string()),
        columns: cols,
        source: InsertSource::Values(expr_rows),
        ignore: false,
        replace: false,
        returning: vec![],
        on_duplicate_update: None,
        on_conflict: None,
    };
    match execute_insert_ctx(insert, exec_ctx, conn_txn, ctx)? {
        QueryResult::Affected { count, .. } => Ok(count),
        _ => Ok(0),
    }
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
