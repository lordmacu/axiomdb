/// Attack 10: caller-facing variant that accumulates root changes
/// in-place (via `idx.root_page_id`) without writing to the catalog.
/// Returns `(secondary_elapsed, catalog_elapsed=0)`. The caller must
/// invoke `flush_deferred_secondary_index_roots` after the batch.
///
/// Attack 11: `skip_mask`, if non-empty, skips secondary indexes
/// where `skip_mask[i] == true`. The caller is responsible for
/// handling those indexes separately (typically: bulk-build via
/// `BTree::bulk_load_sorted`). Pass an empty slice to maintain all
/// indexes.
#[expect(clippy::too_many_arguments, reason = "see maintain_clustered_secondary_inserts")]
pub(crate) fn maintain_clustered_secondary_inserts_deferred(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    table_root_page_id: u64,
    secondary_indexes: &mut [IndexDef],
    secondary_layouts: &[crate::clustered_secondary::ClusteredSecondaryLayout],
    compiled_preds: &[Option<Expr>],
    row_values: &[Value],
    debug_clustered_insert: bool,
    skip_mask: &[bool],
) -> Result<std::time::Duration, DbError> {
    let (secondary_elapsed, _) = maintain_clustered_secondary_inserts_impl(
        storage,
        txn,
        conn_txn,
        bloom,
        table_root_page_id,
        secondary_indexes,
        secondary_layouts,
        compiled_preds,
        row_values,
        debug_clustered_insert,
        /*defer_root_persist=*/ true,
        skip_mask,
    )?;
    Ok(secondary_elapsed)
}

