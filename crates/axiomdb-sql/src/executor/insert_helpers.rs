fn maintain_clustered_secondary_inserts(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &mut crate::bloom::BloomRegistry,
    secondary_indexes: &mut [IndexDef],
    secondary_layouts: &[crate::clustered_secondary::ClusteredSecondaryLayout],
    compiled_preds: &[Option<Expr>],
    row_values: &[Value],
    debug_clustered_insert: bool,
) -> Result<(std::time::Duration, std::time::Duration), DbError> {
    use std::time::{Duration, Instant};

    let mut secondary_time = Duration::ZERO;
    let mut root_persist_time = Duration::ZERO;

    for ((idx, layout), compiled_pred) in secondary_indexes
        .iter_mut()
        .zip(secondary_layouts.iter())
        .zip(compiled_preds.iter())
    {
        let secondary_started = debug_clustered_insert.then(Instant::now);
        if let Some(pred) = compiled_pred {
            if !is_truthy(&eval(pred, row_values)?) {
                continue;
            }
        }

        let Some(entry) = layout.entry_from_row(row_values)? else {
            continue;
        };

        let root_pid = std::sync::atomic::AtomicU64::new(idx.root_page_id);
        layout.insert_row(storage, &root_pid, row_values)?;
        bloom.add(idx.index_id, &entry.physical_key);
        let new_index_root = root_pid.load(std::sync::atomic::Ordering::Acquire);
        txn.record_index_insert(conn_txn, idx.index_id, new_index_root, entry.physical_key);
        if let Some(started) = secondary_started {
            secondary_time += started.elapsed();
        }
        if new_index_root != idx.root_page_id {
            let persist_started = debug_clustered_insert.then(Instant::now);
            CatalogWriter::new(storage, txn, conn_txn)?.update_index_root(idx.index_id, new_index_root)?;
            if let Some(started) = persist_started {
                root_persist_time += started.elapsed();
            }
            idx.root_page_id = new_index_root;
        }
    }

    Ok((secondary_time, root_persist_time))
}

fn build_insert_column_positions(
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    insert_columns: &Option<Vec<String>>,
    table_name: &str,
) -> Result<Vec<usize>, DbError> {
    match insert_columns {
        None => Ok((0..schema_cols.len()).collect()),
        Some(named_cols) => {
            let mut map = vec![usize::MAX; schema_cols.len()];
            for (val_pos, col_name) in named_cols.iter().enumerate() {
                let schema_pos = schema_cols
                    .iter()
                    .position(|c| &c.name == col_name)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: table_name.to_string(),
                    })?;
                map[schema_pos] = val_pos;
            }
            Ok(map)
        }
    }
}

fn materialize_insert_row(
    col_positions: &[usize],
    provided: &[Value],
    schema_cols: &[CatalogColumnDef],
) -> Vec<Value> {
    col_positions
        .iter()
        .enumerate()
        .map(|(col_idx, &val_idx)| {
            if val_idx == usize::MAX {
                // Missing column — use the stored DEFAULT expression if available.
                eval_default_for_column(schema_cols.get(col_idx))
            } else {
                provided.get(val_idx).cloned().unwrap_or(Value::Null)
            }
        })
        .collect()
}

