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
                /*defer_table_root_persist=*/ false,
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
            /*defer_table_root_persist=*/ false,
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

/// Attack 13: when `defer_table_root_persist == true`, the function
/// does NOT call `CatalogWriter::update_table_root` even if the
/// clustered root grew. The caller is responsible for calling
/// `flush_deferred_table_root` once at end-of-Appender-lifetime to
/// persist the final root in a single catalog write. This avoids
/// paying the catalog write cost (8.7ms on macOS APFS) per flush
/// — for an Appender doing N auto-flushes the saving is (N-1)
/// catalog writes per session.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_clustered_insert_rows(
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
    defer_table_root_persist: bool,
) -> Result<u64, DbError> {
    use std::time::{Duration, Instant};

    if rows.is_empty() {
        return Ok(0);
    }

    let txn_id = conn_txn.txn_id;
    let snapshot = txn.active_snapshot(conn_txn);
    // Attack 14: use the per-conn root (which tracks the in-progress
    // txn's writes) instead of the global `last_clustered_roots` map
    // (only updated at commit). For multi-flush Appenders this is the
    // difference between hitting and missing the rightmost-leaf fast
    // path on every flush after the first — the hint stored at the
    // end of flush N carries the NEW root, and flush N+1 would
    // otherwise compare it against the pre-txn (OLD) root.
    let mut current_root = txn
        .clustered_root_for_conn(conn_txn, table_def.id)
        .unwrap_or(table_def.root_page_id);

    // Attack 10: snapshot the secondary index roots BEFORE the batch
    // so flush_deferred_secondary_index_roots only fires
    // update_index_root for indexes whose root actually changed.
    let original_secondary_roots: Vec<u64> =
        secondary_indexes.iter().map(|i| i.root_page_id).collect();

    // Attack 11: classify each secondary index — eligible for
    // bulk-build (regular B-Tree + empty root + no partial-index
    // predicate) vs eager (per-row insert via Attack 10 path).
    // For eligible indexes we collect (encoded_key) per row and call
    // BTree::bulk_load_sorted once after the per-row loop.
    let bulk_eligible: Vec<bool> = secondary_indexes
        .iter()
        .zip(compiled_preds.iter())
        .map(|(idx, pred)| is_bulk_build_eligible(storage, idx, pred.as_ref()))
        .collect();
    // bulk_entries[i] is non-None only when bulk_eligible[i] == true.
    let mut bulk_entries: Vec<Option<Vec<Vec<u8>>>> = bulk_eligible
        .iter()
        .map(|&e| if e { Some(Vec::with_capacity(rows.len())) } else { None })
        .collect();
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
    let mut wal_time = Duration::ZERO;
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
                let inserted = axiomdb_storage::clustered_tree::try_insert_rightmost_leaf_batch(
                    storage,
                    Some(&mut conn_txn.local_page_batch),
                    hinted_leaf_pid,
                    rows[row_idx..].iter().map(|row| {
                        axiomdb_storage::clustered_tree::RightmostAppendRow {
                            key: &row.primary_key_bytes,
                            row_header: &new_header,
                            row_data: &row.encoded_row,
                        }
                    }),
                )?;
                if let Some(started) = fast_try_started {
                    tree_insert_time += started.elapsed();
                }
                if inserted > 0 {
                    fast_path_hits += inserted as u64;
                    // Attack 15: build all ClusteredRowImage's first, then
                    // emit a single batched WAL write. The previous per-row
                    // record_clustered_insert loop did N separate
                    // wal.append_with_buf calls — wasteful in the
                    // rightmost-leaf fast path where every row already
                    // shares the leaf and current_root.
                    let images: Vec<axiomdb_wal::ClusteredRowImage> =
                        rows[row_idx..row_idx + inserted]
                            .iter()
                            .map(|inserted_row| {
                                axiomdb_wal::ClusteredRowImage::new(
                                    current_root,
                                    new_header,
                                    &inserted_row.encoded_row,
                                )
                            })
                            .collect();
                    let wal_started = debug_clustered_insert.then(Instant::now);
                    let entries: Vec<(&[u8], &axiomdb_wal::ClusteredRowImage)> = rows
                        [row_idx..row_idx + inserted]
                        .iter()
                        .zip(images.iter())
                        .map(|(row, image)| (row.primary_key_bytes.as_slice(), image))
                        .collect();
                    txn.record_clustered_insert_batch(conn_txn, table_def.id, &entries)?;
                    if let Some(started) = wal_started {
                        wal_time += started.elapsed();
                    }

                    for inserted_row in &rows[row_idx..row_idx + inserted] {
                        let secondary_elapsed =
                            maintain_clustered_secondary_inserts_deferred(
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
                                &bulk_eligible,
                            )?;
                        secondary_time += secondary_elapsed;
                        // Attack 11: collect entries for bulk-eligible indexes.
                        collect_bulk_secondary_entries(
                            &mut bulk_entries,
                            &bulk_eligible,
                            secondary_layouts,
                            &inserted_row.values,
                        )?;
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

        let secondary_elapsed = maintain_clustered_secondary_inserts_deferred(
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
            &bulk_eligible,
        )?;
        secondary_time += secondary_elapsed;
        // Attack 11: collect entries for bulk-eligible indexes.
        collect_bulk_secondary_entries(
            &mut bulk_entries,
            &bulk_eligible,
            secondary_layouts,
            &row.values,
        )?;

        row_idx += 1;
    }

    // Attack 11: bulk-build the eligible secondary indexes from the
    // collected entries. Each one calls BTree::bulk_load_sorted once,
    // updating idx.root_page_id. flush_deferred_secondary_index_roots
    // below then emits the catalog write.
    bulk_build_eligible_secondaries(
        storage,
        bloom,
        secondary_indexes,
        &bulk_eligible,
        &mut bulk_entries,
    )?;

    // Attack 10: flush deferred secondary-index root changes in ONE
    // catalog write per CHANGED index (not per leaf split). Mirrors
    // the existing update_table_root call below.
    let persist_started = debug_clustered_insert.then(Instant::now);
    flush_deferred_secondary_index_roots(
        storage,
        txn,
        conn_txn,
        secondary_indexes,
        &original_secondary_roots,
    )?;
    if let Some(started) = persist_started {
        root_persist_time += started.elapsed();
    }

    if current_root != table_def.root_page_id && !defer_table_root_persist {
        let persist_started = debug_clustered_insert.then(Instant::now);
        CatalogWriter::new(storage, txn, conn_txn)?.update_table_root(table_def.id, current_root)?;
        if let Some(started) = persist_started {
            root_persist_time += started.elapsed();
        }
    }
    // When `defer_table_root_persist == true`, the caller will read
    // the latest root via `txn.clustered_root(table_def.id)` and emit
    // a single `update_table_root` at end-of-Appender-lifetime.

    if debug_clustered_insert {
        eprintln!(
            "[clustered-insert-debug] rows={} append_biased={} fast_path_hits={} lookup_ms={:.3} tree_ms={:.3} wal_ms={:.3} secondary_ms={:.3} root_persist_ms={:.3}",
            rows.len(),
            append_biased,
            fast_path_hits,
            physical_lookup_time.as_secs_f64() * 1000.0,
            tree_insert_time.as_secs_f64() * 1000.0,
            wal_time.as_secs_f64() * 1000.0,
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

// ── Attack 11 helpers — secondary index bulk-build ───────────────────────────

/// Returns `true` if a secondary index is eligible for bulk-build via
/// `BTree::bulk_load_sorted`. Requirements:
/// - Regular B-Tree index (idx.index_type == 0)
/// - NOT UNIQUE — UNIQUE indexes require logical-key dedup against the
///   existing tree (which is empty here, fine) AND against each other
///   within the batch. Our duplicate check on physical_key MISSES the
///   intra-batch case because physical_key = logical_key + PK_suffix
///   (so two rows with the same indexed value but different PKs have
///   different physical keys). Falling back to the per-row path uses
///   `ensure_unique_logical_key_absent` which checks logical keys
///   correctly.
/// - No partial-index predicate (we'd need to evaluate per row)
/// - The B-Tree's root page is an empty leaf
/// - Not BRIN / FTS / GIN / Trigram
fn is_bulk_build_eligible(
    storage: &dyn axiomdb_storage::StorageEngine,
    idx: &axiomdb_catalog::schema::IndexDef,
    compiled_pred: Option<&Expr>,
) -> bool {
    // index_type: 0 = regular B-Tree; 1 = BRIN; 2 = Trigram; 3 = FTS; 4 = GIN
    if idx.index_type != 0 {
        return false;
    }
    if idx.is_unique {
        return false;
    }
    if compiled_pred.is_some() {
        return false;
    }
    // Read root page and check that it's an empty leaf.
    let Ok(page_ref) = storage.read_page(idx.root_page_id) else {
        return false;
    };
    let bytes = *page_ref.as_bytes();
    let Ok(page) = axiomdb_storage::Page::from_bytes(bytes) else {
        return false;
    };
    let leaf = axiomdb_index::page_layout::cast_leaf(&page);
    leaf.is_leaf == 1 && leaf.num_keys() == 0
}

/// Collects the encoded physical key for each `bulk_eligible[i] == true`
/// secondary index, given the row's values.
fn collect_bulk_secondary_entries(
    bulk_entries: &mut [Option<Vec<Vec<u8>>>],
    bulk_eligible: &[bool],
    secondary_layouts: &[crate::clustered_secondary::ClusteredSecondaryLayout],
    row_values: &[Value],
) -> Result<(), DbError> {
    for (i, layout) in secondary_layouts.iter().enumerate() {
        if !bulk_eligible.get(i).copied().unwrap_or(false) {
            continue;
        }
        let Some(entry) = layout.entry_from_row(row_values)? else {
            continue;
        };
        if let Some(vec) = bulk_entries[i].as_mut() {
            vec.push(entry.physical_key);
        }
    }
    Ok(())
}

/// For each bulk-eligible secondary index with collected entries:
/// sort, check for duplicates (UniqueViolation on UNIQUE indexes),
/// call `BTree::bulk_load_sorted`, update `idx.root_page_id` and the
/// bloom filter.
fn bulk_build_eligible_secondaries(
    storage: &dyn axiomdb_storage::StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
    secondary_indexes: &mut [axiomdb_catalog::schema::IndexDef],
    bulk_eligible: &[bool],
    bulk_entries: &mut [Option<Vec<Vec<u8>>>],
) -> Result<(), DbError> {
    for (i, idx) in secondary_indexes.iter_mut().enumerate() {
        if !bulk_eligible.get(i).copied().unwrap_or(false) {
            continue;
        }
        let Some(entries) = bulk_entries[i].take() else {
            continue;
        };
        if entries.is_empty() {
            continue;
        }
        // Sort by physical_key.
        let mut sorted = entries;
        sorted.sort_unstable();
        // Duplicate detection — physical_key includes the PK suffix
        // so duplicates can only happen if the SAME row was processed
        // twice (a bug above us) or if the PK was re-used (the primary
        // tree would have rejected it first). UNIQUE indexes are
        // excluded from this path (see is_bulk_build_eligible), so
        // logical-key dedup isn't our responsibility here. If a
        // duplicate slips through it's a real corruption signal.
        debug_assert!(
            sorted.windows(2).all(|w| w[0] != w[1]),
            "non-unique index has duplicate physical_key after sort — corruption?"
        );
        // bulk_load_sorted wants &[(&[u8], RecordId)]; build a Vec of
        // references with the secondary's DUMMY_RID (page_id=0, slot_id=0).
        let dummy_rid = axiomdb_core::RecordId {
            page_id: 0,
            slot_id: 0,
        };
        let pairs: Vec<(&[u8], axiomdb_core::RecordId)> =
            sorted.iter().map(|k| (k.as_slice(), dummy_rid)).collect();
        let new_root = axiomdb_index::BTree::bulk_load_sorted(
            storage,
            idx.root_page_id,
            &pairs,
            idx.fillfactor,
        )?;
        idx.root_page_id = new_root;
        // Update bloom filter for every key.
        for key in &sorted {
            bloom.add(idx.index_id, key);
        }
    }
    Ok(())
}
