// ── PostgreSQL INSERT ... ON CONFLICT helpers ────────────────────────────────

enum OnConflictOutcome {
    Insert,
    Skip,
    Updated { row: Vec<Value> },
}

#[allow(clippy::too_many_arguments)]
fn apply_on_conflict_heap(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
    resolved: &axiomdb_catalog::ResolvedTable,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    secondary_indexes: &mut [axiomdb_catalog::IndexDef],
    compiled_preds: &[Option<Expr>],
    clause: &crate::ast::OnConflictClause,
    proposed_row: &[Value],
    touched: &mut std::collections::HashSet<RecordId>,
) -> Result<OnConflictOutcome, DbError> {
    let Some((old_rid, existing_row)) = find_on_conflict_heap(
        storage,
        txn,
        conn_txn,
        bloom,
        resolved,
        schema_cols,
        proposed_row,
        &clause.target_columns,
    )?
    else {
        return Ok(OnConflictOutcome::Insert);
    };

    match &clause.action {
        OnConflictAction::DoNothing => Ok(OnConflictOutcome::Skip),
        OnConflictAction::DoUpdate {
            assignments,
            where_clause,
        } => {
            if touched.contains(&old_rid) {
                return Err(DbError::InvalidValue {
                    reason: "ON CONFLICT DO UPDATE cannot affect the same row twice".into(),
                });
            }

            if let Some(where_expr) = where_clause {
                let v = eval_odku_assignment_rhs(where_expr, &existing_row, proposed_row)?;
                if !crate::eval::is_truthy(&v) {
                    return Ok(OnConflictOutcome::Skip);
                }
            }

            let mut new_row = existing_row.clone();
            for assignment in assignments {
                let target_idx = schema_cols
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&assignment.column))
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: assignment.column.clone(),
                        table: resolved.def.table_name.clone(),
                    })?;
                new_row[target_idx] =
                    eval_odku_assignment_rhs(&assignment.value, &existing_row, proposed_row)?;
            }

            enforce_text_constraints(schema_cols, &mut new_row)?;
            check_row_constraints_with_cols(
                &resolved.constraints,
                &new_row,
                &resolved.def.table_name,
                &resolved.columns,
            )?;
            if !resolved.foreign_keys.is_empty() {
                crate::fk_enforcement::check_fk_child_update(
                    &existing_row,
                    &new_row,
                    &resolved.foreign_keys,
                    storage,
                    txn,
                    conn_txn,
                    bloom,
                )?;
            }

            let parent_key_changed = {
                let snap = txn.active_snapshot(&*conn_txn);
                let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap)?;
                let parent_fks = reader.list_fk_constraints_referencing(resolved.def.id)?;
                parent_fks.iter().any(|fk| {
                    existing_row.get(fk.parent_col_idx as usize)
                        != new_row.get(fk.parent_col_idx as usize)
                })
            };
            if parent_key_changed {
                crate::fk_enforcement::enforce_fk_on_parent_update(
                    &[(old_rid, existing_row.clone())],
                    &[new_row.clone()],
                    resolved.def.id,
                    storage,
                    txn,
                    conn_txn,
                    bloom,
                )?;
            }

            if new_row == existing_row {
                touched.insert(old_rid);
                return Ok(OnConflictOutcome::Updated { row: existing_row });
            }

            let coerced_new = crate::table::coerce_values_with_ctx(
                new_row.clone(),
                schema_cols,
                ctx,
                1,
            )?;
            let new_rid = crate::table::TableEngine::update_row(
                storage,
                txn,
                conn_txn,
                &resolved.def,
                schema_cols,
                old_rid,
                coerced_new.clone(),
            )?;

            let compiled_index_exprs =
                crate::partial_index::compile_index_exprs(secondary_indexes, schema_cols)?;
            if !secondary_indexes.is_empty() {
                let update_pairs =
                    vec![(old_rid, existing_row.clone(), new_rid, coerced_new.clone())];
                let snap = txn.active_snapshot(&*conn_txn);
                apply_update_index_maintenance(
                    secondary_indexes,
                    compiled_preds,
                    &compiled_index_exprs,
                    &update_pairs,
                    storage,
                    txn,
                    conn_txn,
                    bloom,
                    snap,
                )?;
            }

            ctx.stats.on_rows_changed(resolved.def.id, 1);
            touched.insert(old_rid);
            touched.insert(new_rid);
            Ok(OnConflictOutcome::Updated { row: coerced_new })
        }
    }
}

/// Finds a visible heap row that conflicts with `proposed_row` according to
/// PostgreSQL `ON CONFLICT` target semantics.
///
/// `target_columns` empty means "any PRIMARY KEY / UNIQUE conflict" and is
/// only valid for `DO NOTHING`. A non-empty target restricts probing to the
/// matching unique/primary index column set.
fn find_on_conflict_heap(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    resolved: &axiomdb_catalog::ResolvedTable,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    proposed_row: &[Value],
    target_columns: &[String],
) -> Result<Option<(RecordId, Vec<Value>)>, DbError> {
    let snap = txn.active_snapshot(conn_txn);

    for idx in resolved.indexes.iter() {
        if !(idx.is_primary || idx.is_unique) || idx.is_fk_index || idx.columns.is_empty() {
            continue;
        }
        if !target_columns.is_empty()
            && !on_conflict_index_matches_target(idx, schema_cols, target_columns)
        {
            continue;
        }

        if idx.predicate.as_deref().is_some_and(|s| !s.is_empty()) {
            let compiled = crate::partial_index::compile_index_predicates(
                std::slice::from_ref(idx),
                schema_cols,
            )?;
            if let Some(Some(pred)) = compiled.first() {
                let v = crate::eval::eval(pred, proposed_row)?;
                if !crate::eval::is_truthy(&v) {
                    continue;
                }
            }
        }

        let mut key_vals = Vec::with_capacity(idx.columns.len());
        let mut any_null = false;
        for ic in &idx.columns {
            let v = proposed_row
                .get(ic.col_idx as usize)
                .cloned()
                .unwrap_or(Value::Null);
            if matches!(v, Value::Null) {
                any_null = true;
                break;
            }
            key_vals.push(v);
        }
        if any_null {
            continue;
        }

        let key = crate::key_encoding::encode_index_key(&key_vals)?;
        if !bloom.might_exist(idx.index_id, &key) {
            continue;
        }

        let Some(existing_rid) = BTree::lookup_in(storage, idx.root_page_id, &key)? else {
            continue;
        };
        if !HeapChain::is_slot_visible(
            storage,
            existing_rid.page_id,
            existing_rid.slot_id,
            snap.clone(),
        )? {
            continue;
        }

        let Some(bytes) =
            HeapChain::read_row(storage, existing_rid.page_id, existing_rid.slot_id)?
        else {
            continue;
        };
        let old_values = crate::table::decode_row_from_bytes(&bytes, schema_cols)?;
        return Ok(Some((existing_rid, old_values)));
    }

    Ok(None)
}

fn on_conflict_index_matches_target(
    idx: &axiomdb_catalog::IndexDef,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    target_columns: &[String],
) -> bool {
    if idx.columns.len() != target_columns.len() {
        return false;
    }
    idx.columns.iter().all(|ic| {
        if ic.expr.is_some() {
            return false;
        }
        let Some(col) = schema_cols.iter().find(|c| c.col_idx == ic.col_idx) else {
            return false;
        };
        target_columns
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&col.name))
    })
}
