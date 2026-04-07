fn execute_clustered_insert(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
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
    let mut noop_bloom = crate::bloom::BloomRegistry::new();

    /// Prepares one row for clustered insert and pushes it onto `prepared_rows`.
    /// When `ignore` is true, ignorable constraint errors silently skip the row.
    macro_rules! prepare_one_row {
        ($full_values:expr) => {{
            let fv = $full_values;
            match check_row_constraints(&resolved.constraints, &fv, &resolved.def.table_name) {
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
                            &mut noop_bloom,
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
                                Ok(row) => prepared_rows.push(row),
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
                            Ok(row) => prepared_rows.push(row),
                        }
                    }
                }
            }
        }};
    }

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
                    conn_txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
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
                let mut full_values = materialize_insert_row(&col_positions, &row_values);
                assign_auto_increment(
                    storage,
                    txn,
                    conn_txn,
                    &resolved.def,
                    schema_cols,
                    full_values.as_mut_slice(),
                    &mut first_generated,
                )?;
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
                &mut noop_bloom,
                &resolved.def,
                &primary_idx,
                &mut secondary_indexes,
                &secondary_layouts,
                &compiled_preds,
                &prepared_rows[i..i + 1],
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
            &mut noop_bloom,
            &resolved.def,
            &primary_idx,
            &mut secondary_indexes,
            &secondary_layouts,
            &compiled_preds,
            &prepared_rows,
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
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &mut crate::bloom::BloomRegistry,
    table_def: &TableDef,
    primary_idx: &IndexDef,
    secondary_indexes: &mut [IndexDef],
    secondary_layouts: &[crate::clustered_secondary::ClusteredSecondaryLayout],
    compiled_preds: &[Option<Expr>],
    rows: &[crate::clustered_table::PreparedClusteredInsertRow],
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
    let mut rightmost_leaf_hint = None;
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
            let new_root = axiomdb_storage::clustered_tree::insert(
                storage,
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

    Ok(rows.len() as u64)
}

