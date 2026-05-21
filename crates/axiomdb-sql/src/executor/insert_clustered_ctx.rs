/// Hashes the INSERT statement's column list into a stable u64.
///
/// `None` (= "INSERT without column list, use all columns in declaration
/// order") returns 0. Other shapes hash with `DefaultHasher`; we mix in a
/// non-zero discriminant when the hash happens to be 0 so a column list
/// cannot collide with the None sentinel.
fn columns_signature(columns: Option<&Vec<String>>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    match columns {
        None => 0,
        Some(c) => {
            let mut h = DefaultHasher::new();
            c.hash(&mut h);
            let r = h.finish();
            if r == 0 {
                1
            } else {
                r
            }
        }
    }
}

fn execute_clustered_insert_ctx(
    stmt: InsertStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
    resolved: Arc<ResolvedTable>,
) -> Result<QueryResult, DbError> {
    // SAFETY: see ExecutionContext::storage_mut / coord_mut / bloom_mut.
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let bloom = exec_ctx.bloom();

    // REPLACE INTO on clustered tables is deferred — the MVP only ships
    // against heap tables. Surface a clear error rather than silently
    // doing the wrong thing on the clustered secondary-index path.
    if stmt.replace {
        return Err(DbError::NotImplemented {
            feature: "REPLACE INTO on clustered tables (Phase follow-up)".into(),
        });
    }
    if stmt.on_duplicate_update.is_some() {
        return Err(DbError::NotImplemented {
            feature: "INSERT ... ON DUPLICATE KEY UPDATE on clustered tables (Phase follow-up)"
                .into(),
        });
    }
    if stmt.on_conflict.is_some() {
        return Err(DbError::NotImplemented {
            feature: "INSERT ... ON CONFLICT on clustered tables (Phase follow-up)".into(),
        });
    }

    // Phase 40.11: IX(table) — idempotent, before any clustered row write.
    if let Some(lm) = exec_ctx.lock_manager() {
        lm.acquire_table_lock_sync(
            conn_txn.txn_id,
            resolved.def.id,
            axiomdb_lock::LockMode::IntentionExclusive,
        )?;
    }

    let schema_cols = &resolved.columns;
    let primary_idx =
        crate::clustered_table::primary_index(&resolved.indexes, &resolved.def.table_name)?.clone();
    let mut secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
        .cloned()
        .collect();
    // Phase 21.8: compile per-column expressions once; zip with the layouts so
    // the clustered-secondary writer can evaluate LOWER(col) etc. for each row.
    let compiled_index_exprs =
        crate::partial_index::compile_index_exprs(&secondary_indexes, schema_cols)?;
    let secondary_layouts: Vec<crate::clustered_secondary::ClusteredSecondaryLayout> =
        secondary_indexes
            .iter()
            .zip(compiled_index_exprs.iter())
            .map(|(idx, exprs)| {
                crate::clustered_secondary::ClusteredSecondaryLayout::derive(idx, &primary_idx)
                    .and_then(|layout| layout.with_exprs(exprs.clone()))
            })
            .collect::<Result<_, _>>()?;
    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, schema_cols)?;
    // Attack 3.B Step 3: cache col_positions by (table_id, columns_signature),
    // version-stamped with schema_version for lazy eviction on DDL.
    // Skips ~1-3 µs per INSERT call (Vec<usize> alloc + per-column name lookup).
    let sig = columns_signature(stmt.columns.as_ref());
    let table_id_for_cache = resolved.def.id;
    let schema_version = resolved.def.schema_version;
    let col_positions = match ctx.get_insert_col_positions(
        table_id_for_cache,
        schema_version,
        sig,
    ) {
        Some(cached) => cached.clone(),
        None => {
            let computed = build_insert_column_positions(
                schema_cols,
                &stmt.columns,
                &resolved.def.table_name,
            )?;
            ctx.cache_insert_col_positions(
                table_id_for_cache,
                schema_version,
                sig,
                computed.clone(),
            );
            computed
        }
    };
    let ignore = stmt.ignore;

    let mut prepared_rows = Vec::new();
    let mut first_generated = None;

    macro_rules! prepare_one_row_ctx {
        ($full_values:expr, $row_idx:expr) => {{
            let mut fv = $full_values;
            match enforce_text_constraints(&resolved.columns, &mut fv)
                .and_then(|()| check_row_constraints_with_cols(&resolved.constraints, &fv, &resolved.def.table_name, &resolved.columns))
            {
                Err(e) if ignore && is_ignorable_insert_error(&e) => {}
                Err(e) => return Err(e),
                Ok(()) => {
                    let fk_ok = if !resolved.foreign_keys.is_empty() {
                        let (immediate_fks, deferred_fk_ids) =
                            crate::fk_enforcement::split_child_insert_foreign_keys(
                                &fv,
                                &resolved.foreign_keys,
                            );
                        ctx.mark_deferred_fk_constraints(deferred_fk_ids);
                        match crate::fk_enforcement::check_fk_child_insert(
                            &fv,
                            &immediate_fks,
                            storage,
                            txn,
                            &*conn_txn,
                            bloom,
                        ) {
                            Err(e) if ignore && is_ignorable_insert_error(&e) => false,
                            Err(e) => return Err(e),
                            Ok(()) => true,
                        }
                    } else {
                        true
                    };
                    if fk_ok {
                        match crate::clustered_table::prepare_row_with_ctx(
                            fv,
                            schema_cols,
                            &primary_idx,
                            &resolved.def.table_name,
                            ctx,
                            $row_idx,
                        ) {
                            Err(e) if ignore && is_ignorable_insert_error(&e) => {}
                            Err(e) => return Err(e),
                            Ok(row) => {
                                validate_enum_row_values(
                                    &row.values,
                                    schema_cols,
                                    storage,
                                    txn,
                                    conn_txn,
                                )?;
                                prepared_rows.push(row);
                            }
                        }
                    }
                }
            }
        }};
    }

    match stmt.source {
        InsertSource::Values(rows) => {
            for (row_idx, value_exprs) in rows.into_iter().enumerate() {
                let mut provided: Vec<Value> = value_exprs
                    .iter()
                    .map(|e| eval(e, &[]))
                    .collect::<Result<_, _>>()?;
                validate_generated_insert_exprs(
                    &col_positions,
                    &value_exprs,
                    schema_cols,
                    &resolved.def.table_name,
                )?;
                resolve_expr_defaults(&col_positions, &value_exprs, &mut provided, schema_cols);
                let mut full_values = materialize_insert_row(&col_positions, &provided, schema_cols);
                assign_auto_increment(
                    storage,
                    txn,
                    &*conn_txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
                materialize_generated_columns(schema_cols, &mut full_values)?;
                prepare_one_row_ctx!(full_values, row_idx + 1);
            }
        }
        InsertSource::Select(select_stmt) => {
            let select_rows = match execute_select_ctx(*select_stmt, exec_ctx, Some(&*conn_txn), ctx)? {
                QueryResult::Rows { rows, .. } => rows,
                other => {
                    return Err(DbError::Other(format!(
                        "INSERT SELECT: expected Rows from SELECT, got {other:?}"
                    )))
                }
            };

            for (row_idx, row_values) in select_rows.into_iter().enumerate() {
                validate_generated_insert_source_values(
                    &col_positions,
                    row_values.len(),
                    schema_cols,
                    &resolved.def.table_name,
                )?;
                let mut full_values = materialize_insert_row(&col_positions, &row_values, schema_cols);
                assign_auto_increment(
                    storage,
                    txn,
                    &*conn_txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
                materialize_generated_columns(schema_cols, &mut full_values)?;
                prepare_one_row_ctx!(full_values, row_idx + 1);
            }
        }
        InsertSource::DefaultValues => {
            let mut full_values: Vec<Value> = schema_cols.iter().map(|_| Value::Null).collect();
            assign_auto_increment(
                storage,
                txn,
                &*conn_txn,
                &resolved.def,
                schema_cols,
                full_values.as_mut_slice(),
                &mut first_generated,
            )?;
            materialize_generated_columns(schema_cols, &mut full_values)?;
            prepare_one_row_ctx!(full_values, 1);
        }
    }

    let count = if ignore {
        let mut total = 0u64;
        for i in 0..prepared_rows.len() {
            match apply_clustered_insert_rows(
                storage,
                txn,
                conn_txn,
                bloom,
                &resolved.def,
                &primary_idx,
                &mut secondary_indexes,
                &secondary_layouts,
                &compiled_preds,
                &prepared_rows[i..i + 1],
                Some(ctx.clustered_leaf_hint_slot()),
                /*defer_table_root_persist=*/ false,
            ) {
                Ok(n) => total += n,
                Err(e) if is_ignorable_insert_error(&e) => {}
                Err(e) => return Err(e),
            }
        }
        total
    } else {
        // Attack 5 step 5.4: pass the session leaf hint so the autocommit
        // per-row INSERT path can prime its fast path from the previous
        // statement's leaf. This is the bench-mover for insert_autocommit.
        apply_clustered_insert_rows(
            storage,
            txn,
            conn_txn,
            bloom,
            &resolved.def,
            &primary_idx,
            &mut secondary_indexes,
            &secondary_layouts,
            &compiled_preds,
            &prepared_rows,
            Some(ctx.clustered_leaf_hint_slot()),
            /*defer_table_root_persist=*/ false,
        )?
    };
    ctx.stats.on_rows_changed(resolved.def.id, count);
    ctx.invalidate_all();

    if !stmt.returning.is_empty() {
        let returning_rows: Vec<Vec<Value>> = prepared_rows
            .into_iter()
            .map(|p| p.values)
            .collect();
        if let Some(id) = first_generated {
            THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        }
        return project_returning(
            &stmt.returning,
            &returning_rows,
            &resolved.def,
            &resolved.columns,
        );
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
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
    resolved: Arc<ResolvedTable>,
) -> Result<QueryResult, DbError> {
    if stmt.replace {
        return Err(DbError::NotImplemented {
            feature: "REPLACE INTO on clustered tables (Phase follow-up)".into(),
        });
    }
    if stmt.on_duplicate_update.is_some() {
        return Err(DbError::NotImplemented {
            feature: "INSERT ... ON DUPLICATE KEY UPDATE on clustered tables (Phase follow-up)"
                .into(),
        });
    }
    if stmt.on_conflict.is_some() {
        return Err(DbError::NotImplemented {
            feature: "INSERT ... ON CONFLICT on clustered tables (Phase follow-up)".into(),
        });
    }

    // SAFETY: see ExecutionContext::storage_mut / coord_mut / bloom_mut.
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let bloom = exec_ctx.bloom();

    // Phase 40.11: IX(table) — idempotent, before batch staging.
    if let Some(lm) = exec_ctx.lock_manager() {
        lm.acquire_table_lock_sync(
            conn_txn.txn_id,
            resolved.def.id,
            axiomdb_lock::LockMode::IntentionExclusive,
        )?;
    }

    let schema_cols = &resolved.columns;
    let table_id = resolved.def.id;

    // Flush existing batch if it is for a different table.
    if ctx
        .clustered_insert_batch
        .as_ref()
        .is_some_and(|b| b.table_id != table_id)
    {
        flush_clustered_insert_batch(exec_ctx, ctx)?;
    }

    // Initialize batch if none exists yet for this table.
    if ctx.clustered_insert_batch.is_none() {
        let primary_idx = std::sync::Arc::new(
            crate::clustered_table::primary_index(&resolved.indexes, &resolved.def.table_name)?
                .clone(),
        );
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

    // Cheap Arc::clone (atomic increment) to drop the borrow on ctx before
    // the per-row loop calls prepare_row_with_ctx (which takes &mut ctx).
    // Replaces a full IndexDef clone (which would allocate Vec<IndexColumnDef>).
    let primary_idx = std::sync::Arc::clone(
        &ctx.clustered_insert_batch.as_ref().unwrap().primary_idx,
    );

    // Attack 3.B Step 3: cache col_positions by (table_id, columns_signature),
    // version-stamped with schema_version for lazy eviction on DDL.
    // Skips ~1-3 µs per INSERT call (Vec<usize> alloc + per-column name lookup).
    let sig = columns_signature(stmt.columns.as_ref());
    let table_id_for_cache = resolved.def.id;
    let schema_version = resolved.def.schema_version;
    let col_positions = match ctx.get_insert_col_positions(
        table_id_for_cache,
        schema_version,
        sig,
    ) {
        Some(cached) => cached.clone(),
        None => {
            let computed = build_insert_column_positions(
                schema_cols,
                &stmt.columns,
                &resolved.def.table_name,
            )?;
            ctx.cache_insert_col_positions(
                table_id_for_cache,
                schema_version,
                sig,
                computed.clone(),
            );
            computed
        }
    };
    let ignore = stmt.ignore;

    let mut count = 0u64;
    let mut first_generated = None;
    let statement_start_len = ctx
        .clustered_insert_batch
        .as_ref()
        .map_or(0, |batch| batch.rows.len());
    let mut statement_staged_pks: Vec<Vec<u8>> = Vec::new();

    let rows = match stmt.source {
        InsertSource::Values(rows) => rows,
        _ => unreachable!("enqueue_clustered_insert_ctx: non-Values source"),
    };

    for (row_idx, value_exprs) in rows.into_iter().enumerate() {
        let mut provided: Vec<Value> = crate::time_insert_phase!(eval_ns, {
            value_exprs
                .iter()
                .map(|e| eval(e, &[]))
                .collect::<Result<_, _>>()?
        });
        crate::time_insert_phase!(validate_ns, {
            validate_generated_insert_exprs(
                &col_positions,
                &value_exprs,
                schema_cols,
                &resolved.def.table_name,
            )?;
            resolve_expr_defaults(&col_positions, &value_exprs, &mut provided, schema_cols);
        });
        let mut full_values =
            crate::time_insert_phase!(validate_ns, {
                materialize_insert_row(&col_positions, &provided, schema_cols)
            });
        crate::time_insert_phase!(auto_inc_ns, {
            assign_auto_increment(
                storage,
                txn,
                &*conn_txn,
                &resolved.def,
                schema_cols,
                full_values.as_mut_slice(),
                &mut first_generated,
            )?;
        });
        crate::time_insert_phase!(generated_cols_ns, {
            materialize_generated_columns(schema_cols, &mut full_values)?;
        });
        let constraints_result = crate::time_insert_phase!(constraints_ns, {
            enforce_text_constraints(&resolved.columns, &mut full_values)
                .and_then(|()| check_row_constraints_with_cols(&resolved.constraints, &full_values, &resolved.def.table_name, &resolved.columns))
        });
        match constraints_result {
            Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
            other => other?,
        }
        if !resolved.foreign_keys.is_empty() {
            let fk_result = crate::time_insert_phase!(fk_check_ns, {
                let (immediate_fks, deferred_fk_ids) =
                    crate::fk_enforcement::split_child_insert_foreign_keys(
                        &full_values,
                        &resolved.foreign_keys,
                    );
                ctx.mark_deferred_fk_constraints(deferred_fk_ids);
                crate::fk_enforcement::check_fk_child_insert(
                    &full_values,
                    &immediate_fks,
                    storage,
                    txn,
                    &*conn_txn,
                    bloom,
                )
            });
            match fk_result {
                Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
                other => other?,
            }
        }

        // Encode the row (coercion + PK extraction + row codec).
        let prepared_result = crate::time_insert_phase!(prepare_row_ns, {
            crate::clustered_table::prepare_row_with_ctx(
                full_values,
                schema_cols,
                &primary_idx,
                &resolved.def.table_name,
                ctx,
                row_idx + 1,
            )
        });
        let prepared = match prepared_result {
            Err(e) if ignore && is_ignorable_insert_error(&e) => continue,
            other => other?,
        };
        crate::time_insert_phase!(enum_validate_ns, {
            validate_enum_row_values(&prepared.values, schema_cols, storage, txn, conn_txn)?;
        });

        // Intra-batch PK duplicate check — O(1) via staged_pks HashSet.
        let pk_already_staged = crate::time_insert_phase!(pk_dup_ns, {
            ctx.clustered_insert_batch
                .as_ref()
                .unwrap()
                .staged_pks
                .contains(&prepared.primary_key_bytes)
        });
        if pk_already_staged {
            if ignore {
                continue;
            }
            let idx_name = ctx
                .clustered_insert_batch
                .as_ref()
                .unwrap()
                .primary_idx
                .name
                .clone();
            let pk_first = prepared.primary_key_values.first().map(|v| format!("{v}"));
            if let Some(batch) = ctx.clustered_insert_batch.as_mut() {
                batch.rows.truncate(statement_start_len);
                for pk in &statement_staged_pks {
                    batch.staged_pks.remove(pk);
                }
            }
            return Err(DbError::UniqueViolation {
                index_name: idx_name,
                value: pk_first,
            });
        }

        crate::time_insert_phase!(batch_push_ns, {
            let batch = ctx.clustered_insert_batch.as_mut().unwrap();
            batch.staged_pks.insert(prepared.primary_key_bytes.clone());
            statement_staged_pks.push(prepared.primary_key_bytes.clone());
            batch.rows.push(crate::session::StagedClusteredRow {
                values: prepared.values,
                encoded_row: prepared.encoded_row,
                primary_key_values: prepared.primary_key_values,
                primary_key_bytes: prepared.primary_key_bytes,
            });
        });
        crate::bench_timings::bump_rows(1);
        count += 1;
    }

    // Safety valve: flush immediately if the batch has grown very large.
    if ctx
        .clustered_insert_batch
        .as_ref()
        .is_some_and(|b| b.rows.len() >= CLUSTERED_BATCH_MAX_ROWS)
    {
        flush_clustered_insert_batch(exec_ctx, ctx)?;
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