/// Evaluates the stored DEFAULT expression for a column, returning `Value::Null`
/// if no default is declared.
fn eval_default_for_column(col: Option<&CatalogColumnDef>) -> Value {
    let col = match col {
        Some(c) => c,
        None => return Value::Null,
    };
    let expr_str = match &col.default_expr {
        Some(s) => s,
        None => return Value::Null,
    };
    match crate::parser::parse_expr_only(expr_str) {
        Ok(expr) => eval(&expr, &[]).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// Resolves `Expr::Default` tokens in a row of provided value expressions.
///
/// `col_positions[schema_idx] == val_idx` maps schema columns to provided values.
/// When a provided value was `Expr::Default`, replaces it with the schema
/// column's stored default expression result.
pub(crate) fn resolve_expr_defaults(
    col_positions: &[usize],
    exprs: &[Expr],
    provided: &mut [Value],
    schema_cols: &[CatalogColumnDef],
) {
    for (schema_idx, &val_idx) in col_positions.iter().enumerate() {
        if val_idx == usize::MAX || val_idx >= exprs.len() {
            continue;
        }
        if matches!(&exprs[val_idx], Expr::Default) {
            provided[val_idx] = eval_default_for_column(schema_cols.get(schema_idx));
        }
    }
}

fn assign_auto_increment(
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    values: &mut [Value],
    first_generated: &mut Option<u64>,
) -> Result<(), DbError> {
    let Some(ai_col) = schema_cols.iter().position(|c| c.auto_increment) else {
        return Ok(());
    };
    // MySQL: explicit 0 on an AUTO_INCREMENT column triggers sequence assignment.
    let is_auto_trigger = matches!(
        values.get(ai_col),
        Some(Value::Null) | Some(Value::Int(0)) | Some(Value::BigInt(0))
    );
    if !is_auto_trigger {
        return Ok(());
    }

    let table_id = table_def.id;
    let cached = AUTO_INC_SEQ.with(|seq| seq.borrow().get(&table_id).copied());
    let next = if let Some(next) = cached {
        next
    } else {
        let snap = txn.active_snapshot(conn_txn);
        let max_existing = if table_def.is_clustered() {
            crate::clustered_table::scan_max_numeric_column(
                storage,
                txn.clustered_root(table_id)
                    .or(Some(table_def.root_page_id)),
                schema_cols,
                ai_col,
                &snap,
            )?
        } else {
            let rows = TableEngine::scan_table(storage, table_def, schema_cols, snap, None)?;
            rows.iter()
                .filter_map(|(_, vals)| vals.get(ai_col))
                .filter_map(|v| match v {
                    Value::Int(n) => Some(*n as u64),
                    Value::BigInt(n) => Some(*n as u64),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
        };
        max_existing + 1
    };

    AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
    values[ai_col] = match schema_cols[ai_col].col_type {
        axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(next as i64),
        _ => Value::Int(next as i32),
    };
    if first_generated.is_none() {
        *first_generated = Some(next);
    }
    Ok(())
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

/// Enforces text column constraints: VARCHAR(N) length validation and CHAR(N)
/// right-padding with spaces.
///
/// Called on every INSERT and UPDATE row before it is written to storage.
///
/// - **VARCHAR(N)**: rejects values longer than N characters with
///   [`DbError::DataTooLong`].
/// - **CHAR(N)**: right-pads values shorter than N with spaces; rejects values
///   longer than N (after stripping trailing spaces, per MySQL behavior).
/// - Columns with `type_len == 0` are unbounded (`TEXT`) and are skipped.
pub(crate) fn enforce_text_constraints(
    schema_cols: &[CatalogColumnDef],
    row_values: &mut [Value],
) -> Result<(), DbError> {
    for (col, val) in schema_cols.iter().zip(row_values.iter_mut()) {
        if col.type_len == 0 {
            continue;
        }
        if col.col_type != ColumnType::Text {
            continue;
        }
        let max_len = col.type_len as usize;
        if let Value::Text(s) = val {
            if col.is_fixed_len {
                // CHAR(N): strip trailing spaces first (MySQL behavior), then
                // check length, then pad to exactly N characters.
                let trimmed = s.trim_end_matches(' ');
                let char_count = trimmed.chars().count();
                if char_count > max_len {
                    return Err(DbError::DataTooLong {
                        column: col.name.clone(),
                        max_len: col.type_len,
                        actual_len: char_count,
                    });
                }
                if char_count < max_len {
                    let mut padded = String::with_capacity(trimmed.len() + (max_len - char_count));
                    padded.push_str(trimmed);
                    for _ in 0..(max_len - char_count) {
                        padded.push(' ');
                    }
                    *s = padded;
                } else if trimmed.len() != s.len() {
                    // Same char count but had trailing spaces — normalize.
                    *s = trimmed.to_string();
                }
            } else {
                // VARCHAR(N): reject if too long.
                let char_count = s.chars().count();
                if char_count > max_len {
                    return Err(DbError::DataTooLong {
                        column: col.name.clone(),
                        max_len: col.type_len,
                        actual_len: char_count,
                    });
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn check_row_constraints(
    constraints: &[axiomdb_catalog::schema::ConstraintDef],
    row_values: &[Value],
    table_name: &str,
) -> Result<(), DbError> {
    for c in constraints {
        if c.check_expr.is_empty() {
            continue;
        }
        let expr = match crate::parser::parse_expr_only(&c.check_expr) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let result = eval(&expr, row_values)?;
        if !crate::eval::is_truthy(&result) {
            return Err(DbError::CheckViolation {
                table: table_name.to_string(),
                constraint: c.name.clone(),
            });
        }
    }
    Ok(())
}

// ── ALTER TABLE constraint helpers (Phase 4.22b) ──────────────────────────────
