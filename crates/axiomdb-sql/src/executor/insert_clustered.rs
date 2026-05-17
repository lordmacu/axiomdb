fn execute_clustered_insert(
    stmt: InsertStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
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
    let ignore = stmt.ignore;

    let mut prepared_rows = Vec::new();
    let mut first_generated = None;
    let noop_bloom = crate::bloom::BloomRegistry::new();

    /// Prepares one row for clustered insert and pushes it onto `prepared_rows`.
    /// When `ignore` is true, ignorable constraint errors silently skip the row.
    macro_rules! prepare_one_row {
        ($full_values:expr) => {{
            let mut fv = $full_values;
            match enforce_text_constraints(&resolved.columns, &mut fv)
                .and_then(|()| check_row_constraints_with_cols(&resolved.constraints, &fv, &resolved.def.table_name, &resolved.columns))
            {
                Err(e) if ignore && is_ignorable_insert_error(&e) => {}
                Err(e) => return Err(e),
                Ok(()) => {
                    if !resolved.foreign_keys.is_empty() {
                        match crate::fk_enforcement::check_fk_child_insert(
                            &fv,
                            &resolved.foreign_keys,
                            storage,
                            txn,
                            conn_txn,
                            &noop_bloom,
                        ) {
                            Err(e) if ignore && is_ignorable_insert_error(&e) => {}
                            Err(e) => return Err(e),
                            Ok(()) => match crate::clustered_table::prepare_row(
                                fv,
                                schema_cols,
                                &primary_idx,
                                &resolved.def.table_name,
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
                            },
                        }
                    } else {
                        match crate::clustered_table::prepare_row(
                            fv,
                            schema_cols,
                            &primary_idx,
                            &resolved.def.table_name,
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
            for value_exprs in rows {
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
                    conn_txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
                materialize_generated_columns(schema_cols, &mut full_values)?;
                prepare_one_row!(full_values);
            }
        }
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
                    conn_txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
                materialize_generated_columns(schema_cols, &mut full_values)?;
                prepare_one_row!(full_values);
            }
        }
        InsertSource::DefaultValues => {
            let mut full_values: Vec<Value> = schema_cols.iter().map(|_| Value::Null).collect();
            assign_auto_increment(
                storage,
                txn,
                conn_txn,
                &resolved.def,
                schema_cols,
                full_values.as_mut_slice(),
                &mut first_generated,
            )?;
            materialize_generated_columns(schema_cols, &mut full_values)?;
            prepare_one_row!(full_values);
        }
    }

    // Apply prepared rows to the B-tree.
    // When INSERT IGNORE is active, apply one row at a time so unique violations
    // on the primary key skip the offending row instead of aborting the statement.
    let count = if ignore {
        let mut total = 0u64;
        for i in 0..prepared_rows.len() {
            match apply_clustered_insert_rows(
                storage,
                txn,
                conn_txn,
                &noop_bloom,
                &resolved.def,
                &primary_idx,
                &mut secondary_indexes,
                &secondary_layouts,
                &compiled_preds,
                &prepared_rows[i..i + 1],
                None, // legacy non-ctx path — no session hint available
            ) {
                Ok(n) => total += n,
                Err(e) if is_ignorable_insert_error(&e) => {} // skip row
                Err(e) => return Err(e),
            }
        }
        total
    } else {
        apply_clustered_insert_rows(
            storage,
            txn,
            conn_txn,
            &noop_bloom,
            &resolved.def,
            &primary_idx,
            &mut secondary_indexes,
            &secondary_layouts,
            &compiled_preds,
            &prepared_rows,
            None, // legacy non-ctx path — no session hint available
        )?
    };

    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        return Ok(QueryResult::affected_with_id(count, id));
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn apply_clustered_insert_rows(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    table_def: &TableDef,
    primary_idx: &IndexDef,
    secondary_indexes: &mut [IndexDef],
    secondary_layouts: &[crate::clustered_secondary::ClusteredSecondaryLayout],
    compiled_preds: &[Option<Expr>],
    rows: &[crate::clustered_table::PreparedClusteredInsertRow],
    session_hint: Option<&mut Option<axiomdb_storage::clustered_tree::LeafCursorHint>>,
) -> Result<u64, DbError> {
    use std::time::{Duration, Instant};

    if rows.is_empty() {
        return Ok(0);
    }

    let txn_id = conn_txn.txn_id;
    let snapshot = txn.active_snapshot(conn_txn);
    let mut current_root = txn
        .clustered_root(table_def.id)
        .unwrap_or(table_def.root_page_id);
    let append_biased = rows
        .windows(2)
        .all(|pair| pair[0].primary_key_bytes < pair[1].primary_key_bytes);
    // Attack 5 step 5.4: prime the rightmost_leaf_hint from the session
    // when it matches THIS table — that's how the autocommit per-row
    // INSERT path can reuse the previous statement's leaf and trigger
    // the try_insert_rightmost_leaf_batch fast path on the very first row.
    let mut rightmost_leaf_hint: Option<u64> = session_hint
        .as_ref()
        .and_then(|slot| slot.as_ref())
        .filter(|h| {
            h.table_id == table_def.id
                && h.root_page_id == current_root
                && h.schema_version == table_def.schema_version
        })
        .map(|h| h.leaf_page_id);
    let debug_clustered_insert = std::env::var_os("AXIOMDB_DEBUG_CLUSTERED_INSERT").is_some();
    let mut fast_path_hits = 0u64;
    let mut physical_lookup_time = Duration::ZERO;
    let mut tree_insert_time = Duration::ZERO;
    let mut secondary_time = Duration::ZERO;
    let mut root_persist_time = Duration::ZERO;

    let mut row_idx = 0usize;
    while row_idx < rows.len() {
        let row = &rows[row_idx];
        let new_header = axiomdb_storage::RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };

        if append_biased {
            if let Some(hinted_leaf_pid) = rightmost_leaf_hint {
                let fast_try_started = debug_clustered_insert.then(Instant::now);
                let append_rows: Vec<axiomdb_storage::clustered_tree::RightmostAppendRow<'_>> =
                    rows[row_idx..]
                        .iter()
                        .map(|row| axiomdb_storage::clustered_tree::RightmostAppendRow {
                            key: &row.primary_key_bytes,
                            row_header: &new_header,
                            row_data: &row.encoded_row,
                        })
                        .collect();
                let inserted = axiomdb_storage::clustered_tree::try_insert_rightmost_leaf_batch(
                    storage,
                    Some(&mut conn_txn.local_page_batch),
                    hinted_leaf_pid,
                    &append_rows,
                )?;
                if let Some(started) = fast_try_started {
                    tree_insert_time += started.elapsed();
                }
                if inserted > 0 {
                    fast_path_hits += inserted as u64;
                    for inserted_row in &rows[row_idx..row_idx + inserted] {
                        let new_image = axiomdb_wal::ClusteredRowImage::new(
                            current_root,
                            new_header,
                            &inserted_row.encoded_row,
                        );
                        txn.record_clustered_insert(
                            conn_txn,
                            table_def.id,
                            &inserted_row.primary_key_bytes,
                            &new_image,
                        )?;

                        let (secondary_elapsed, root_persist_elapsed) =
                            maintain_clustered_secondary_inserts(
                                storage,
                                txn,
                                conn_txn,
                                bloom,
                                current_root,
                                secondary_indexes,
                                secondary_layouts,
                                compiled_preds,
                                &inserted_row.values,
                                debug_clustered_insert,
                            )?;
                        secondary_time += secondary_elapsed;
                        root_persist_time += root_persist_elapsed;
                    }

                    row_idx += inserted;
                    if row_idx < rows.len() {
                        rightmost_leaf_hint = None;
                    }
                    continue;
                }
            }
        }

        let lookup_started = debug_clustered_insert.then(Instant::now);
        let physical_existing = axiomdb_storage::clustered_tree::lookup_physical(
            storage,
            Some(current_root),
            &row.primary_key_bytes,
        )?;
        if let Some(started) = lookup_started {
            physical_lookup_time += started.elapsed();
        }

        let tree_started = debug_clustered_insert.then(Instant::now);
        let new_root = if let Some(old_row) = physical_existing {
            if old_row.row_header.is_visible(&snapshot) {
                return Err(DbError::UniqueViolation {
                    index_name: primary_idx.name.clone(),
                    value: row.primary_key_values.first().map(|v| format!("{v}")),
                });
            }

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
            txn.record_clustered_update(
                conn_txn,
                table_def.id,
                &row.primary_key_bytes,
                &old_image,
                &new_image,
            )?;
            new_root
        } else {
            let new_root = axiomdb_storage::clustered_tree::insert_with_batch(
                storage,
                Some(&mut conn_txn.local_page_batch),
                Some(current_root),
                &row.primary_key_bytes,
                &new_header,
                &row.encoded_row,
            )?;
            let new_image =
                axiomdb_wal::ClusteredRowImage::new(new_root, new_header, &row.encoded_row);
            txn.record_clustered_insert(conn_txn, table_def.id, &row.primary_key_bytes, &new_image)?;
            new_root
        };
        if let Some(started) = tree_started {
            tree_insert_time += started.elapsed();
        }
        current_root = new_root;
        if append_biased {
            rightmost_leaf_hint = Some(
                axiomdb_storage::clustered_tree::descend_to_leaf_pub(
                    storage,
                    current_root,
                    &row.primary_key_bytes,
                )?
                .header()
                .page_id,
            );
        }

        let (secondary_elapsed, root_persist_elapsed) = maintain_clustered_secondary_inserts(
            storage,
            txn,
            conn_txn,
            bloom,
            current_root,
            secondary_indexes,
            secondary_layouts,
            compiled_preds,
            &row.values,
            debug_clustered_insert,
        )?;
        secondary_time += secondary_elapsed;
        root_persist_time += root_persist_elapsed;

        row_idx += 1;
    }

    if current_root != table_def.root_page_id {
        let persist_started = debug_clustered_insert.then(Instant::now);
        CatalogWriter::new(storage, txn, conn_txn)?.update_table_root(table_def.id, current_root)?;
        if let Some(started) = persist_started {
            root_persist_time += started.elapsed();
        }
    }

    if debug_clustered_insert {
        eprintln!(
            "[clustered-insert-debug] rows={} append_biased={} fast_path_hits={} lookup_ms={:.3} tree_ms={:.3} secondary_ms={:.3} root_persist_ms={:.3}",
            rows.len(),
            append_biased,
            fast_path_hits,
            physical_lookup_time.as_secs_f64() * 1000.0,
            tree_insert_time.as_secs_f64() * 1000.0,
            secondary_time.as_secs_f64() * 1000.0,
            root_persist_time.as_secs_f64() * 1000.0,
        );
    }

    // Attack 5 step 5.4: persist the rightmost leaf as a session hint so
    // the NEXT autocommit INSERT statement can prime its fast path and
    // skip the descent entirely (the bench's insert_autocommit pattern).
    if let (Some(slot), Some(leaf_pid), Some(last_row)) = (
        session_hint,
        rightmost_leaf_hint,
        rows.last(),
    ) {
        // The leaf may have just been split inside try_insert_rightmost_leaf
        // — read the current max key directly from the page rather than
        // relying on the in-memory row data.
        if let Ok(leaf) = storage.read_page(leaf_pid) {
            use axiomdb_storage::clustered_leaf;
            if clustered_leaf::num_cells(&leaf) > 0 {
                let nc = clustered_leaf::num_cells(&leaf);
                if let (Ok(first), Ok(last)) = (
                    clustered_leaf::read_cell(&leaf, 0),
                    clustered_leaf::read_cell(&leaf, nc - 1),
                ) {
                    *slot = Some(axiomdb_storage::clustered_tree::LeafCursorHint {
                        table_id: table_def.id,
                        root_page_id: current_root,
                        leaf_page_id: leaf_pid,
                        min_key: first.key.to_vec(),
                        max_key: last.key.to_vec(),
                        schema_version: table_def.schema_version,
                    });
                }
            }
        }
        let _ = last_row; // suppress unused warning when assertion not used
    }

    Ok(rows.len() as u64)
}
