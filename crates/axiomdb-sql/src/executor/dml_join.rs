#[derive(Clone)]
struct DmlJoinRow {
    values: Row,
    target: Option<(RecordId, Vec<Value>)>,
}

#[derive(Clone)]
struct DmlJoinCandidate {
    rid: RecordId,
    target_values: Vec<Value>,
    combined_values: Row,
}

#[allow(clippy::too_many_arguments)]
fn execute_update_join_ctx(
    stmt: UpdateStmt,
    assignments: Vec<(usize, Expr)>,
    schema_cols: &[CatalogColumnDef],
    secondary_indexes: &[IndexDef],
    col_types: &[DataType],
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    snap: TransactionSnapshot,
    resolved: &ResolvedTable,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // Phase 13.9: immutable tables reject UPDATE at the executor layer.
    if resolved.def.immutable {
        return Err(DbError::ImmutableTable {
            table: resolved.def.table_name.clone(),
            operation: "UPDATE".into(),
        });
    }

    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let bloom = exec_ctx.bloom();

    let candidates = collect_dml_join_candidates_ctx(
        &stmt.table,
        &stmt.joins,
        None,
        stmt.where_clause.as_ref(),
        &stmt.order_by,
        stmt.limit.as_ref(),
        exec_ctx,
        Some(conn_txn),
        ctx,
    )?;

    let mut to_update: Vec<(RecordId, Vec<Value>, Vec<Value>)> = Vec::new();
    let mut matched_count = 0u64;
    for candidate in candidates {
        let current_values = if let Some(lm) = exec_ctx.lock_manager() {
            let result = lm.acquire_record_lock_sync(
                conn_txn.txn_id,
                candidate.rid.page_id,
                candidate.rid.slot_id,
                axiomdb_lock::LockMode::Exclusive,
                axiomdb_lock::LockFlags::REC_NOT_GAP,
            )?;
            if result == axiomdb_lock::LockResult::WaitGranted {
                match crate::table::reread_heap_row_if_visible(
                    storage,
                    candidate.rid,
                    &snap,
                    col_types,
                ) {
                    Some(refreshed) => refreshed,
                    None => continue,
                }
            } else {
                candidate.target_values
            }
        } else {
            candidate.target_values
        };

        matched_count += 1;
        let mut changed = false;
        let mut new_values = Vec::with_capacity(current_values.len());
        for (ci, cv) in current_values.iter().enumerate() {
            if let Some((_, val_expr)) = assignments.iter().find(|(pos, _)| *pos == ci) {
                let nv = eval(val_expr, &candidate.combined_values)?;
                if nv != *cv {
                    changed = true;
                }
                new_values.push(nv);
            } else {
                new_values.push(cv.clone());
            }
        }
        if changed {
            to_update.push((candidate.rid, current_values, new_values));
        }
    }

    for (_, _, new_values) in &mut to_update {
        enforce_text_constraints(&resolved.columns, new_values)?;
    }
    if !resolved.constraints.is_empty() {
        for (_, _, new_values) in &to_update {
            check_row_constraints_with_cols(
                &resolved.constraints,
                new_values,
                &resolved.def.table_name,
                &resolved.columns,
            )?;
        }
    }
    if !resolved.foreign_keys.is_empty() {
        for (_, old_values, new_values) in &to_update {
            crate::fk_enforcement::check_fk_child_update(
                old_values,
                new_values,
                &resolved.foreign_keys,
                storage,
                txn,
                conn_txn,
                bloom,
            )?;
        }
    }
    if !to_update.is_empty() {
        let old_rows: Vec<(RecordId, Vec<Value>)> = to_update
            .iter()
            .map(|(rid, old, _)| (*rid, old.clone()))
            .collect();
        let new_rows: Vec<Vec<Value>> = to_update.iter().map(|(_, _, new)| new.clone()).collect();
        crate::fk_enforcement::enforce_fk_on_parent_update(
            &old_rows,
            &new_rows,
            resolved.def.id,
            storage,
            txn,
            conn_txn,
            bloom,
        )?;
    }

    let compiled_preds =
        crate::partial_index::compile_index_predicates(secondary_indexes, schema_cols)?;
    let heap_updates: Vec<(RecordId, Vec<Value>)> = to_update
        .iter()
        .map(|(rid, _old, new)| (*rid, new.clone()))
        .collect();
    let new_rids = TableEngine::update_rows_preserve_rid_with_ctx(
        storage,
        txn,
        conn_txn,
        &resolved.def,
        schema_cols,
        ctx,
        heap_updates,
    )?;

    if !secondary_indexes.is_empty() && !to_update.is_empty() {
        let all_rids_stable = to_update
            .iter()
            .zip(new_rids.iter())
            .all(|((old_rid, _, _), new_rid)| old_rid == new_rid);
        let any_index_affected =
            statement_might_affect_indexes(secondary_indexes, &compiled_preds, &assignments);

        if any_index_affected || !all_rids_stable {
            let mut current_indexes = secondary_indexes.to_vec();
            let update_pairs: Vec<(RecordId, Vec<Value>, RecordId, Vec<Value>)> = to_update
                .into_iter()
                .zip(new_rids)
                .map(|((old_rid, old_values, new_values), new_rid)| {
                    (old_rid, old_values, new_rid, new_values)
                })
                .collect();
            apply_update_index_maintenance(
                &mut current_indexes,
                &compiled_preds,
                &update_pairs,
                storage,
                txn,
                conn_txn,
                bloom,
                snap,
            )?;
        }
    }

    if matched_count > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, matched_count);
        ctx.invalidate_all();
    }
    Ok(QueryResult::Affected {
        count: matched_count,
        last_insert_id: None,
    })
}

