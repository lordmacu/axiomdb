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

fn materialize_insert_row(col_positions: &[usize], provided: &[Value]) -> Vec<Value> {
    col_positions
        .iter()
        .map(|&idx| {
            if idx == usize::MAX {
                Value::Null
            } else {
                provided.get(idx).cloned().unwrap_or(Value::Null)
            }
        })
        .collect()
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

/// Checks that no `Text` value exceeds the declared `type_len` for its column.
///
/// Called on every INSERT and UPDATE row before it is written to storage.
/// Returns [`DbError::DataTooLong`] on the first violation found.
/// Columns with `type_len == 0` are unbounded and are skipped.
pub(crate) fn check_varchar_lengths(
    schema_cols: &[CatalogColumnDef],
    row_values: &[Value],
) -> Result<(), DbError> {
    for (col, val) in schema_cols.iter().zip(row_values.iter()) {
        if col.type_len == 0 {
            continue;
        }
        if col.col_type != ColumnType::Text {
            continue;
        }
        if let Value::Text(s) = val {
            let char_count = s.chars().count();
            if char_count > col.type_len as usize {
                return Err(DbError::DataTooLong {
                    column: col.name.clone(),
                    max_len: col.type_len,
                    actual_len: char_count,
                });
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
