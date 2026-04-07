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
            .map(|idx| {
                crate::clustered_secondary::ClusteredSecondaryLayout::derive(idx, &primary_idx)
            })
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
                    ctx.conn_txn.as_ref().expect("active txn"),
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
                        ctx.conn_txn.as_ref().expect("conn_txn for fk_check_insert"),
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
                    ctx.conn_txn.as_ref().expect("active txn"),
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
                        ctx.conn_txn.as_ref().expect("conn_txn for fk_check_insert"),
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
            let mut full_values: Vec<Value> = schema_cols.iter().map(|_| Value::Null).collect();
            assign_auto_increment(
                storage,
                txn,
                ctx.conn_txn.as_ref().expect("active txn"),
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
                    ctx.conn_txn.as_ref().expect("conn_txn for fk_check_insert"),
                    bloom,
                )?;
            }
            prepared_rows.push(crate::clustered_table::prepare_row_with_ctx(
                full_values,
                schema_cols,
                &primary_idx,
                &resolved.def.table_name,
                ctx,
                1,
            )?);
        }
    }

    let count = apply_clustered_insert_rows(
        storage,
        txn,
        ctx.conn_txn.as_mut().expect("active txn"),
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

/// Maximum rows held in `ClusteredInsertBatch` before an automatic flush.
/// Prevents unbounded memory growth during very large single-transaction loads.
const CLUSTERED_BATCH_MAX_ROWS: usize = 200_000;

/// Enqueues rows from a `INSERT ... VALUES` statement into the `ClusteredInsertBatch`
/// staging buffer for the current explicit transaction.
///
/// Rows are validated (CHECK constraints, FK constraints) and encoded at enqueue
/// time. The batch is flushed via `flush_clustered_insert_batch` before any
/// barrier statement (SELECT, UPDATE, DELETE, DDL, COMMIT, SAVEPOINT, or INSERT
/// into a different table) or when `CLUSTERED_BATCH_MAX_ROWS` is reached.
///
/// ## PK duplicate detection
/// - **Intra-batch**: checked via `staged_pks` HashSet in O(1) — returns
///   `UniqueViolation` immediately and discards the batch.
/// - **Against committed data**: detected at flush time by `apply_clustered_insert_rows`
///   via `lookup_physical`.
fn enqueue_clustered_insert_ctx(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
    resolved: ResolvedTable,
) -> Result<QueryResult, DbError> {
    let schema_cols = &resolved.columns;
    let table_id = resolved.def.id;

    // Flush existing batch if it is for a different table.
    if ctx
        .clustered_insert_batch
        .as_ref()
        .is_some_and(|b| b.table_id != table_id)
    {
        flush_clustered_insert_batch(storage, txn, bloom, ctx)?;
    }

    // Initialize batch if none exists yet for this table.
    if ctx.clustered_insert_batch.is_none() {
        let primary_idx =
            crate::clustered_table::primary_index(&resolved.indexes, &resolved.def.table_name)?
                .clone();
        let secondary_indexes: Vec<IndexDef> = resolved
            .indexes
            .iter()
            .filter(|i| !i.is_primary && !i.columns.is_empty())
            .cloned()
            .collect();
        let secondary_layouts = secondary_indexes
            .iter()
            .map(|idx| {
                crate::clustered_secondary::ClusteredSecondaryLayout::derive(idx, &primary_idx)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let compiled_preds =
            crate::partial_index::compile_index_predicates(&secondary_indexes, schema_cols)?;

        ctx.clustered_insert_batch = Some(crate::session::ClusteredInsertBatch {
            table_id,
            table_def: resolved.def.clone(),
            columns: schema_cols.to_vec(),
            primary_idx,
            secondary_indexes,
            secondary_layouts,
            compiled_preds,
            rows: Vec::new(),
            staged_pks: std::collections::HashSet::new(),
        });
    }

    // Clone primary_idx to avoid a simultaneous borrow conflict with ctx when
    // calling prepare_row_with_ctx (which takes &mut ctx).
    let primary_idx = ctx
        .clustered_insert_batch
        .as_ref()
        .unwrap()
        .primary_idx
        .clone();

    let col_positions =
        build_insert_column_positions(schema_cols, &stmt.columns, &resolved.def.table_name)?;

    let mut count = 0u64;
    let mut first_generated = None;

    let rows = match stmt.source {
        InsertSource::Values(rows) => rows,
        _ => unreachable!("enqueue_clustered_insert_ctx: non-Values source"),
    };

    for (row_idx, value_exprs) in rows.into_iter().enumerate() {
        let provided: Vec<Value> = value_exprs
            .iter()
            .map(|e| eval(e, &[]))
            .collect::<Result<_, _>>()?;
        let mut full_values = materialize_insert_row(&col_positions, &provided);
        assign_auto_increment(
            storage,
            txn,
            ctx.conn_txn.as_ref().expect("active txn"),
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
                ctx.conn_txn.as_ref().expect("conn_txn for fk_check_insert"),
                bloom,
            )?;
        }

        // Encode the row (coercion + PK extraction + row codec).
        let prepared = crate::clustered_table::prepare_row_with_ctx(
            full_values,
            schema_cols,
            &primary_idx,
            &resolved.def.table_name,
            ctx,
            row_idx + 1,
        )?;

        // Intra-batch PK duplicate check — O(1) via staged_pks HashSet.
        if ctx
            .clustered_insert_batch
            .as_ref()
            .unwrap()
            .staged_pks
            .contains(&prepared.primary_key_bytes)
        {
            let idx_name = ctx
                .clustered_insert_batch
                .as_ref()
                .unwrap()
                .primary_idx
                .name
                .clone();
            let pk_first = prepared.primary_key_values.first().map(|v| format!("{v}"));
            ctx.clustered_insert_batch = None; // discard batch
            return Err(DbError::UniqueViolation {
                index_name: idx_name,
                value: pk_first,
            });
        }

        let batch = ctx.clustered_insert_batch.as_mut().unwrap();
        batch.staged_pks.insert(prepared.primary_key_bytes.clone());
        batch.rows.push(crate::session::StagedClusteredRow {
            values: prepared.values,
            encoded_row: prepared.encoded_row,
            primary_key_values: prepared.primary_key_values,
            primary_key_bytes: prepared.primary_key_bytes,
        });
        count += 1;
    }

    // Safety valve: flush immediately if the batch has grown very large.
    if ctx
        .clustered_insert_batch
        .as_ref()
        .is_some_and(|b| b.rows.len() >= CLUSTERED_BATCH_MAX_ROWS)
    {
        flush_clustered_insert_batch(storage, txn, bloom, ctx)?;
    }

    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        return Ok(QueryResult::affected_with_id(count, id));
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