fn execute_delete_join_ctx(
    stmt: DeleteStmt,
    resolved: &ResolvedTable,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // Phase 13.9: immutable tables reject DELETE at the executor layer.
    if resolved.def.immutable {
        return Err(DbError::ImmutableTable {
            table: resolved.def.table_name.clone(),
            operation: "DELETE".into(),
        });
    }

    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let bloom = exec_ctx.bloom();
    let snap = txn.active_snapshot(conn_txn);
    let candidates = collect_dml_join_candidates_ctx(
        &stmt.table,
        &stmt.joins,
        stmt.target.as_deref(),
        stmt.where_clause.as_ref(),
        &stmt.order_by,
        stmt.limit.as_ref(),
        exec_ctx,
        Some(conn_txn),
        ctx,
    )?;

    if candidates.is_empty() {
        return Ok(QueryResult::Affected {
            count: 0,
            last_insert_id: None,
        });
    }

    let has_fk_references = {
        let mut reader = CatalogReader::new(storage, snap)?;
        !reader
            .list_fk_constraints_referencing(resolved.def.id)?
            .is_empty()
    };
    let to_delete: Vec<(RecordId, Vec<Value>)> = candidates
        .into_iter()
        .map(|candidate| (candidate.rid, candidate.target_values))
        .collect();

    if has_fk_references {
        crate::fk_enforcement::enforce_fk_on_parent_delete(
            &to_delete,
            resolved.def.id,
            storage,
            txn,
            conn_txn,
            bloom,
            0,
        )?;
    }

    let rids_only: Vec<RecordId> = to_delete.iter().map(|(rid, _)| *rid).collect();
    let count = TableEngine::delete_rows_batch(storage, txn, conn_txn, &resolved.def, &rids_only)?;
    if count > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, count);
    }
    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_dml_join_candidates_ctx(
    from_ref: &TableRef,
    joins: &[JoinClause],
    target_name: Option<&str>,
    where_clause: Option<&Expr>,
    order_by: &[OrderByItem],
    limit: Option<&Expr>,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<Vec<DmlJoinCandidate>, DbError> {
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let snap = conn_txn
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());

    let mut all_sources: Vec<JoinSourceSchema> = Vec::new();
    let mut scanned: Vec<Vec<DmlJoinRow>> = Vec::new();
    let mut col_offsets: Vec<usize> = Vec::new();
    // Phase 11.20d4: parallel tracker for LATERAL-correlated JSON_TABLE
    // right sources. None → right side is already in scanned[i]; Some(spec)
    // → placeholder scanned[i] and per-outer-row re-materialization.
    let mut correlated_jt: Vec<Option<crate::json_table::JsonTableSpec>> = vec![None];
    // Phase 11.25a: same for JSONB SRFs.
    let mut correlated_srf: Vec<Option<crate::ast::JsonbSrfKind>> = vec![None];
    let mut running_offset = 0usize;

    let from_t = resolve_table_cached(storage, txn, ctx, conn_txn, from_ref)?;
    let from_is_target = dml_source_matches_target(from_ref, &from_t, target_name, true);
    if from_is_target && from_t.def.is_clustered() {
        return Err(DbError::NotImplemented {
            feature: "multi-table UPDATE/DELETE JOIN on clustered target tables".into(),
        });
    }
    let from_rows =
        TableEngine::scan_table(storage, &from_t.def, &from_t.columns, snap.clone(), None)?;
    col_offsets.push(running_offset);
    running_offset += from_t.columns.len();
    all_sources.push(join_source_schema_from_resolved(from_ref, &from_t));
    scanned.push(dml_source_rows(from_rows, from_is_target));

    for join in joins {
        match &join.table {
            FromClause::Table(tref) => {
                let jt = resolve_table_cached(storage, txn, ctx, conn_txn, tref)?;
                let is_target = dml_source_matches_target(tref, &jt, target_name, false);
                if is_target && jt.def.is_clustered() {
                    return Err(DbError::NotImplemented {
                        feature: "multi-table UPDATE/DELETE JOIN on clustered target tables".into(),
                    });
                }
                let rows =
                    TableEngine::scan_table(storage, &jt.def, &jt.columns, snap.clone(), None)?;
                col_offsets.push(running_offset);
                running_offset += jt.columns.len();
                all_sources.push(join_source_schema_from_resolved(tref, &jt));
                scanned.push(dml_source_rows(rows, is_target));
                correlated_jt.push(None);
                correlated_srf.push(None);
            }
            FromClause::Subquery { query, alias, .. } => {
                let inner_result = execute_select_ctx((**query).clone(), exec_ctx, conn_txn, ctx)?;
                let (columns, rows) = match inner_result {
                    QueryResult::Rows { columns, rows } => (columns, rows),
                    _ => {
                        return Err(DbError::Internal {
                            message: "join-side subquery did not return rows".into(),
                        });
                    }
                };
                col_offsets.push(running_offset);
                running_offset += columns.len();
                all_sources.push(join_source_schema_from_derived(alias, columns));
                scanned.push(
                    rows.into_iter()
                        .map(|values| DmlJoinRow {
                            values,
                            target: None,
                        })
                        .collect(),
                );
                correlated_jt.push(None);
                correlated_srf.push(None);
            }
            // Phase 11.20d4 — JSON_TABLE as DML right-side source.
            FromClause::JsonTable(jt) => {
                let spec = crate::json_table::compile_json_table(jt)?;
                let column_metas = crate::json_table::column_metas_for_spec(&spec);
                col_offsets.push(running_offset);
                running_offset += column_metas.len();
                all_sources.push(join_source_schema_from_derived(&spec.alias, column_metas));
                if crate::json_table::jsontable_is_correlated(jt) {
                    scanned.push(Vec::new());
                    correlated_jt.push(Some(spec));
                } else {
                    let doc_val = crate::eval::eval(&jt.doc, &[])?;
                    let rows = match crate::json_table::doc_to_serde(&doc_val)? {
                        None => Vec::new(),
                        Some(sj) => {
                            let mut runner = crate::eval::NoSubquery;
                            crate::json_table::materialize_json_table(&spec, &sj, &[], &mut runner)?
                        }
                    };
                    scanned.push(
                        rows.into_iter()
                            .map(|values| DmlJoinRow {
                                values,
                                target: None,
                            })
                            .collect(),
                    );
                    correlated_jt.push(None);
                    correlated_srf.push(None);
                }
            }
            // Phase 11.25a — JSONB SRF as DML JOIN right-side source.
            FromClause::JsonbSrf(srf) => {
                let alias = crate::jsonb_srf::srf_alias(srf);
                let column_metas = crate::jsonb_srf::column_metas_for_srf(srf.kind, &alias);
                col_offsets.push(running_offset);
                running_offset += column_metas.len();
                all_sources.push(join_source_schema_from_derived(&alias, column_metas));
                if crate::jsonb_srf::srf_is_correlated(srf) {
                    scanned.push(Vec::new());
                    correlated_jt.push(None);
                    correlated_srf.push(Some(srf.kind));
                } else {
                    let doc_val = crate::eval::eval(&srf.doc, &[])?;
                    let rows = crate::jsonb_srf::materialize_jsonb_srf(srf.kind, &doc_val)?;
                    scanned.push(
                        rows.into_iter()
                            .map(|values| DmlJoinRow {
                                values,
                                target: None,
                            })
                            .collect(),
                    );
                    correlated_jt.push(None);
                    correlated_srf.push(None);
                }
            }
            // Phase 21.22 — inline VALUES as DML JOIN right-side source.
            FromClause::Values(vc) => {
                let column_metas = crate::values_clause::column_metas_for_values(vc);
                let rows = crate::values_clause::materialize_values(vc)?;
                col_offsets.push(running_offset);
                running_offset += column_metas.len();
                all_sources.push(join_source_schema_from_derived(&vc.alias, column_metas));
                scanned.push(
                    rows.into_iter()
                        .map(|values| DmlJoinRow {
                            values,
                            target: None,
                        })
                        .collect(),
                );
                correlated_jt.push(None);
                correlated_srf.push(None);
            }
            // Phase 21.3 — recursive CTE as DML join source deferred.
            FromClause::RecursiveCte(_) => {
                return Err(DbError::NotImplemented {
                    feature: "recursive CTE as UPDATE/DELETE JOIN source deferred".into(),
                });
            }
        }
    }

    let mut combined_rows = scanned.first().cloned().unwrap_or_default();
    let mut left_col_count = all_sources[0].columns.len();
    let mut left_schema: Vec<(String, usize)> = all_sources[0]
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| (col.name.clone(), i))
        .collect();

    for (i, join) in joins.iter().enumerate() {
        let right_idx = i + 1;
        let right_col_count = all_sources[right_idx].columns.len();
        let right_col_offset = col_offsets[right_idx];
        combined_rows = if let Some(spec) = correlated_jt[right_idx].as_ref() {
            let jt_ast = match &joins[i].table {
                FromClause::JsonTable(j) => j.as_ref(),
                _ => unreachable!("correlated_jt set but AST is not JsonTable"),
            };
            apply_correlated_jt_dml_join(
                combined_rows,
                jt_ast,
                spec,
                &all_sources[right_idx].columns,
                right_col_count,
                join.join_type,
                &join.condition,
                &left_schema,
                right_col_offset,
            )?
        } else if let Some(kind) = correlated_srf[right_idx] {
            let srf_ast = match &joins[i].table {
                FromClause::JsonbSrf(s) => s.as_ref(),
                _ => unreachable!("correlated_srf set but AST is not JsonbSrf"),
            };
            apply_correlated_srf_dml_join(
                combined_rows,
                kind,
                &srf_ast.doc,
                &all_sources[right_idx].columns,
                right_col_count,
                join.join_type,
                &join.condition,
                &left_schema,
                right_col_offset,
            )?
        } else {
            apply_dml_join(
                combined_rows,
                &scanned[right_idx],
                left_col_count,
                right_col_count,
                join.join_type,
                &join.condition,
                &left_schema,
                right_col_offset,
                &all_sources[right_idx].columns,
            )?
        };

        for (j, col) in all_sources[right_idx].columns.iter().enumerate() {
            left_schema.push((col.name.clone(), right_col_offset + j));
        }
        left_col_count += right_col_count;
    }

    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in combined_rows {
        if let Some(wc) = where_clause {
            if !is_truthy(&eval(wc, &row.values)?) {
                continue;
            }
        }
        if let Some((rid, target_values)) = row.target {
            if seen.insert((rid.page_id, rid.slot_id)) {
                candidates.push(DmlJoinCandidate {
                    rid,
                    target_values,
                    combined_values: row.values,
                });
            }
        }
    }

    apply_order_by_limit_to_dml_join_candidates(candidates, order_by, limit)
}

