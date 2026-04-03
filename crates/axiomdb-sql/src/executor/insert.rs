fn execute_insert_ctx(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let resolved = resolve_table_cached(
        storage,
        txn,
        ctx,
        &stmt.table,
    )?;
    if resolved.def.is_clustered() {
        if ctx.pending_inserts.is_some() {
            flush_pending_inserts_ctx(storage, txn, bloom, ctx)?;
        }
        return execute_clustered_insert_ctx(stmt, storage, txn, bloom, ctx, resolved);
    }
    resolved
        .def
        .ensure_heap_runtime("INSERT into clustered table — Phase 39.14")?;

    let schema_cols = &resolved.columns;
    let mut secondary_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

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

    // Find the AUTO_INCREMENT column (at most one per table).
    let auto_inc_col: Option<usize> = schema_cols.iter().position(|c| c.auto_increment);
    let mut first_generated: Option<u64> = None;

    fn next_auto_inc_ctx(
        storage: &mut dyn StorageEngine,
        txn: &TxnManager,
        table_def: &axiomdb_catalog::schema::TableDef,
        schema_cols: &[axiomdb_catalog::schema::ColumnDef],
        col_idx: usize,
    ) -> Result<u64, DbError> {
        let table_id = table_def.id;
        let cached = AUTO_INC_SEQ.with(|seq| seq.borrow().get(&table_id).copied());
        if let Some(next) = cached {
            AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
            return Ok(next);
        }
        let snap = txn.active_snapshot()?;
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

    match stmt.source {
        // ── INSERT ... VALUES — staging path (explicit transaction) ───────────
        InsertSource::Values(rows) if ctx.in_explicit_txn => {
            // If there is already a staged batch for a different table, flush it first.
            let needs_flush = ctx
                .pending_inserts
                .as_ref()
                .map(|b| b.table_id != resolved.def.id)
                .unwrap_or(false);
            if needs_flush {
                flush_pending_inserts_ctx(storage, txn, bloom, ctx)?;
            }

            // Initialise the batch if this is the first INSERT for this table.
            if ctx.pending_inserts.is_none() {
                let committed_empty =
                    detect_committed_empty_unique_indexes(storage, &secondary_indexes)?;
                ctx.pending_inserts = Some(crate::session::PendingInsertBatch {
                    table_id: resolved.def.id,
                    table_def: resolved.def.clone(),
                    columns: resolved.columns.clone(),
                    indexes: secondary_indexes.clone(),
                    compiled_preds: compiled_preds.clone(),
                    rows: Vec::new(),
                    unique_seen: std::collections::HashMap::new(),
                    committed_empty,
                });
            }

            for value_exprs in rows {
                let provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;

                let mut full_values: Vec<Value> = col_positions
                    .iter()
                    .map(|&idx| {
                        if idx == usize::MAX {
                            Value::Null
                        } else {
                            provided.get(idx).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();

                // Assign AUTO_INCREMENT now (before staging).
                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id =
                            next_auto_inc_ctx(storage, txn, &resolved.def, schema_cols, ai_col)?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }

                // CHECK constraints evaluated at enqueue time.
                check_row_constraints(
                    &resolved.constraints,
                    &full_values,
                    &resolved.def.table_name,
                )?;

                // FK child validation at enqueue time.
                if !resolved.foreign_keys.is_empty() {
                    crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        bloom,
                    )?;
                }

                // UNIQUE / PK precheck against committed indexes and in-buffer keys.
                // Detects duplicates before any heap mutation so errors surface immediately.
                {
                    let batch = ctx.pending_inserts.as_mut().expect("batch initialised above");
                    for idx in batch.indexes.iter() {
                        if !idx.is_unique || idx.is_fk_index {
                            continue;
                        }
                        let key_vals: Vec<Value> = idx
                            .columns
                            .iter()
                            .map(|c| {
                                full_values
                                    .get(c.col_idx as usize)
                                    .cloned()
                                    .unwrap_or(Value::Null)
                            })
                            .collect();
                        if key_vals.iter().any(|v| matches!(v, Value::Null)) {
                            continue; // NULL never violates UNIQUE
                        }
                        let key = crate::key_encoding::encode_index_key(&key_vals)?;
                        // Check committed index — skip when we know the
                        // committed BTree was empty at batch creation.
                        if !batch.committed_empty.contains(&idx.index_id)
                            && BTree::lookup_in(storage, idx.root_page_id, &key)?.is_some()
                        {
                            return Err(DbError::UniqueViolation {
                                index_name: idx.name.clone(),
                                value: key_vals.first().map(|v| format!("{v}")),
                            });
                        }
                        // Check in-buffer keys for this index.
                        let seen = batch
                            .unique_seen
                            .entry(idx.index_id)
                            .or_default();
                        if !seen.insert(key) {
                            return Err(DbError::UniqueViolation {
                                index_name: idx.name.clone(),
                                value: key_vals.first().map(|v| format!("{v}")),
                            });
                        }
                    }

                    // Enqueue the fully materialized row.
                    batch.rows.push(full_values);
                }

                count += 1;
            }

            // Return per-statement result immediately (no heap write yet).
            if let Some(id) = first_generated {
                THREAD_LAST_INSERT_ID.with(|v| v.set(id));
                return Ok(QueryResult::affected_with_id(count, id));
            }
            return Ok(QueryResult::Affected {
                count,
                last_insert_id: None,
            });
        }

        // ── INSERT ... VALUES — immediate path (autocommit / ineligible) ──────
        InsertSource::Values(rows) => {
            let mut full_batch: Vec<Vec<Value>> = Vec::with_capacity(rows.len());

            for value_exprs in rows {
                let provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;

                let mut full_values: Vec<Value> = col_positions
                    .iter()
                    .map(|&idx| {
                        if idx == usize::MAX {
                            Value::Null
                        } else {
                            provided.get(idx).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();

                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id =
                            next_auto_inc_ctx(storage, txn, &resolved.def, schema_cols, ai_col)?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }

                // Evaluate active CHECK constraints from axiom_constraints.
                check_row_constraints(
                    &resolved.constraints,
                    &full_values,
                    &resolved.def.table_name,
                )?;

                // FK validation: every non-NULL FK value must reference an existing parent row.
                if !resolved.foreign_keys.is_empty() {
                    crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        bloom,
                    )?;
                }

                full_batch.push(full_values);
            }

            if full_batch.len() == 1 {
                let full_values = full_batch.remove(0);
                let rid = TableEngine::insert_row_with_ctx(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    ctx,
                    full_values.clone(),
                    1,
                )?;
                if !secondary_indexes.is_empty() {
                    let snap = txn.active_snapshot()?;
                    let updated = crate::index_maintenance::insert_into_indexes_with_undo(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        bloom,
                        &compiled_preds,
                        snap,
                        Some(txn),
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                        if let Some(idx) = secondary_indexes
                            .iter_mut()
                            .find(|i| i.index_id == index_id)
                        {
                            idx.root_page_id = new_root;
                        }
                        ctx.invalidate_all();
                    }
                }
                count = 1;
            } else {
                let committed_empty = std::collections::HashSet::new();
                let n = full_batch.len() as u64;
                apply_insert_batch_with_ctx(
                    storage,
                    txn,
                    bloom,
                    ctx,
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
        InsertSource::Select(select_stmt) => {
            let select_rows = match execute_select_ctx(*select_stmt, storage, txn, bloom, ctx)? {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(DbError::Other(format!(
                        "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                    )))
                }
            };
            for (row_idx, row_values) in select_rows.into_iter().enumerate() {
                let mut full_values: Vec<Value> = col_positions
                    .iter()
                    .map(|&idx| {
                        if idx == usize::MAX {
                            Value::Null
                        } else {
                            row_values.get(idx).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();
                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id =
                            next_auto_inc_ctx(storage, txn, &resolved.def, schema_cols, ai_col)?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }
                // FK validation for INSERT SELECT path.
                if !resolved.foreign_keys.is_empty() {
                    crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        bloom,
                    )?;
                }
                // Clone so full_values remains available for index maintenance.
                let rid = TableEngine::insert_row_with_ctx(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    ctx,
                    full_values.clone(),
                    row_idx + 1,
                )?;
                if !secondary_indexes.is_empty() {
                    let snap = txn.active_snapshot()?;
                    let updated = crate::index_maintenance::insert_into_indexes_with_undo(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        bloom,
                        &compiled_preds,
                        snap,
                        Some(txn),
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                        if let Some(idx) = secondary_indexes
                            .iter_mut()
                            .find(|i| i.index_id == index_id)
                        {
                            idx.root_page_id = new_root;
                        }
                    }
                }
                count += 1;
            }
        }
        InsertSource::DefaultValues => {
            return Err(DbError::NotImplemented {
                feature: "DEFAULT VALUES — Phase 4.3c".into(),
            })
        }
    }

    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        // Track row changes for stats staleness (Phase 6.11).
        ctx.stats.on_rows_changed(resolved.def.id, count);
        return Ok(QueryResult::affected_with_id(count, id));
    }

    // Track row changes for stats staleness (Phase 6.11).
    ctx.stats.on_rows_changed(resolved.def.id, count);

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}


fn execute_insert(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };
    if resolved.def.is_clustered() {
        return execute_clustered_insert(stmt, storage, txn, resolved);
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
    let mut noop_bloom = crate::bloom::BloomRegistry::new();

    // Find the AUTO_INCREMENT column index (at most one per table).
    let auto_inc_col: Option<usize> = schema_cols.iter().position(|c| c.auto_increment);

    // Track the first generated ID for LAST_INSERT_ID() semantics.
    let mut first_generated: Option<u64> = None;

    /// Returns the next value from the per-table AUTO_INCREMENT sequence,
    /// initializing it from MAX(col)+1 on first use (restart-safe).
    fn next_auto_inc(
        storage: &mut dyn StorageEngine,
        txn: &TxnManager,
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
        let snap = txn.active_snapshot()?;
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

    match stmt.source {
        // ── INSERT ... VALUES ─────────────────────────────────────────────────
        InsertSource::Values(rows) => {
            // ── Phase 1: evaluate expressions + resolve AUTO_INCREMENT for all rows ──
            // This is done upfront so that:
            // (a) any expression error fails fast before touching the heap, and
            // (b) the batch path receives final Value vecs (no per-row eval inside batch).
            let mut full_batch: Vec<Vec<Value>> = Vec::with_capacity(rows.len());

            for value_exprs in &rows {
                let provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;

                let mut full_values: Vec<Value> = col_positions
                    .iter()
                    .map(|&idx| {
                        if idx == usize::MAX {
                            Value::Null
                        } else {
                            provided.get(idx).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();

                // AUTO_INCREMENT: assign the next ID before batching.
                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id = next_auto_inc(storage, txn, &resolved.def, schema_cols, ai_col)?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }

                full_batch.push(full_values);
            }

            // ── Phase 2: insert into the heap / indexes ──────────────────────
            //
            // Single-row path stays unchanged.
            // Multi-row `VALUES` now uses the batch heap path even when indexes
            // exist, then applies grouped index maintenance once per statement.
            if full_batch.len() == 1 {
                // ── Single row — existing path, no overhead ────────────────────
                let full_values = full_batch.remove(0);
                let rid = TableEngine::insert_row(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    full_values.clone(),
                )?;
                if !secondary_indexes.is_empty() {
                    let snap = txn.active_snapshot()?;
                    let updated = crate::index_maintenance::insert_into_indexes_with_undo(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        &mut noop_bloom,
                        &compiled_preds,
                        snap,
                        Some(txn),
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                    }
                }
                count = 1;
            } else {
                let n = full_batch.len() as u64;
                let committed_empty = std::collections::HashSet::new();
                apply_insert_batch(
                    storage,
                    txn,
                    &mut noop_bloom,
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
            let select_rows = match execute_select(*select_stmt, storage, txn)? {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(DbError::Other(format!(
                        "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                    )))
                }
            };

            for row_values in select_rows {
                let mut full_values: Vec<Value> = col_positions
                    .iter()
                    .map(|&idx| {
                        if idx == usize::MAX {
                            Value::Null
                        } else {
                            row_values.get(idx).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();

                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id = next_auto_inc(storage, txn, &resolved.def, schema_cols, ai_col)?;
                        full_values[ai_col] = match schema_cols[ai_col].col_type {
                            axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                            _ => Value::Int(id as i32),
                        };
                        if first_generated.is_none() {
                            first_generated = Some(id);
                        }
                    }
                }

                let rid = TableEngine::insert_row(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    full_values.clone(),
                )?;
                if !secondary_indexes.is_empty() {
                    let snap = txn.active_snapshot()?;
                    let updated = crate::index_maintenance::insert_into_indexes_with_undo(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        &mut noop_bloom,
                        &compiled_preds,
                        snap,
                        Some(txn),
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                    }
                }
                count += 1;
            }
        }

        InsertSource::DefaultValues => {
            return Err(DbError::NotImplemented {
                feature: "DEFAULT VALUES — Phase 4.3c".into(),
            })
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

fn execute_clustered_insert_ctx(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
    resolved: ResolvedTable,
) -> Result<QueryResult, DbError> {
    let schema_cols = &resolved.columns;
    let primary_idx =
        crate::clustered_table::primary_index(&resolved.indexes, &resolved.def.table_name)?.clone();
    let mut secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
        .cloned()
        .collect();
    let secondary_layouts: Vec<crate::clustered_secondary::ClusteredSecondaryLayout> =
        secondary_indexes
            .iter()
            .map(|idx| crate::clustered_secondary::ClusteredSecondaryLayout::derive(idx, &primary_idx))
            .collect::<Result<_, _>>()?;
    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, schema_cols)?;
    let col_positions =
        build_insert_column_positions(schema_cols, &stmt.columns, &resolved.def.table_name)?;

    let mut prepared_rows = Vec::new();
    let mut first_generated = None;

    match stmt.source {
        InsertSource::Values(rows) => {
            for (row_idx, value_exprs) in rows.into_iter().enumerate() {
                let provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;
                let mut full_values = materialize_insert_row(&col_positions, &provided);
                assign_auto_increment(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
                check_row_constraints(
                    &resolved.constraints,
                    &full_values,
                    &resolved.def.table_name,
                )?;
                if !resolved.foreign_keys.is_empty() {
                    crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        bloom,
                    )?;
                }
                prepared_rows.push(crate::clustered_table::prepare_row_with_ctx(
                    full_values,
                    schema_cols,
                    &primary_idx,
                    &resolved.def.table_name,
                    ctx,
                    row_idx + 1,
                )?);
            }
        }
        InsertSource::Select(select_stmt) => {
            let select_rows = match execute_select_ctx(*select_stmt, storage, txn, bloom, ctx)? {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(DbError::Other(format!(
                        "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                    )))
                }
            };

            for (row_idx, row_values) in select_rows.into_iter().enumerate() {
                let mut full_values = materialize_insert_row(&col_positions, &row_values);
                assign_auto_increment(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
                check_row_constraints(
                    &resolved.constraints,
                    &full_values,
                    &resolved.def.table_name,
                )?;
                if !resolved.foreign_keys.is_empty() {
                    crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        bloom,
                    )?;
                }
                prepared_rows.push(crate::clustered_table::prepare_row_with_ctx(
                    full_values,
                    schema_cols,
                    &primary_idx,
                    &resolved.def.table_name,
                    ctx,
                    row_idx + 1,
                )?);
            }
        }
        InsertSource::DefaultValues => {
            return Err(DbError::NotImplemented {
                feature: "DEFAULT VALUES — Phase 4.3c".into(),
            })
        }
    }

    let count = apply_clustered_insert_rows(
        storage,
        txn,
        bloom,
        &resolved.def,
        &primary_idx,
        &mut secondary_indexes,
        &secondary_layouts,
        &compiled_preds,
        &prepared_rows,
    )?;
    ctx.stats.on_rows_changed(resolved.def.id, count);
    ctx.invalidate_all();

    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        return Ok(QueryResult::affected_with_id(count, id));
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

fn execute_clustered_insert(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    resolved: ResolvedTable,
) -> Result<QueryResult, DbError> {
    let schema_cols = &resolved.columns;
    let primary_idx =
        crate::clustered_table::primary_index(&resolved.indexes, &resolved.def.table_name)?.clone();
    let mut secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
        .cloned()
        .collect();
    let secondary_layouts: Vec<crate::clustered_secondary::ClusteredSecondaryLayout> =
        secondary_indexes
            .iter()
            .map(|idx| crate::clustered_secondary::ClusteredSecondaryLayout::derive(idx, &primary_idx))
            .collect::<Result<_, _>>()?;
    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, schema_cols)?;
    let col_positions =
        build_insert_column_positions(schema_cols, &stmt.columns, &resolved.def.table_name)?;

    let mut prepared_rows = Vec::new();
    let mut first_generated = None;
    let mut noop_bloom = crate::bloom::BloomRegistry::new();

    match stmt.source {
        InsertSource::Values(rows) => {
            for value_exprs in rows {
                let provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;
                let mut full_values = materialize_insert_row(&col_positions, &provided);
                assign_auto_increment(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
                check_row_constraints(
                    &resolved.constraints,
                    &full_values,
                    &resolved.def.table_name,
                )?;
                if !resolved.foreign_keys.is_empty() {
                    crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        &mut noop_bloom,
                    )?;
                }
                prepared_rows.push(crate::clustered_table::prepare_row(
                    full_values,
                    schema_cols,
                    &primary_idx,
                    &resolved.def.table_name,
                )?);
            }
        }
        InsertSource::Select(select_stmt) => {
            let select_rows = match execute_select(*select_stmt, storage, txn)? {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(DbError::Other(format!(
                        "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                    )))
                }
            };

            for row_values in select_rows {
                let mut full_values = materialize_insert_row(&col_positions, &row_values);
                assign_auto_increment(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
                check_row_constraints(
                    &resolved.constraints,
                    &full_values,
                    &resolved.def.table_name,
                )?;
                if !resolved.foreign_keys.is_empty() {
                    crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        &mut noop_bloom,
                    )?;
                }
                prepared_rows.push(crate::clustered_table::prepare_row(
                    full_values,
                    schema_cols,
                    &primary_idx,
                    &resolved.def.table_name,
                )?);
            }
        }
        InsertSource::DefaultValues => {
            return Err(DbError::NotImplemented {
                feature: "DEFAULT VALUES — Phase 4.3c".into(),
            })
        }
    }

    let count = apply_clustered_insert_rows(
        storage,
        txn,
        &mut noop_bloom,
        &resolved.def,
        &primary_idx,
        &mut secondary_indexes,
        &secondary_layouts,
        &compiled_preds,
        &prepared_rows,
    )?;

    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        return Ok(QueryResult::affected_with_id(count, id));
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

fn apply_clustered_insert_rows(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    table_def: &TableDef,
    primary_idx: &IndexDef,
    secondary_indexes: &mut [IndexDef],
    secondary_layouts: &[crate::clustered_secondary::ClusteredSecondaryLayout],
    compiled_preds: &[Option<Expr>],
    rows: &[crate::clustered_table::PreparedClusteredInsertRow],
) -> Result<u64, DbError> {
    if rows.is_empty() {
        return Ok(0);
    }

    let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    let snapshot = txn.active_snapshot()?;
    let mut current_root = txn
        .clustered_root(table_def.id)
        .unwrap_or(table_def.root_page_id);

    for row in rows {
        let visible_existing = axiomdb_storage::clustered_tree::lookup(
            storage,
            Some(current_root),
            &row.primary_key_bytes,
            &snapshot,
        )?;
        if visible_existing.is_some() {
            return Err(DbError::UniqueViolation {
                index_name: primary_idx.name.clone(),
                value: row.primary_key_values.first().map(|v| format!("{v}")),
            });
        }

        let new_header = axiomdb_storage::RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };

        let physical_existing = axiomdb_storage::clustered_tree::lookup_physical(
            storage,
            Some(current_root),
            &row.primary_key_bytes,
        )?;

        let new_root = if let Some(old_row) = physical_existing {
            let new_root = axiomdb_storage::clustered_tree::restore_exact_row_image(
                storage,
                current_root,
                &row.primary_key_bytes,
                &new_header,
                &row.encoded_row,
            )?;
            let old_image = axiomdb_wal::ClusteredRowImage::new(
                current_root,
                old_row.row_header,
                &old_row.row_data,
            );
            let new_image =
                axiomdb_wal::ClusteredRowImage::new(new_root, new_header, &row.encoded_row);
            txn.record_clustered_update(table_def.id, &row.primary_key_bytes, &old_image, &new_image)?;
            new_root
        } else {
            let new_root = axiomdb_storage::clustered_tree::insert(
                storage,
                Some(current_root),
                &row.primary_key_bytes,
                &new_header,
                &row.encoded_row,
            )?;
            let new_image =
                axiomdb_wal::ClusteredRowImage::new(new_root, new_header, &row.encoded_row);
            txn.record_clustered_insert(table_def.id, &row.primary_key_bytes, &new_image)?;
            new_root
        };
        current_root = new_root;

        for ((idx, layout), compiled_pred) in secondary_indexes
            .iter_mut()
            .zip(secondary_layouts.iter())
            .zip(compiled_preds.iter())
        {
            if let Some(pred) = compiled_pred {
                if !is_truthy(&eval(pred, &row.values)?) {
                    continue;
                }
            }

            let Some(entry) = layout.entry_from_row(&row.values)? else {
                continue;
            };

            let root_pid = std::sync::atomic::AtomicU64::new(idx.root_page_id);
            layout.insert_row(storage, &root_pid, &row.values)?;
            bloom.add(idx.index_id, &entry.physical_key);
            let new_index_root = root_pid.load(std::sync::atomic::Ordering::Acquire);
            txn.record_index_insert(idx.index_id, new_index_root, entry.physical_key)?;
            if new_index_root != idx.root_page_id {
                CatalogWriter::new(storage, txn)?.update_index_root(idx.index_id, new_index_root)?;
                idx.root_page_id = new_index_root;
            }
        }
    }

    if current_root != table_def.root_page_id {
        CatalogWriter::new(storage, txn)?.update_table_root(table_def.id, current_root)?;
    }

    Ok(rows.len() as u64)
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
    table_def: &axiomdb_catalog::schema::TableDef,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    values: &mut [Value],
    first_generated: &mut Option<u64>,
) -> Result<(), DbError> {
    let Some(ai_col) = schema_cols.iter().position(|c| c.auto_increment) else {
        return Ok(());
    };
    if !matches!(values.get(ai_col), Some(Value::Null)) {
        return Ok(());
    }

    let table_id = table_def.id;
    let cached = AUTO_INC_SEQ.with(|seq| seq.borrow().get(&table_id).copied());
    let next = if let Some(next) = cached {
        next
    } else {
        let snap = txn.active_snapshot()?;
        let max_existing = if table_def.is_clustered() {
            crate::clustered_table::scan_max_numeric_column(
                storage,
                txn.clustered_root(table_id).or(Some(table_def.root_page_id)),
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


fn check_row_constraints(
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
