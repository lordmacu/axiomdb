/// Three update paths:
/// 1. Non-key in-place: `update_in_place()` when PK and index keys unchanged
/// 2. Non-key relocation: `update_with_relocation()` when row grows beyond leaf
/// 3. Key change: `delete_mark()` + `insert()` when PK changes
#[allow(clippy::too_many_arguments)]
fn execute_clustered_update(
    where_clause: Option<Expr>,
    order_by: &[OrderByItem],
    limit: Option<&Expr>,
    assignments: Vec<(usize, Expr)>,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    secondary_indexes: &[axiomdb_catalog::IndexDef],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    use std::ops::Bound;

    let col_types: Vec<axiomdb_types::DataType> = schema_cols
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();

    let pk_col_positions: Vec<usize> = clustered_update_primary_index(resolved)?
        .columns
        .iter()
        .map(|c| c.col_idx as usize)
        .collect();

    let mut root_pid = txn
        .clustered_root(resolved.def.id)
        .unwrap_or(resolved.def.root_page_id);

    // ── Fused clustered scan-patch fast path ─────────────────────────────
    // When ALL of these hold, skip candidate collection entirely and patch
    // fields directly on clustered leaf pages in a single pass:
    //   1. All SET cols fixed-size (field_patch eligible)
    //   2. No FK constraints
    //   3. No secondary indexes with changed key columns
    //   4. PK columns not changed
    let has_generated_columns = schema_cols.iter().any(|c| c.generated_expr.is_some());
    let field_patch_ok = !has_generated_columns
        && resolved.foreign_keys.is_empty()
        && assignments.iter().all(|(col_pos, _)| {
            axiomdb_types::field_patch::fixed_encoded_size(col_types[*col_pos].clone()).is_some()
        });
    let pk_might_change = assignments.iter().any(|(col_pos, _)| pk_col_positions.contains(col_pos));
    let has_affected_secondary = secondary_indexes.iter().any(|idx| {
        !idx.is_primary
            && (idx.predicate.is_some()
                || idx.columns.iter().any(|c| {
                    assignments.iter().any(|(a_pos, _)| *a_pos == c.col_idx as usize)
                }))
    });

    let access_method = where_clause
        .as_ref()
        .map(|wc| {
            normalize_clustered_update_access_method(crate::planner::plan_update_candidates_ctx(
                wc,
                secondary_indexes,
                schema_cols,
                ctx.effective_collation(),
            ))
        })
        .unwrap_or(crate::planner::AccessMethod::Scan);

    // ORDER BY / LIMIT require candidate collection + sort — skip the fast path.
    if field_patch_ok && !pk_might_change && !has_affected_secondary && order_by.is_empty() && limit.is_none() {
        let bounds = match &access_method {
            crate::planner::AccessMethod::Scan => {
                Some((Bound::Unbounded, Bound::Unbounded))
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key } if index_def.is_primary => {
                Some((Bound::Included(key.clone()), Bound::Included(key.clone())))
            }
            crate::planner::AccessMethod::IndexRange { index_def, lo, hi, .. } if index_def.is_primary => {
                Some((
                    lo.clone().map_or(Bound::Unbounded, Bound::Included),
                    hi.clone().map_or(Bound::Unbounded, Bound::Included),
                ))
            }
            _ => None,
        };

        if let Some((from, to)) = bounds {
            return fused_clustered_scan_patch(
                where_clause.as_ref(),
                &assignments,
                &col_types,
                storage,
                txn,
                conn_txn,
                snap.clone(),
                resolved,
                root_pid,
                ctx,
                from,
                to,
            );
        }
    }

    let mut candidates = collect_clustered_update_candidates(
        where_clause.as_ref(),
        schema_cols,
        secondary_indexes,
        &access_method,
        storage,
        snap.clone(),
        resolved,
        root_pid,
    )?;

    // Apply ORDER BY + LIMIT (G5.3): sort by row values then truncate.
    if !order_by.is_empty() || limit.is_some() {
        candidates = apply_order_by_limit_to_clustered_update_candidates(candidates, order_by, limit)?;
    }

    if candidates.is_empty() {
        return Ok(QueryResult::Affected {
            count: 0,
            last_insert_id: None,
        });
    }

    // ── FK pre-flight (before any storage mutation) ──────────────────────────
    // Mirrors the batch-check in execute_update_with_candidates:
    // 1. Child FK: new FK value must reference an existing parent row.
    // 2. Parent FK: RESTRICT/NO ACTION if a referenced column changes.
    {
        let mut batch_old: Vec<(axiomdb_core::RecordId, Vec<axiomdb_types::Value>)> = Vec::new();
        let mut batch_new: Vec<Vec<axiomdb_types::Value>> = Vec::new();

        for candidate in &candidates {
            let mut new_values = candidate.values.clone();
            for &(col_pos, ref val_expr) in &assignments {
                let nv = eval(val_expr, &candidate.values)?;
                new_values[col_pos] = nv;
            }
            materialize_generated_columns(schema_cols, &mut new_values)?;
            let changed = new_values != candidate.values;
            if !changed {
                continue;
            }
            // RID field is unused by enforce_fk_on_parent_update — use a zero placeholder.
            batch_old.push((
                axiomdb_core::RecordId { page_id: 0, slot_id: 0 },
                candidate.values.clone(),
            ));
            batch_new.push(new_values);
        }

        if !batch_old.is_empty() {
            if !resolved.foreign_keys.is_empty() {
                for ((_, old_values), new_values) in batch_old.iter().zip(batch_new.iter()) {
                    let (immediate_fks, deferred_fk_ids) =
                        crate::fk_enforcement::split_child_update_foreign_keys(
                            old_values,
                            new_values,
                            &resolved.foreign_keys,
                        );
                    ctx.mark_deferred_fk_constraints(deferred_fk_ids);
                    crate::fk_enforcement::check_fk_child_update(
                        old_values,
                        new_values,
                        &immediate_fks,
                        storage,
                        txn,
                        conn_txn,
                        bloom,
                    )?;
                }
            }
            crate::fk_enforcement::enforce_fk_on_parent_update(
                &batch_old,
                &batch_new,
                resolved.def.id,
                storage,
                txn,
                conn_txn,
                bloom,
                Some(&mut ctx.deferred_fk_constraint_ids),
            )?;
        }
    }

    let txn_id = conn_txn.txn_id;
    let mut matched_count = 0u64;
    let mut changed_count = 0u64;

    let primary_idx = clustered_update_primary_index(resolved)?;
    let compiled_secondary_preds =
        crate::partial_index::compile_index_predicates(secondary_indexes, schema_cols)?;
    // Phase 21.8: compile expression-index expressions so the secondary layout
    // can evaluate LOWER(col) etc. when rebuilding the logical key on UPDATE.
    let compiled_secondary_exprs =
        crate::partial_index::compile_index_exprs(secondary_indexes, schema_cols)?;
    let secondary_layouts: Vec<(
        &axiomdb_catalog::IndexDef,
        Option<crate::expr::Expr>,
        crate::clustered_secondary::ClusteredSecondaryLayout,
        std::sync::atomic::AtomicU64,
    )> = secondary_indexes
        .iter()
        .zip(compiled_secondary_preds.iter())
        .zip(compiled_secondary_exprs.iter())
        .filter(|((idx, _), _)| !idx.is_primary && !idx.columns.is_empty())
        .filter_map(|((idx, compiled_pred), idx_exprs)| {
            crate::clustered_secondary::ClusteredSecondaryLayout::derive(idx, primary_idx)
                .and_then(|layout| layout.with_exprs(idx_exprs.clone()))
                .ok()
                .map(|layout| {
                    (
                        idx,
                        compiled_pred.clone(),
                        layout,
                        std::sync::atomic::AtomicU64::new(idx.root_page_id),
                    )
                })
        })
        .collect();

    for candidate in &candidates {
        matched_count += 1;

        let mut new_values = candidate.values.clone();
        for &(col_pos, ref val_expr) in &assignments {
            let nv = eval(val_expr, &candidate.values)?;
            new_values[col_pos] = nv;
        }
        materialize_generated_columns(schema_cols, &mut new_values)?;
        let changed = new_values != candidate.values;
        if !changed {
            continue;
        }

        enforce_text_constraints(schema_cols, &mut new_values)?;
        new_values = crate::table::coerce_values(new_values, schema_cols)?;
        validate_enum_row_values(&new_values, schema_cols, storage, txn, conn_txn)?;

        let pk_changed = pk_col_positions
            .iter()
            .any(|&pos| candidate.values[pos] != new_values[pos]);

        // Field-patch optimization: when all changed columns are fixed-size
        // and PK is unchanged, patch bytes directly in the existing row_data
        // instead of full encode_row(). Saves ~16× per row.
        let field_patch_ok = !has_generated_columns
            && !pk_changed
            && assignments.iter().all(|(col_pos, _)| {
                axiomdb_types::field_patch::fixed_encoded_size(col_types[*col_pos].clone()).is_some()
            });

        let new_row_data = if field_patch_ok {
            // Patch only changed fields in a copy of the existing row_data.
            let mut patched = candidate.row_data.clone();
            let bitmap_len = col_types.len().div_ceil(8);
            let bitmap = patched[..bitmap_len].to_vec();
            for &(col_pos, _) in &assignments {
                if candidate.values[col_pos] == new_values[col_pos] {
                    continue;
                }
                if let Some(loc) =
                    axiomdb_types::field_patch::compute_field_location_runtime(
                        &col_types,
                        col_pos,
                        &bitmap,
                        Some(&patched),
                    )
                {
                    let _ = axiomdb_types::field_patch::write_field(
                        &mut patched,
                        &loc,
                        &new_values[col_pos],
                    );
                }
            }
            patched
        } else {
            axiomdb_types::codec::encode_row(&new_values, &col_types)?
        };

        let old_image =
            axiomdb_wal::ClusteredRowImage::new(root_pid, candidate.row_header, &candidate.row_data);

        if pk_changed {
            let new_pk_key = crate::key_encoding::encode_index_key(
                &pk_col_positions
                    .iter()
                    .map(|&pos| new_values[pos].clone())
                    .collect::<Vec<_>>(),
            )?;

            if !axiomdb_storage::clustered_tree::delete_mark(
                storage,
                Some(root_pid),
                &candidate.pk_key,
                txn_id,
                &snap,
            )? {
                continue;
            }

            let delete_marked_header = axiomdb_storage::heap::RowHeader {
                txn_id_created: candidate.row_header.txn_id_created,
                txn_id_deleted: txn_id,
                row_version: candidate.row_header.row_version,
                _flags: candidate.row_header._flags,
            };
            let delete_mark_image = axiomdb_wal::ClusteredRowImage::new(
                root_pid,
                delete_marked_header,
                &candidate.row_data,
            );
            txn.record_clustered_delete_mark(
                conn_txn,
                resolved.def.id,
                &candidate.pk_key,
                &old_image,
                &delete_mark_image,
            )?;

            let new_header = axiomdb_storage::heap::RowHeader {
                txn_id_created: txn_id,
                txn_id_deleted: 0,
                row_version: 0,
                _flags: candidate.row_header._flags,
            };
            root_pid = axiomdb_storage::clustered_tree::insert_with_batch(
                storage,
                Some(&mut conn_txn.local_page_batch),
                Some(root_pid),
                &new_pk_key,
                &new_header,
                &new_row_data,
            )?;
            let inserted_image =
                axiomdb_wal::ClusteredRowImage::new(root_pid, new_header, &new_row_data);
            txn.record_clustered_insert(conn_txn, resolved.def.id, &new_pk_key, &inserted_image)?;

            for (idx, compiled_pred, layout, sec_root) in &secondary_layouts {
                apply_clustered_secondary_update(
                    idx,
                    compiled_pred.as_ref(),
                    layout,
                    sec_root,
                    root_pid,
                    &candidate.values,
                    &new_values,
                    storage,
                    txn,
                    conn_txn,
                    bloom,
                )?;
            }
        } else {
            let new_header = axiomdb_storage::heap::RowHeader {
                txn_id_created: txn_id,
                txn_id_deleted: 0,
                row_version: candidate.row_header.row_version.saturating_add(1),
                _flags: candidate.row_header._flags,
            };

            match axiomdb_storage::clustered_tree::update_in_place(
                storage,
                Some(root_pid),
                &candidate.pk_key,
                &new_row_data,
                txn_id,
                &snap,
            ) {
                Ok(true) => {
                    let new_image =
                        axiomdb_wal::ClusteredRowImage::new(root_pid, new_header, &new_row_data);
                    txn.record_clustered_update(
                        conn_txn,
                        resolved.def.id,
                        &candidate.pk_key,
                        &old_image,
                        &new_image,
                    )?;
                }
                Ok(false) => continue,
                Err(DbError::HeapPageFull { .. }) => {
                    if let Some(new_root) = axiomdb_storage::clustered_tree::update_with_relocation(
                        storage,
                        Some(root_pid),
                        &candidate.pk_key,
                        &new_row_data,
                        txn_id,
                        &snap,
                    )? {
                        let new_image = axiomdb_wal::ClusteredRowImage::new(
                            new_root,
                            new_header,
                            &new_row_data,
                        );
                        txn.record_clustered_update(
                            conn_txn,
                            resolved.def.id,
                            &candidate.pk_key,
                            &old_image,
                            &new_image,
                        )?;
                        root_pid = new_root;
                    } else {
                        continue;
                    }
                }
                Err(err) => return Err(err),
            }

            let any_sec_col_changed = secondary_layouts.iter().any(|(idx, _, _, _)| {
                idx.predicate.is_some()
                    || idx.columns.iter().any(|c| {
                        let pos = c.col_idx as usize;
                        candidate.values.get(pos) != new_values.get(pos)
                    })
            });
            if any_sec_col_changed {
                for (idx, compiled_pred, layout, sec_root) in &secondary_layouts {
                    apply_clustered_secondary_update(
                        idx,
                        compiled_pred.as_ref(),
                        layout,
                        sec_root,
                        root_pid,
                        &candidate.values,
                        &new_values,
                        storage,
                        txn,
                        conn_txn,
                        bloom,
                    )?;
                }
            }
        }

        changed_count += 1;
    }

    if root_pid != resolved.def.root_page_id {
        axiomdb_catalog::CatalogWriter::new(storage, txn, conn_txn)?
            .update_table_root(resolved.def.id, root_pid)?;
    }

    for (idx, _, _, sec_root) in &secondary_layouts {
        let current = sec_root.load(std::sync::atomic::Ordering::Acquire);
        if current != idx.root_page_id {
            axiomdb_catalog::CatalogWriter::new(storage, txn, conn_txn)?
                .update_index_root(idx.index_id, current)?;
        }
    }

    if changed_count > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, changed_count);
    }
    ctx.invalidate_all();

    Ok(QueryResult::Affected {
        count: matched_count,
        last_insert_id: None,
    })
}