fn dml_source_matches_target(
    table_ref: &TableRef,
    resolved: &ResolvedTable,
    target_name: Option<&str>,
    default_target: bool,
) -> bool {
    match target_name {
        Some(target) => {
            table_ref
                .alias
                .as_deref()
                .map(|alias| alias.eq_ignore_ascii_case(target))
                .unwrap_or(false)
                || table_ref.name.eq_ignore_ascii_case(target)
                || resolved.def.table_name.eq_ignore_ascii_case(target)
        }
        None => default_target,
    }
}

fn dml_source_rows(rows: Vec<(RecordId, Vec<Value>)>, is_target: bool) -> Vec<DmlJoinRow> {
    rows.into_iter()
        .map(|(rid, values)| {
            let target = if is_target {
                Some((rid, values.clone()))
            } else {
                None
            };
            DmlJoinRow { values, target }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn apply_dml_join(
    left_rows: Vec<DmlJoinRow>,
    right_rows: &[DmlJoinRow],
    left_col_count: usize,
    right_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    left_schema: &[(String, usize)],
    right_col_offset: usize,
    right_columns: &[ColumnMeta],
) -> Result<Vec<DmlJoinRow>, DbError> {
    match join_type {
        JoinType::Inner | JoinType::Cross => {
            let mut result = Vec::new();
            for left in &left_rows {
                for right in right_rows {
                    let combined = concat_dml_join_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined.values,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                    }
                }
            }
            Ok(result)
        }
        JoinType::Left => {
            let null_right = DmlJoinRow {
                values: vec![Value::Null; right_col_count],
                target: None,
            };
            let mut result = Vec::new();
            for left in &left_rows {
                let mut matched = false;
                for right in right_rows {
                    let combined = concat_dml_join_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined.values,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched = true;
                    }
                }
                if !matched {
                    result.push(concat_dml_join_rows(left, &null_right));
                }
            }
            Ok(result)
        }
        JoinType::Right => {
            let null_left = DmlJoinRow {
                values: vec![Value::Null; left_col_count],
                target: None,
            };
            let mut matched_right = vec![false; right_rows.len()];
            let mut result = Vec::new();
            for left in &left_rows {
                for (i, right) in right_rows.iter().enumerate() {
                    let combined = concat_dml_join_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined.values,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched_right[i] = true;
                    }
                }
            }
            for (i, right) in right_rows.iter().enumerate() {
                if !matched_right[i] {
                    result.push(concat_dml_join_rows(&null_left, right));
                }
            }
            Ok(result)
        }
        JoinType::Full => {
            let null_left = DmlJoinRow {
                values: vec![Value::Null; left_col_count],
                target: None,
            };
            let null_right = DmlJoinRow {
                values: vec![Value::Null; right_col_count],
                target: None,
            };
            let mut matched_right = vec![false; right_rows.len()];
            let mut result = Vec::new();
            for left in &left_rows {
                let mut matched = false;
                for (i, right) in right_rows.iter().enumerate() {
                    let combined = concat_dml_join_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined.values,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched = true;
                        matched_right[i] = true;
                    }
                }
                if !matched {
                    result.push(concat_dml_join_rows(left, &null_right));
                }
            }
            for (i, right) in right_rows.iter().enumerate() {
                if !matched_right[i] {
                    result.push(concat_dml_join_rows(&null_left, right));
                }
            }
            Ok(result)
        }
    }
}

