fn execute_insert(
    stmt: InsertStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver =
            make_resolver_with_database(storage, txn, Some(conn_txn), DEFAULT_DATABASE_NAME)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };
    if resolved.def.is_clustered() {
        return execute_clustered_insert(stmt, storage, txn, conn_txn, resolved);
    }
    resolved
        .def
        .ensure_heap_runtime("INSERT into clustered table — Phase 39.14")?;

    let schema_cols = &resolved.columns;

    // Determine the mapping: schema_column_index → values_row_index (or MAX = Null).
    let col_positions: Vec<usize> = match &stmt.columns {
        None => (0..schema_cols.len()).collect(),
        Some(named_cols) => {
            let mut map = vec![usize::MAX; schema_cols.len()];
            for (val_pos, col_name) in named_cols.iter().enumerate() {
                let schema_pos = schema_cols
                    .iter()
                    .position(|c| &c.name == col_name)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: resolved.def.table_name.clone(),
                    })?;
                map[schema_pos] = val_pos;
            }
            map
        }
    };

    let mut count = 0u64;

    // Use the already-loaded indexes from the resolved table (cached by SchemaCache).
    // Avoids a second catalog heap scan per INSERT.
    let mut secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    // No-op bloom for the non-ctx path (bloom is managed by execute_with_ctx callers).
    let noop_bloom = crate::bloom::BloomRegistry::new();

    // Find the AUTO_INCREMENT column index (at most one per table).
    let auto_inc_col: Option<usize> = schema_cols.iter().position(|c| c.auto_increment);

    // Track the first generated ID for LAST_INSERT_ID() semantics.
    let mut first_generated: Option<u64> = None;

    /// Returns the next value from the per-table AUTO_INCREMENT sequence,
    /// initializing it from MAX(col)+1 on first use (restart-safe).
    fn next_auto_inc(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &ConnectionTxn,
        table_def: &axiomdb_catalog::schema::TableDef,
        schema_cols: &[axiomdb_catalog::schema::ColumnDef],
        col_idx: usize,
    ) -> Result<u64, DbError> {
        let table_id = table_def.id;
        // Check if already initialized.
        let cached = AUTO_INC_SEQ.with(|seq| seq.borrow().get(&table_id).copied());
        if let Some(next) = cached {
            AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
            return Ok(next);
        }
        // First use: scan the table to find MAX of the auto-increment column.
        let snap = txn.active_snapshot(conn_txn);
        let rows = TableEngine::scan_table(storage, table_def, schema_cols, snap, None)?;
        let max_existing: u64 = rows
            .iter()
            .filter_map(|(_, vals)| vals.get(col_idx))
            .filter_map(|v| match v {
                Value::Int(n) => Some(*n as u64),
                Value::BigInt(n) => Some(*n as u64),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let next = max_existing + 1;
        AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
        Ok(next)
    }

    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, schema_cols)?;
    let ignore = stmt.ignore;

    match stmt.source {
        // ── INSERT ... VALUES ─────────────────────────────────────────────────
        InsertSource::Values(rows) => {
            // ── Phase 1: evaluate expressions + resolve AUTO_INCREMENT for all rows ──
            // This is done upfront so that:
            // (a) any expression error fails fast before touching the heap, and
            // (b) the batch path receives final Value vecs (no per-row eval inside batch).
            let mut full_batch: Vec<Vec<Value>> = Vec::with_capacity(rows.len());

            for value_exprs in &rows {
                let mut provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;
                resolve_expr_defaults(&col_positions, value_exprs, &mut provided, schema_cols);

                let mut full_values =
                    materialize_insert_row(&col_positions, &provided, schema_cols);

                // AUTO_INCREMENT: assign the next ID before batching.
                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id = next_auto_inc(
                            storage,
                            txn,
                            conn_txn,
                            &resolved.def,
                            schema_cols,
                            ai_col,
                        )?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }

                full_batch.push(crate::table::coerce_values(full_values, schema_cols)?);
            }

            // ── Phase 2: insert into the heap / indexes ──────────────────────
            //
            // Per-row path: used for single-row inserts and INSERT IGNORE.
            // Batch path: used for multi-row inserts without IGNORE.
            if full_batch.len() == 1 || ignore {
                for full_values in full_batch {
                    let rid = match TableEngine::insert_row(
                        storage,
                        txn,
                        conn_txn,
                        &resolved.def,
                        schema_cols,
                        full_values.clone(),
                    ) {
                        Ok(rid) => rid,
                        Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
                        Err(e) => return Err(e),
                    };
                    if !secondary_indexes.is_empty() {
                        let snap = txn.active_snapshot(conn_txn);
                        match crate::index_maintenance::insert_into_indexes_with_undo(
                            &secondary_indexes,
                            &full_values,
                            rid,
                            storage,
                            &noop_bloom,
                            &compiled_preds,
                            snap,
                            Some(txn),
                            Some(conn_txn),
                        ) {
                            Ok(updated) => {
                                for (index_id, new_root) in updated {
                                    CatalogWriter::new(storage, txn, conn_txn)?
                                        .update_index_root(index_id, new_root)?;
                                }
                            }
                            Err(e) if ignore && is_ignorable_insert_error(&e) => {
                                // Undo the heap insert — unique violation on secondary index.
                                TableEngine::delete_row(
                                    storage,
                                    txn,
                                    conn_txn,
                                    &resolved.def,
                                    rid,
                                )?;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    count += 1;
                }
            } else {
                let n = full_batch.len() as u64;
                let committed_empty = std::collections::HashSet::new();
                apply_insert_batch(
                    storage,
                    txn,
                    conn_txn,
                    &noop_bloom,
                    InsertBatchApply {
                        table_def: &resolved.def,
                        columns: schema_cols,
                        indexes: &mut secondary_indexes,
                        rows: &full_batch,
                        compiled_preds: &compiled_preds,
                        skip_unique_check: false,
                        committed_empty: &committed_empty,
                    },
                )?;
                count = n;
            }
        }

        // ── INSERT ... SELECT ─────────────────────────────────────────────────
        InsertSource::Select(select_stmt) => {
            let select_rows = match execute_select(*select_stmt, storage, txn, Some(conn_txn))? {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(DbError::Other(format!(
                        "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                    )))
                }
            };

            for row_values in select_rows {
                let mut full_values =
                    materialize_insert_row(&col_positions, &row_values, schema_cols);

                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id = next_auto_inc(
                            storage,
                            txn,
                            conn_txn,
                            &resolved.def,
                            schema_cols,
                            ai_col,
                        )?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }
                let full_values = crate::table::coerce_values(full_values, schema_cols)?;

                let rid = match TableEngine::insert_row(
                    storage,
                    txn,
                    conn_txn,
                    &resolved.def,
                    schema_cols,
                    full_values.clone(),
                ) {
                    Ok(rid) => rid,
                    Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
                    Err(e) => return Err(e),
                };
                if !secondary_indexes.is_empty() {
                    let snap = txn.active_snapshot(conn_txn);
                    match crate::index_maintenance::insert_into_indexes_with_undo(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        &noop_bloom,
                        &compiled_preds,
                        snap,
                        Some(txn),
                        Some(conn_txn),
                    ) {
                        Ok(updated) => {
                            for (index_id, new_root) in updated {
                                CatalogWriter::new(storage, txn, conn_txn)?
                                    .update_index_root(index_id, new_root)?;
                            }
                        }
                        Err(e) if ignore && is_ignorable_insert_error(&e) => {
                            TableEngine::delete_row(storage, txn, conn_txn, &resolved.def, rid)?;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                count += 1;
            }
        }

        InsertSource::DefaultValues => {
            let mut full_values: Vec<Value> = schema_cols.iter().map(|_| Value::Null).collect();
            if let Some(ai_col) = auto_inc_col {
                if matches!(full_values.get(ai_col), Some(Value::Null)) {
                    let id =
                        next_auto_inc(storage, txn, conn_txn, &resolved.def, schema_cols, ai_col)?;
                    full_values[ai_col] = match schema_cols[ai_col].col_type {
                        axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                        _ => Value::Int(id as i32),
                    };
                    if first_generated.is_none() {
                        first_generated = Some(id);
                    }
                }
            }
            let full_values = crate::table::coerce_values(full_values, schema_cols)?;
            let rid = TableEngine::insert_row(
                storage,
                txn,
                conn_txn,
                &resolved.def,
                schema_cols,
                full_values.clone(),
            )?;
            if !secondary_indexes.is_empty() {
                let snap = txn.active_snapshot(conn_txn);
                let updated = crate::index_maintenance::insert_into_indexes_with_undo(
                    &secondary_indexes,
                    &full_values,
                    rid,
                    storage,
                    &noop_bloom,
                    &compiled_preds,
                    snap,
                    Some(txn),
                    Some(conn_txn),
                )?;
                for (index_id, new_root) in updated {
                    CatalogWriter::new(storage, txn, conn_txn)?
                        .update_index_root(index_id, new_root)?;
                }
            }
            count += 1;
        }
    }

    // Update the thread-local LAST_INSERT_ID if we generated any IDs.
    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        return Ok(QueryResult::affected_with_id(count, id));
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

/// Returns `true` if the error should be silently suppressed by `INSERT IGNORE`.
///
/// MySQL INSERT IGNORE converts these constraint violations into warnings and
/// skips the offending row. All other errors are still propagated as failures.
fn is_ignorable_insert_error(e: &DbError) -> bool {
    matches!(
        e,
        DbError::UniqueViolation { .. }
            | DbError::DuplicateKey
            | DbError::NotNullViolation { .. }
            | DbError::ForeignKeyViolation { .. }
    )
}