fn apply_order_by_limit_to_clustered_update_candidates(
    mut candidates: Vec<ClusteredUpdateCandidate>,
    order_by: &[OrderByItem],
    limit: Option<&Expr>,
) -> Result<Vec<ClusteredUpdateCandidate>, DbError> {
    // Phase 11.11: Top-N optimization for clustered UPDATE.
    let k = if let Some(limit_expr) = limit {
        let n = eval(limit_expr, &[])?;
        match n {
            Value::Int(v) => Some(v.max(0) as usize),
            Value::BigInt(v) => Some(v.max(0) as usize),
            _ => {
                return Err(DbError::InvalidValue {
                    reason: "UPDATE … LIMIT must be an integer".into(),
                })
            }
        }
    } else {
        None
    };

    if !order_by.is_empty() {
        let mut sort_err: Option<DbError> = None;
        let cmp = |a: &ClusteredUpdateCandidate, b: &ClusteredUpdateCandidate| {
            if sort_err.is_some() {
                return std::cmp::Ordering::Equal;
            }
            match compare_rows_for_sort(&a.values, &b.values, order_by) {
                Ok(ord) => ord,
                Err(e) => {
                    sort_err = Some(e);
                    std::cmp::Ordering::Equal
                }
            }
        };

        if let Some(k) = k {
            let k = k.min(candidates.len());
            if k > 0 && k < candidates.len() {
                candidates.select_nth_unstable_by(k - 1, cmp);
                if let Some(e) = sort_err {
                    return Err(e);
                }
                candidates.truncate(k);
                candidates.sort_by(|a, b| {
                    compare_rows_for_sort(&a.values, &b.values, order_by)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            } else {
                candidates.sort_by(cmp);
                if let Some(e) = sort_err {
                    return Err(e);
                }
                candidates.truncate(k);
            }
        } else {
            candidates.sort_by(cmp);
            if let Some(e) = sort_err {
                return Err(e);
            }
        }
    } else if let Some(k) = k {
        candidates.truncate(k);
    }

    Ok(candidates)
}