fn concat_dml_join_rows(left: &DmlJoinRow, right: &DmlJoinRow) -> DmlJoinRow {
    let mut values = Vec::with_capacity(left.values.len() + right.values.len());
    values.extend_from_slice(&left.values);
    values.extend_from_slice(&right.values);
    DmlJoinRow {
        values,
        target: left.target.clone().or_else(|| right.target.clone()),
    }
}

fn apply_order_by_limit_to_dml_join_candidates(
    mut candidates: Vec<DmlJoinCandidate>,
    order_by: &[OrderByItem],
    limit: Option<&Expr>,
) -> Result<Vec<DmlJoinCandidate>, DbError> {
    if !order_by.is_empty() {
        let mut sort_err: Option<DbError> = None;
        candidates.sort_by(|a, b| {
            if sort_err.is_some() {
                return std::cmp::Ordering::Equal;
            }
            match compare_rows_for_sort(&a.combined_values, &b.combined_values, order_by) {
                Ok(ord) => ord,
                Err(e) => {
                    sort_err = Some(e);
                    std::cmp::Ordering::Equal
                }
            }
        });
        if let Some(e) = sort_err {
            return Err(e);
        }
    }

    if let Some(limit_expr) = limit {
        let n = eval(limit_expr, &[])?;
        let limit_n = match n {
            Value::Int(v) => v.max(0) as usize,
            Value::BigInt(v) => v.max(0) as usize,
            _ => {
                return Err(DbError::InvalidValue {
                    reason: "ORDER BY … LIMIT must be an integer".into(),
                })
            }
        };
        candidates.truncate(limit_n);
    }
    Ok(candidates)
}

