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
        stmt.table.schema.as_deref(),
        &stmt.table.name,
    )?;

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
                ctx.pending_inserts = Some(crate::session::PendingInsertBatch {
                    table_id: resolved.def.id,
                    table_def: resolved.def.clone(),
                    columns: resolved.columns.clone(),
                    indexes: secondary_indexes.clone(),
                    compiled_preds: compiled_preds.clone(),
                    rows: Vec::new(),
                    unique_seen: std::collections::HashMap::new(),
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
                        // Check committed index.
                        if BTree::lookup_in(storage, idx.root_page_id, &key)?.is_some() {
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
            for (row_idx, value_exprs) in rows.into_iter().enumerate() {
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
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        bloom,
                        &compiled_preds,
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                        if let Some(idx) = secondary_indexes
                            .iter_mut()
                            .find(|i| i.index_id == index_id)
                        {
                            idx.root_page_id = new_root;
                        }
                        // The schema cache stores the old root_page_id. Invalidate
                        // so the next call re-reads from catalog rather than calling
                        // lookup_in with a freed page id.
                        ctx.invalidate_all();
                    }
                }
                count += 1;
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
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        bloom,
                        &compiled_preds,
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
    let secondary_indexes: Vec<IndexDef> = resolved
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

            // ── Phase 2: insert into the heap ─────────────────────────────────
            //
            // Single-row path: use insert_row() directly — no Vec allocation
            // overhead, same as before this optimization.
            //
            // Multi-row path (N > 1, no secondary indexes): use insert_rows_batch()
            // which loads each heap page once for the entire batch (vs. once per row).
            //
            // Multi-row path (N > 1, with secondary indexes): fall back to the
            // per-row loop so that secondary index maintenance has the Value vecs
            // available for each row. This maintains correctness at a minor
            // performance cost; optimizing secondary-index batch maintenance is
            // deferred to a follow-up.
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
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        &mut noop_bloom,
                        &compiled_preds,
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                    }
                }
                count = 1;
            } else if secondary_indexes.is_empty() {
                // ── Multi-row batch, no secondary indexes — fast path ──────────
                // HeapChain::insert_batch() loads each page once, writes once.
                let n = full_batch.len() as u64;
                TableEngine::insert_rows_batch(
                    storage,
                    txn,
                    &resolved.def,
                    schema_cols,
                    &full_batch,
                )?;
                count = n;
            } else {
                // ── Multi-row with secondary indexes — per-row fallback ────────
                for full_values in full_batch {
                    let rid = TableEngine::insert_row(
                        storage,
                        txn,
                        &resolved.def,
                        schema_cols,
                        full_values.clone(),
                    )?;
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        &mut noop_bloom,
                        &compiled_preds,
                    )?;
                    for (index_id, new_root) in updated {
                        CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                    }
                    count += 1;
                }
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
                    let updated = crate::index_maintenance::insert_into_indexes(
                        &secondary_indexes,
                        &full_values,
                        rid,
                        storage,
                        &mut noop_bloom,
                        &compiled_preds,
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