/// Persists any pending secondary-index root changes accumulated by
/// `maintain_clustered_secondary_inserts_deferred`. Call once at the
/// end of a batch. The `original_roots` slice MUST be the same shape
/// as `secondary_indexes` and hold the roots BEFORE the batch started.
pub(crate) fn flush_deferred_secondary_index_roots(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    secondary_indexes: &[IndexDef],
    original_roots: &[u64],
) -> Result<(), DbError> {
    debug_assert_eq!(secondary_indexes.len(), original_roots.len());
    let mut writer: Option<axiomdb_catalog::writer::CatalogWriter<'_>> = None;
    for (idx, &orig_root) in secondary_indexes.iter().zip(original_roots.iter()) {
        if idx.root_page_id != orig_root {
            let w = match writer.as_mut() {
                Some(w) => w,
                None => {
                    writer = Some(axiomdb_catalog::writer::CatalogWriter::new(
                        storage, txn, conn_txn,
                    )?);
                    writer.as_mut().unwrap()
                }
            };
            w.update_index_root(idx.index_id, idx.root_page_id)?;
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments, reason = "see maintain_clustered_secondary_inserts")]
fn maintain_clustered_secondary_inserts_impl(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    table_root_page_id: u64,
    secondary_indexes: &mut [IndexDef],
    secondary_layouts: &[crate::clustered_secondary::ClusteredSecondaryLayout],
    compiled_preds: &[Option<Expr>],
    row_values: &[Value],
    debug_clustered_insert: bool,
    defer_root_persist: bool,
    skip_mask: &[bool],
) -> Result<(std::time::Duration, std::time::Duration), DbError> {
    use std::time::{Duration, Instant};

    let mut secondary_time = Duration::ZERO;
    let mut root_persist_time = Duration::ZERO;

    for (i, ((idx, layout), compiled_pred)) in secondary_indexes
        .iter_mut()
        .zip(secondary_layouts.iter())
        .zip(compiled_preds.iter())
        .enumerate()
    {
        // Attack 11: caller-controlled skip for indexes that go
        // through bulk_load_sorted instead of per-row insert.
        if skip_mask.get(i).copied().unwrap_or(false) {
            continue;
        }
        let secondary_started = debug_clustered_insert.then(Instant::now);
        if let Some(pred) = compiled_pred {
            if !is_truthy(&eval(pred, row_values)?) {
                continue;
            }
        }

        if idx.index_type == 4 {
            let Some(terms) = crate::index_maintenance::gin_terms_for_row_value(
                row_values.get(idx.columns[0].col_idx as usize),
            )?
            else {
                continue;
            };
            let pk_key = crate::index_maintenance::encode_clustered_pk_key_from_row(
                &idx.name,
                &layout.primary_cols,
                row_values,
            )?;
            let root_pid = std::sync::atomic::AtomicU64::new(idx.root_page_id);
            for term in &terms {
                let key = crate::index_maintenance::gin_clustered_key(term, &pk_key);
                // Clear any stale entry left by a previous MVCC-deferred delete at
                // this same PK. DELETE leaves clustered secondary entries in place
                // for VACUUM (see execute_clustered_delete), and the clustered GIN
                // key [term][0x00][pk_key] is identical across the dead and the
                // re-inserted row, so insert_in would otherwise hit the leftover
                // entry and return DuplicateKey. Mirrors the regular clustered
                // secondary path (ClusteredSecondaryLayout::insert_row_maybe_visible).
                let _ = BTree::delete_in(storage, &root_pid, &key)?;
                BTree::insert_in(
                    storage,
                    &root_pid,
                    &key,
                    crate::index_maintenance::GIN_CLUSTERED_DUMMY_RID,
                    idx.fillfactor,
                )?;
                txn.record_index_insert(
                    conn_txn,
                    idx.index_id,
                    root_pid.load(std::sync::atomic::Ordering::Acquire),
                    key,
                );
            }
            let new_index_root = root_pid.load(std::sync::atomic::Ordering::Acquire);
            if let Some(started) = secondary_started {
                secondary_time += started.elapsed();
            }
            if new_index_root != idx.root_page_id {
                if defer_root_persist {
                    // Attack 10: just track the new root in-memory;
                    // caller flushes via flush_deferred_secondary_index_roots.
                    idx.root_page_id = new_index_root;
                } else {
                    let persist_started = debug_clustered_insert.then(Instant::now);
                    CatalogWriter::new(storage, txn, conn_txn)?
                        .update_index_root(idx.index_id, new_index_root)?;
                    if let Some(started) = persist_started {
                        root_persist_time += started.elapsed();
                    }
                    idx.root_page_id = new_index_root;
                }
            }
            continue;
        }

        let Some(entry) = layout.entry_from_row(row_values)? else {
            continue;
        };

        let root_pid = std::sync::atomic::AtomicU64::new(idx.root_page_id);
        let snap = txn.active_snapshot(conn_txn);
        layout.insert_row_visible(storage, &root_pid, table_root_page_id, &snap, row_values)?;
        bloom.add(idx.index_id, &entry.physical_key);
        let new_index_root = root_pid.load(std::sync::atomic::Ordering::Acquire);
        txn.record_index_insert(conn_txn, idx.index_id, new_index_root, entry.physical_key);
        if let Some(started) = secondary_started {
            secondary_time += started.elapsed();
        }
        if new_index_root != idx.root_page_id {
            if defer_root_persist {
                idx.root_page_id = new_index_root;
            } else {
                let persist_started = debug_clustered_insert.then(Instant::now);
                CatalogWriter::new(storage, txn, conn_txn)?
                    .update_index_root(idx.index_id, new_index_root)?;
                if let Some(started) = persist_started {
                    root_persist_time += started.elapsed();
                }
                idx.root_page_id = new_index_root;
            }
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

pub(crate) fn validate_generated_insert_exprs(
    col_positions: &[usize],
    exprs: &[Expr],
    schema_cols: &[CatalogColumnDef],
    table_name: &str,
) -> Result<(), DbError> {
    for (schema_idx, col) in schema_cols.iter().enumerate() {
        if col.generated_expr.is_none() {
            continue;
        }
        let Some(&val_idx) = col_positions.get(schema_idx) else {
            continue;
        };
        if val_idx == usize::MAX || val_idx >= exprs.len() {
            continue;
        }
        if !matches!(exprs[val_idx], Expr::Default) {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "generated column '{}.{}' cannot be assigned explicitly",
                    table_name, col.name
                ),
            });
        }
    }
    Ok(())
}

pub(crate) fn validate_generated_insert_source_values(
    col_positions: &[usize],
    provided_len: usize,
    schema_cols: &[CatalogColumnDef],
    table_name: &str,
) -> Result<(), DbError> {
    for (schema_idx, col) in schema_cols.iter().enumerate() {
        if col.generated_expr.is_none() {
            continue;
        }
        let Some(&val_idx) = col_positions.get(schema_idx) else {
            continue;
        };
        if val_idx != usize::MAX && val_idx < provided_len {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "generated column '{}.{}' cannot be assigned explicitly",
                    table_name, col.name
                ),
            });
        }
    }
    Ok(())
}

pub fn materialize_generated_columns(
    schema_cols: &[CatalogColumnDef],
    row_values: &mut [Value],
) -> Result<(), DbError> {
    for (idx, col) in schema_cols.iter().enumerate() {
        let Some(expr_sql) = col.generated_expr.as_ref() else {
            continue;
        };
        if !col.generated_stored {
            return Err(DbError::NotImplemented {
                feature: "virtual generated columns".into(),
            });
        }
        let mut expr = crate::parser::parse_expr_only(expr_sql)?;
        resolve_column_refs(&mut expr, schema_cols);
        let value = eval(&expr, row_values)?;
        if let Some(slot) = row_values.get_mut(idx) {
            *slot = value;
        }
    }
    Ok(())
}

