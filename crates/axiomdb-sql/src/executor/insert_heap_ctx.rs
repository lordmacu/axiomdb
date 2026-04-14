fn execute_insert_ctx(
    stmt: InsertStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // Phase 21.4 — INSERT RETURNING is parser-accepted but executor wiring is
    // deferred to 21.4b: the INSERT path has ~10 exit points across heap /
    // clustered / batched / SELECT-source / ON DUPLICATE / REPLACE, and each
    // needs a post-write row-capture hook. Landing them in a dedicated subphase
    // to keep commits focused. Today: clear runtime rejection so ORMs fail
    // fast rather than silently missing returned rows.
    if !stmt.returning.is_empty() {
        return Err(DbError::NotImplemented {
            feature: "INSERT ... RETURNING — executor support deferred to 21.4b; \
                      parser + AST + analyzer landed in 21.4"
                .into(),
        });
    }
    // SAFETY: see ExecutionContext::storage_mut / coord_mut / bloom_mut.
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let bloom = exec_ctx.bloom();
    let resolved = resolve_table_cached(storage, txn, ctx, Some(conn_txn), &stmt.table)?;

    // Phase 40.11: IX(table) — once per statement, before any row write.
    // Idempotent: InnoDB `lock_table_has()` pattern — returns immediately if
    // this txn already holds IX on this table.
    if let Some(lm) = exec_ctx.lock_manager() {
        lm.acquire_table_lock_sync(
            conn_txn.txn_id,
            resolved.def.id,
            axiomdb_lock::LockMode::IntentionExclusive,
        )?;
    }

    if resolved.def.is_clustered() {
        if ctx.pending_inserts.is_some() {
            flush_pending_inserts_ctx(exec_ctx, ctx)?;
        }
        // For explicit transactions with a VALUES source, stage rows into the
        // batch instead of writing immediately.  All other cases (SELECT source,
        // autocommit) go through the existing single-statement path.
        if ctx.in_explicit_txn {
            if let InsertSource::Values(_) = &stmt.source {
                return enqueue_clustered_insert_ctx(stmt, exec_ctx, conn_txn, ctx, resolved);
            }
        }
        return execute_clustered_insert_ctx(stmt, exec_ctx, conn_txn, ctx, resolved);
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

    // Number of explicitly-named columns (None = INSERT without column list = all columns).
    let explicit_col_count: Option<usize> = stmt.columns.as_ref().map(|c| c.len());

    let mut count = 0u64;

    // Find the AUTO_INCREMENT column (at most one per table).
    let auto_inc_col: Option<usize> = schema_cols.iter().position(|c| c.auto_increment);
    let mut first_generated: Option<u64> = None;

    fn next_auto_inc_ctx(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &ConnectionTxn,
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
    let replace_mode = stmt.replace;
    // Pre-resolve the ODKU assignment list once so the per-row loop only
    // evaluates against the dual-row context, not parse-tree column names.
    let odku_assignments: Option<Vec<(usize, Expr)>> = match &stmt.on_duplicate_update {
        Some(list) => Some(resolve_odku_assignments(
            list,
            schema_cols,
            &resolved.def.table_name,
        )?),
        None => None,
    };
    let odku_mode = odku_assignments.is_some();

    match stmt.source {
        // ── INSERT ... VALUES — immediate path ────────────────────────────────
        // NOTE: heap-table INSERT staging (buffering rows in ctx.pending_inserts
        // before flush) was removed. Staging delayed the physical heap write and
        // its UndoInsert record until a barrier statement (DELETE, SELECT, etc.)
        // triggered a flush — which could happen *after* a user savepoint was
        // taken. If rollback_to_savepoint ran before the flush it captured
        // undo_len=0 and incorrectly undid the flush, making the pre-savepoint
        // row disappear (test: test_bulk_delete_savepoint_rollback_restores_data).
        // Clustered tables retain batch staging via enqueue_clustered_insert_ctx
        // (handled above) which tracks savepoint semantics via clustered_roots.
        // All heap inserts now use the immediate write path so UndoInsert is
        // recorded before any subsequent savepoint.
        InsertSource::Values(rows) => {
            let mut full_batch: Vec<Vec<Value>> = Vec::with_capacity(rows.len());

            for (row_idx, value_exprs) in rows.into_iter().enumerate() {
                let mut provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;
                resolve_expr_defaults(
                    &col_positions,
                    &value_exprs,
                    &mut provided,
                    &resolved.columns,
                );

                // MySQL error 1136: column count in VALUES must match the explicit column list.
                if let Some(expected) = explicit_col_count {
                    if provided.len() != expected {
                        return Err(DbError::ColumnCountMismatch {
                            expected,
                            got: provided.len(),
                            row: row_idx + 1,
                        });
                    }
                }

                let mut full_values =
                    materialize_insert_row(&col_positions, &provided, &resolved.columns);

                if let Some(ai_col) = auto_inc_col {
                    // MySQL: explicit 0 on AUTO_INCREMENT column is treated same as NULL.
                    let is_auto_trigger = matches!(
                        full_values.get(ai_col),
                        Some(Value::Null) | Some(Value::Int(0)) | Some(Value::BigInt(0))
                    );
                    if is_auto_trigger {
                        let id = next_auto_inc_ctx(
                            storage,
                            txn,
                            &*conn_txn,
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

                // CHAR(N) padding + VARCHAR(N) length check + CHECK constraints.
                match enforce_text_constraints(&resolved.columns, &mut full_values).and_then(|()| {
                    check_row_constraints_with_cols(
                        &resolved.constraints,
                        &full_values,
                        &resolved.def.table_name,
                        &resolved.columns,
                    )
                }) {
                    Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
                    other => other?,
                }

                // FK validation: every non-NULL FK value must reference an existing parent row.
                if !resolved.foreign_keys.is_empty() {
                    match crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        &*conn_txn,
                        bloom,
                    ) {
                        Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
                        other => other?,
                    }
                }

                full_batch.push(crate::table::coerce_values_with_ctx(
                    full_values,
                    schema_cols,
                    ctx,
                    row_idx + 1,
                )?);
            }

            if full_batch.len() == 1 || ignore || replace_mode || odku_mode {
                for (row_idx, full_values) in full_batch.into_iter().enumerate() {
                    // REPLACE: delete every row that would violate a PK / UNIQUE
                    // before attempting the INSERT. Each displaced row counts
                    // toward affected_rows (matches MariaDB's copied+deleted formula).
                    if replace_mode {
                        let deleted = replace_displace_conflicts_heap(
                            storage,
                            txn,
                            conn_txn,
                            bloom,
                            ctx,
                            &resolved,
                            schema_cols,
                            &full_values,
                        )?;
                        count += deleted;
                    }
                    // ODKU: if the proposed row would conflict with a PK /
                    // UNIQUE index, update the conflicting row in place.
                    // MySQL formula — insert=1, update_changed=2, update_unchanged=0.
                    if let Some(ref assigns) = odku_assignments {
                        match apply_odku_heap(
                            storage,
                            txn,
                            conn_txn,
                            bloom,
                            ctx,
                            &resolved,
                            schema_cols,
                            &mut secondary_indexes,
                            &compiled_preds,
                            assigns,
                            &full_values,
                        )? {
                            OdkuOutcome::UpdatedChanged => {
                                count += 2;
                                continue;
                            }
                            OdkuOutcome::UpdatedNoChange => {
                                continue;
                            }
                            OdkuOutcome::Inserted => {
                                // Fall through to the normal INSERT path below.
                            }
                        }
                    }
                    let rid = match TableEngine::insert_row_with_ctx(
                        storage,
                        txn,
                        &resolved.def,
                        schema_cols,
                        ctx,
                        conn_txn,
                        full_values.clone(),
                        row_idx + 1,
                    ) {
                        Ok(rid) => rid,
                        Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
                        Err(e) => return Err(e),
                    };
                    if !secondary_indexes.is_empty() {
                        let snap = txn.active_snapshot(&*conn_txn);
                        match crate::index_maintenance::insert_into_indexes_with_undo(
                            &secondary_indexes,
                            &full_values,
                            rid,
                            storage,
                            bloom,
                            &compiled_preds,
                            snap,
                            Some(txn),
                            Some(conn_txn),
                        ) {
                            Ok(updated) => {
                                for (index_id, new_root) in updated {
                                    CatalogWriter::new(storage, txn, conn_txn)?
                                        .update_index_root(index_id, new_root)?;
                                    if let Some(idx) = secondary_indexes
                                        .iter_mut()
                                        .find(|i| i.index_id == index_id)
                                    {
                                        idx.root_page_id = new_root;
                                    }
                                    ctx.invalidate_all();
                                }
                            }
                            Err(e) if ignore && is_ignorable_insert_error(&e) => {
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
                let committed_empty = std::collections::HashSet::new();
                let n = full_batch.len() as u64;
                apply_insert_batch_with_ctx(
                    storage,
                    txn,
                    bloom,
                    ctx,
                    conn_txn,
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
            let select_rows =
                match execute_select_ctx(*select_stmt, exec_ctx, Some(&*conn_txn), ctx)? {
                    QueryResult::Rows { rows, .. } => rows,
                    other => {
                        return Err(DbError::Other(format!(
                            "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                        )))
                    }
                };

            // Phase 11.15: batch INSERT SELECT — collect all rows first,
            // then use apply_insert_batch_with_ctx for batch heap+WAL+index.
            // Per-row: materialize, AUTO_INCREMENT, FK validation, coerce.
            // Batch: heap insert + WAL + index maintenance (single pass).
            let mut full_batch: Vec<Vec<Value>> = Vec::with_capacity(select_rows.len());
            for (row_idx, row_values) in select_rows.into_iter().enumerate() {
                let mut full_values =
                    materialize_insert_row(&col_positions, &row_values, &resolved.columns);
                if let Some(ai_col) = auto_inc_col {
                    if matches!(full_values.get(ai_col), Some(Value::Null)) {
                        let id = next_auto_inc_ctx(
                            storage,
                            txn,
                            &*conn_txn,
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
                // FK validation (still per-row — FK check reads catalog).
                if !resolved.foreign_keys.is_empty() {
                    match crate::fk_enforcement::check_fk_child_insert(
                        &full_values,
                        &resolved.foreign_keys,
                        storage,
                        txn,
                        &*conn_txn,
                        bloom,
                    ) {
                        Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
                        other => other?,
                    }
                }
                let full_values = crate::table::coerce_values_with_ctx(
                    full_values,
                    schema_cols,
                    ctx,
                    row_idx + 1,
                )?;
                full_batch.push(full_values);
            }

            // Batch insert: one heap pass + one WAL record per page + batch index maintenance.
            if !full_batch.is_empty() {
                if ignore || replace_mode || odku_mode {
                    // IGNORE / REPLACE / ODKU mode: fall back to per-row for
                    // error handling / conflict displacement / conflict-update.
                    for (row_idx, full_values) in full_batch.into_iter().enumerate() {
                        if replace_mode {
                            let deleted = replace_displace_conflicts_heap(
                                storage,
                                txn,
                                conn_txn,
                                bloom,
                                ctx,
                                &resolved,
                                schema_cols,
                                &full_values,
                            )?;
                            count += deleted;
                        }
                        if let Some(ref assigns) = odku_assignments {
                            match apply_odku_heap(
                                storage,
                                txn,
                                conn_txn,
                                bloom,
                                ctx,
                                &resolved,
                                schema_cols,
                                &mut secondary_indexes,
                                &compiled_preds,
                                assigns,
                                &full_values,
                            )? {
                                OdkuOutcome::UpdatedChanged => {
                                    count += 2;
                                    continue;
                                }
                                OdkuOutcome::UpdatedNoChange => {
                                    continue;
                                }
                                OdkuOutcome::Inserted => {}
                            }
                        }
                        let rid = match TableEngine::insert_row_with_ctx(
                            storage, txn, &resolved.def, schema_cols, ctx, conn_txn,
                            full_values.clone(), row_idx + 1,
                        ) {
                            Ok(rid) => rid,
                            Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
                            Err(e) => return Err(e),
                        };
                        if !secondary_indexes.is_empty() {
                            let snap = txn.active_snapshot(&*conn_txn);
                            match crate::index_maintenance::insert_into_indexes_with_undo(
                                &secondary_indexes, &full_values, rid, storage, bloom,
                                &compiled_preds, snap, Some(txn), Some(conn_txn),
                            ) {
                                Ok(updated) => {
                                    for (index_id, new_root) in updated {
                                        CatalogWriter::new(storage, txn, conn_txn)?
                                            .update_index_root(index_id, new_root)?;
                                        if let Some(idx) = secondary_indexes.iter_mut().find(|i| i.index_id == index_id) {
                                            idx.root_page_id = new_root;
                                        }
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
                } else {
                    // Fast batch path: single heap pass + batch WAL + batch index.
                    let committed_empty = std::collections::HashSet::new();
                    let n = full_batch.len() as u64;
                    apply_insert_batch_with_ctx(
                        storage, txn, bloom, ctx, conn_txn,
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
        }
        InsertSource::DefaultValues => {
            // Build a row where every column is NULL, then apply AUTO_INCREMENT.
            let mut full_values: Vec<Value> = schema_cols.iter().map(|_| Value::Null).collect();
            if let Some(ai_col) = auto_inc_col {
                if matches!(full_values.get(ai_col), Some(Value::Null)) {
                    let id = next_auto_inc_ctx(
                        storage,
                        txn,
                        &*conn_txn,
                        &resolved.def,
                        schema_cols,
                        ai_col,
                    )?;
                    full_values[ai_col] = match schema_cols[ai_col].col_type {
                        axiomdb_catalog::schema::ColumnType::BigInt => Value::BigInt(id as i64),
                        _ => Value::Int(id as i32),
                    };
                    first_generated = Some(id);
                }
            }
            enforce_text_constraints(&resolved.columns, &mut full_values)?;
            check_row_constraints_with_cols(
                &resolved.constraints,
                &full_values,
                &resolved.def.table_name,
                &resolved.columns,
            )?;
            if !resolved.foreign_keys.is_empty() {
                crate::fk_enforcement::check_fk_child_insert(
                    &full_values,
                    &resolved.foreign_keys,
                    storage,
                    txn,
                    &*conn_txn,
                    bloom,
                )?;
            }
            let full_values =
                crate::table::coerce_values_with_ctx(full_values, schema_cols, ctx, 1)?;
            if replace_mode {
                let deleted = replace_displace_conflicts_heap(
                    storage,
                    txn,
                    conn_txn,
                    bloom,
                    ctx,
                    &resolved,
                    schema_cols,
                    &full_values,
                )?;
                count += deleted;
            }
            let odku_outcome = if let Some(ref assigns) = odku_assignments {
                Some(apply_odku_heap(
                    storage,
                    txn,
                    conn_txn,
                    bloom,
                    ctx,
                    &resolved,
                    schema_cols,
                    &mut secondary_indexes,
                    &compiled_preds,
                    assigns,
                    &full_values,
                )?)
            } else {
                None
            };
            match odku_outcome {
                Some(OdkuOutcome::UpdatedChanged) => {
                    count += 2;
                    // Skip the INSERT; this row was resolved via UPDATE branch.
                    return Ok(QueryResult::Affected {
                        count,
                        last_insert_id: first_generated,
                    });
                }
                Some(OdkuOutcome::UpdatedNoChange) => {
                    return Ok(QueryResult::Affected {
                        count,
                        last_insert_id: first_generated,
                    });
                }
                Some(OdkuOutcome::Inserted) | None => {}
            }
            let rid = TableEngine::insert_row_with_ctx(
                storage,
                txn,
                &resolved.def,
                schema_cols,
                ctx,
                conn_txn,
                full_values.clone(),
                1,
            )?;
            if !secondary_indexes.is_empty() {
                let snap = txn.active_snapshot(&*conn_txn);
                let updated = crate::index_maintenance::insert_into_indexes_with_undo(
                    &secondary_indexes,
                    &full_values,
                    rid,
                    storage,
                    bloom,
                    &compiled_preds,
                    snap,
                    Some(txn),
                    Some(conn_txn),
                )?;
                for (index_id, new_root) in updated {
                    CatalogWriter::new(storage, txn, conn_txn)?
                        .update_index_root(index_id, new_root)?;
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