/// Phase 11.20d4 — DML variant of `apply_correlated_jt_join`. A
/// LATERAL-correlated JSON_TABLE on the right side of a multi-table
/// UPDATE/DELETE JOIN is re-materialized per outer row. Outer rows
/// come as `DmlJoinRow` (carry a `target` RID for the modifiable
/// table); JT right rows have `target = None` and contribute only
/// values. `concat_dml_join_rows` already carries the target from
/// the left operand, so the combined row points to the correct
/// modifiable RID.
#[allow(clippy::too_many_arguments)]
fn apply_correlated_jt_dml_join(
    left_rows: Vec<DmlJoinRow>,
    jt_ast: &crate::ast::JsonTable,
    spec: &crate::json_table::JsonTableSpec,
    right_columns: &[ColumnMeta],
    right_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    left_schema: &[(String, usize)],
    right_col_offset: usize,
) -> Result<Vec<DmlJoinRow>, DbError> {
    if matches!(join_type, JoinType::Right | JoinType::Full) {
        return Err(DbError::NotImplemented {
            feature: "RIGHT/FULL JOIN on LATERAL-correlated JSON_TABLE in \
                      UPDATE/DELETE — PG-compatible rejection"
                .into(),
        });
    }

    let null_right = DmlJoinRow {
        values: vec![Value::Null; right_col_count],
        target: None,
    };
    let mut out: Vec<DmlJoinRow> = Vec::with_capacity(left_rows.len());

    for outer in &left_rows {
        let doc_val = crate::eval::eval(&jt_ast.doc, &outer.values)?;
        let rows = match crate::json_table::doc_to_serde(&doc_val)? {
            None => Vec::new(),
            Some(sj) => {
                let mut runner = crate::eval::NoSubquery;
                crate::json_table::materialize_json_table(spec, &sj, &outer.values, &mut runner)?
            }
        };

        let mut matched = false;
        for values in &rows {
            let right = DmlJoinRow {
                values: values.clone(),
                target: None,
            };
            let combined = concat_dml_join_rows(outer, &right);
            if eval_join_cond(
                condition,
                &combined.values,
                left_schema,
                right_col_offset,
                right_columns,
            )? {
                out.push(combined);
                matched = true;
            }
        }

        if !matched && matches!(join_type, JoinType::Left) {
            out.push(concat_dml_join_rows(outer, &null_right));
        }
    }

    Ok(out)
}

