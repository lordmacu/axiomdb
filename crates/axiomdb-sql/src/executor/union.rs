// ── UNION / UNION ALL executor ───────────────────────────────────────────────

/// Executes `SELECT ... UNION [ALL] SELECT ...` chains.
///
/// For UNION ALL: concatenates all result sets (keeps duplicates).
/// For UNION: concatenates then deduplicates using hash-based dedup.
///
/// Column metadata is taken from the first SELECT. All SELECTs must produce
/// the same number of columns (MySQL behavior: types are coerced from the
/// first query's column types).
fn execute_union(
    selects: Vec<SelectStmt>,
    all: bool,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    if selects.is_empty() {
        return Ok(QueryResult::affected(0));
    }

    // Execute each SELECT and collect rows.
    let mut columns: Option<Vec<ColumnMeta>> = None;
    let mut all_rows: Vec<Row> = Vec::new();
    let mut expected_width: Option<usize> = None;

    for select in selects {
        let result = execute_select_ctx(select, exec_ctx, conn_txn, ctx)?;
        match result {
            QueryResult::Rows { columns: cols, rows } => {
                // Validate column count consistency.
                match expected_width {
                    None => {
                        expected_width = Some(cols.len());
                        columns = Some(cols);
                    }
                    Some(w) if cols.len() != w => {
                        return Err(DbError::ParseError {
                            message: format!(
                                "UNION queries must have the same number of columns \
                                 (first SELECT has {w}, this one has {})",
                                cols.len()
                            ),
                            position: None,
                        });
                    }
                    _ => {} // same width, discard cols (use first SELECT's metadata)
                }
                all_rows.extend(rows);
            }
            // Non-row results (shouldn't happen for SELECT, but handle gracefully)
            other => return Ok(other),
        }
    }

    let columns = columns.unwrap_or_default();

    // For UNION ALL: just return all rows.
    if all {
        return Ok(QueryResult::Rows {
            columns,
            rows: all_rows,
        });
    }

    // For UNION (distinct): deduplicate using a HashSet on serialized row bytes.
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::with_capacity(all_rows.len());
    for row in all_rows {
        let key = row_to_dedup_key(&row);
        if seen.insert(key) {
            deduped.push(row);
        }
    }

    Ok(QueryResult::Rows {
        columns,
        rows: deduped,
    })
}

/// Creates a deduplication key from a row by serializing all values.
///
/// Uses a simple but correct serialization: each value is converted to its
/// debug string representation. This is not the most efficient approach but
/// is correct for all value types including NULL.
fn row_to_dedup_key(row: &Row) -> Vec<u8> {
    let mut key = Vec::new();
    for value in row.iter() {
        // Type tag to distinguish Int(0) from BigInt(0) from Text("0")
        match value {
            Value::Null => key.push(0),
            Value::Bool(b) => {
                key.push(1);
                key.push(*b as u8);
            }
            Value::Int(n) => {
                key.push(2);
                key.extend_from_slice(&n.to_le_bytes());
            }
            Value::BigInt(n) => {
                key.push(3);
                key.extend_from_slice(&n.to_le_bytes());
            }
            Value::Real(f) => {
                key.push(4);
                key.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            Value::Decimal(m, s) => {
                key.push(5);
                key.extend_from_slice(&m.to_le_bytes());
                key.push(*s);
            }
            Value::Date(d) => {
                key.push(6);
                key.extend_from_slice(&d.to_le_bytes());
            }
            Value::Timestamp(t) => {
                key.push(7);
                key.extend_from_slice(&t.to_le_bytes());
            }
            Value::Text(s) | Value::Json(s) => {
                key.push(8);
                key.extend_from_slice(s.as_bytes());
                key.push(0); // terminator
            }
            Value::Bytes(b) => {
                key.push(9);
                key.extend_from_slice(b);
                key.push(0);
            }
            Value::Uuid(u) => {
                key.push(10);
                key.extend_from_slice(u);
            }
            Value::Jsonb(b) => {
                key.push(11);
                key.extend_from_slice(b);
                key.push(0);
            }
        }
    }
    key
}