pub(crate) fn validate_generated_update_assignments(
    assignments: &[(usize, Expr)],
    schema_cols: &[CatalogColumnDef],
    table_name: &str,
) -> Result<(), DbError> {
    for (col_pos, expr) in assignments {
        let Some(col) = schema_cols.get(*col_pos) else {
            continue;
        };
        if col.generated_expr.is_none() {
            continue;
        }
        if !matches!(expr, Expr::Default) {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "generated column '{}.{}' cannot be assigned explicitly",
                    table_name, col.name
                ),
            });
        }
    }
    Ok(())
}

fn assign_auto_increment(
    storage: &dyn StorageEngine,
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

    // Phase 24.1c: `GENERATED ALWAYS AS IDENTITY` rejects explicit values
    // (SQL:2003). MySQL `AUTO_INCREMENT` and `GENERATED BY DEFAULT AS
    // IDENTITY` both accept them. NULL / 0 / missing still auto-generate
    // in every form.
    if schema_cols[ai_col].identity_kind == axiomdb_catalog::IdentityKind::Always {
        match values.get(ai_col) {
            Some(Value::Null) | Some(Value::Int(0)) | Some(Value::BigInt(0)) | None => {}
            Some(_) => {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "cannot insert explicit value into column '{}': \
                         column is GENERATED ALWAYS AS IDENTITY",
                        schema_cols[ai_col].name
                    ),
                });
            }
        }
    }

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
pub fn enforce_text_constraints(
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

/// Phase 21.6 — CHECK enforcement with column-name resolution. Walks the
/// re-parsed expression and replaces each `Expr::Column { name, col_idx }`
/// with the actual `col_idx` from the schema's `columns` list. Needed
/// because CREATE TABLE persists the CHECK SQL without the name→index
/// resolution that ALTER ADD CONSTRAINT does via the analyzer.
///
/// SQL standard: CHECK passes when expr is TRUE or UNKNOWN (NULL); only
/// explicit FALSE is a violation.
pub fn check_row_constraints_with_cols(
    constraints: &[axiomdb_catalog::schema::ConstraintDef],
    row_values: &[Value],
    table_name: &str,
    columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<(), DbError> {
    for c in constraints {
        if c.check_expr.is_empty() {
            continue;
        }
        let mut expr = match crate::parser::parse_expr_only(&c.check_expr) {
            Ok(e) => e,
            Err(_) => continue,
        };
        resolve_column_refs(&mut expr, columns);
        let result = eval(&expr, row_values)?;
        if matches!(result, Value::Bool(false)) {
            return Err(DbError::CheckViolation {
                table: table_name.to_string(),
                constraint: c.name.clone(),
            });
        }
    }
    Ok(())
}

fn resolve_column_refs(
    expr: &mut Expr,
    columns: &[axiomdb_catalog::schema::ColumnDef],
) {
    use crate::expr::Expr as E;
    match expr {
        E::Column { col_idx, name } => {
            // Strip optional `table.` prefix for lookup.
            let bare = name.rsplit('.').next().unwrap_or(name);
            if let Some(pos) = columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(bare))
            {
                *col_idx = pos;
            }
        }
        E::BinaryOp { left, right, .. } => {
            resolve_column_refs(left, columns);
            resolve_column_refs(right, columns);
        }
        E::UnaryOp { operand, .. } => resolve_column_refs(operand, columns),
        E::Between { expr, low, high, .. } => {
            resolve_column_refs(expr, columns);
            resolve_column_refs(low, columns);
            resolve_column_refs(high, columns);
        }
        E::Like { expr, pattern, .. } => {
            resolve_column_refs(expr, columns);
            resolve_column_refs(pattern, columns);
        }
        E::In { expr, list, .. } => {
            resolve_column_refs(expr, columns);
            for e in list {
                resolve_column_refs(e, columns);
            }
        }
        E::IsNull { expr, .. } => resolve_column_refs(expr, columns),
        E::IsBoolean { expr, .. } => resolve_column_refs(expr, columns),
        E::Cast { expr, .. } => resolve_column_refs(expr, columns),
        E::Function { args, .. } => {
            for a in args {
                resolve_column_refs(a, columns);
            }
        }
        E::Case { operand, when_thens, else_result } => {
            if let Some(o) = operand {
                resolve_column_refs(o, columns);
            }
            for (w, t) in when_thens {
                resolve_column_refs(w, columns);
                resolve_column_refs(t, columns);
            }
            if let Some(e) = else_result {
                resolve_column_refs(e, columns);
            }
        }
        _ => {}
    }
}

// ── ALTER TABLE constraint helpers (Phase 4.22b) ──────────────────────────────