/// Phase 11.25a — DML variant of `apply_correlated_srf_join`. Per-outer-row
/// re-materialization of a correlated JSONB SRF on the right side of an
/// UPDATE/DELETE JOIN. `DmlJoinRow` propagates the target RID from the
/// outer; SRF rows contribute values only (target=None).
#[allow(clippy::too_many_arguments)]
fn apply_correlated_srf_dml_join(
    left_rows: Vec<DmlJoinRow>,
    kind: crate::ast::JsonbSrfKind,
    doc_expr: &crate::expr::Expr,
    right_columns: &[ColumnMeta],
    right_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    left_schema: &[(String, usize)],
    right_col_offset: usize,
) -> Result<Vec<DmlJoinRow>, DbError> {
    if matches!(join_type, JoinType::Right | JoinType::Full) {
        return Err(DbError::NotImplemented {
            feature: "RIGHT/FULL JOIN on correlated JSONB SRF in UPDATE/DELETE \
                      — PG-compatible rejection"
                .into(),
        });
    }
    let null_right = DmlJoinRow {
        values: vec![Value::Null; right_col_count],
        target: None,
    };
    let mut out: Vec<DmlJoinRow> = Vec::with_capacity(left_rows.len());
    for outer in &left_rows {
        let doc_val = crate::eval::eval(doc_expr, &outer.values)?;
        let rows = crate::jsonb_srf::materialize_jsonb_srf(kind, &doc_val)?;
        let mut matched = false;
        for values in &rows {
            let right = DmlJoinRow {
                values: values.clone(),
                target: None,
            };
            let combined = concat_dml_join_rows(outer, &right);
            if eval_join_cond(
                condition,
                &combined.values,
                left_schema,
                right_col_offset,
                right_columns,
            )? {
                out.push(combined);
                matched = true;
            }
        }
        if !matched && matches!(join_type, JoinType::Left) {
            out.push(concat_dml_join_rows(outer, &null_right));
        }
    }
    Ok(out)
}
