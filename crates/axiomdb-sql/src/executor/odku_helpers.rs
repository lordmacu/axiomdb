// ── INSERT ... ON DUPLICATE KEY UPDATE — per-row executor helper ─────────────
//
// Design reference: MariaDB `sql/sql_insert.cc::write_record` DUP_UPDATE
// branch. AxiomDB uses proactive lookups (same as REPLACE) instead of the
// handler's retry-on-error loop because the conflict locator then owns the
// dual-row (existing + proposed) evaluation context.
//
// Spec: specs/fase-gap-audit/spec-insert-on-duplicate-key-update.md
// Plan: specs/fase-gap-audit/plan-insert-on-duplicate-key-update.md

/// Outcome of running the ODKU per-row helper.
enum OdkuOutcome {
    /// No conflict — the caller must proceed with the regular INSERT.
    Inserted,
    /// Conflict resolved via UPDATE; the row actually changed.
    /// `affected_rows += 2` (MySQL formula).
    UpdatedChanged,
    /// Conflict resolved via UPDATE; the new row equals the old row.
    /// `affected_rows += 0` (MySQL formula).
    UpdatedNoChange,
}

/// Routes a column reference in an ODKU UPDATE assignment to either the
/// existing row (plain `col`) or the proposed row (`VALUES(col)`).
fn eval_odku_assignment_rhs(
    expr: &Expr,
    existing_row: &[Value],
    proposed_row: &[Value],
) -> Result<Value, DbError> {
    match expr {
        Expr::Column { col_idx, .. } => {
            existing_row
                .get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: existing_row.len(),
                })
        }
        Expr::InsertValue { col_idx, .. } => {
            proposed_row
                .get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: proposed_row.len(),
                })
        }
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Default => Ok(Value::Null),
        Expr::UnaryOp { op, operand } => {
            let v = eval_odku_assignment_rhs(operand, existing_row, proposed_row)?;
            crate::eval::eval(
                &Expr::UnaryOp {
                    op: *op,
                    operand: Box::new(Expr::Literal(v)),
                },
                &[],
            )
        }
        Expr::BinaryOp { op, left, right } => {
            let l = eval_odku_assignment_rhs(left, existing_row, proposed_row)?;
            let r = eval_odku_assignment_rhs(right, existing_row, proposed_row)?;
            crate::eval::eval(
                &Expr::BinaryOp {
                    op: *op,
                    left: Box::new(Expr::Literal(l)),
                    right: Box::new(Expr::Literal(r)),
                },
                &[],
            )
        }
        Expr::IsNull { expr, negated } => {
            let v = eval_odku_assignment_rhs(expr, existing_row, proposed_row)?;
            let is_null = matches!(v, Value::Null);
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }
        Expr::Function { name, args } => {
            let arg_vals: Vec<Value> = args
                .iter()
                .map(|a| eval_odku_assignment_rhs(a, existing_row, proposed_row))
                .collect::<Result<_, _>>()?;
            // Route through the generic function dispatcher by fabricating
            // Literal args (evaluation already done).
            let lit_args: Vec<Expr> = arg_vals.into_iter().map(Expr::Literal).collect();
            crate::eval::eval(
                &Expr::Function {
                    name: name.clone(),
                    args: lit_args,
                },
                &[],
            )
        }
        // Falls back to the normal evaluator for expressions that cannot
        // mention InsertValue (arithmetic subtrees fully evaluate under
        // the two handled arms above). For anything else, evaluate against
        // the existing row — matches MariaDB's IN_UPDATE_ON_DUP_KEY scope
        // where VALUES() is the only exception to "refers to existing row".
        _ => crate::eval::eval(expr, existing_row),
    }
}

/// ODKU conflict-resolution pass for a single heap-table row.
///
/// If the proposed row would conflict with an existing row on a PRIMARY KEY
/// or UNIQUE index, the existing row is updated in place via the stored
/// assignment list; otherwise the helper returns `OdkuOutcome::Inserted`
/// and the caller performs the normal INSERT. MariaDB parity: the FIRST
/// conflicting unique index (catalog order) wins.
#[allow(clippy::too_many_arguments)]
fn apply_odku_heap(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
    resolved: &axiomdb_catalog::ResolvedTable,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    secondary_indexes: &mut [axiomdb_catalog::IndexDef],
    compiled_preds: &[Option<Expr>],
    assignments_resolved: &[(usize, Expr)],
    proposed_row: &[Value],
) -> Result<OdkuOutcome, DbError> {
    use axiomdb_index::BTree;
    use axiomdb_storage::heap_chain::HeapChain;

    let snap = txn.active_snapshot(&*conn_txn);

    // Find the FIRST conflicting row (MariaDB semantics: get_dup_key returns
    // only one index; later conflicts surface naturally through the UPDATE
    // path's index maintenance).
    let mut conflict: Option<(RecordId, Vec<Value>)> = None;
    for idx in resolved.indexes.iter() {
        if !(idx.is_primary || idx.is_unique) {
            continue;
        }
        if idx.is_fk_index || idx.columns.is_empty() {
            continue;
        }
        // Partial-index predicate — only conflict if the proposed row
        // satisfies the predicate.
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
        // Extract key values; MATCH SIMPLE — any NULL means no conflict.
        let mut key_vals: Vec<Value> = Vec::with_capacity(idx.columns.len());
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

        let row_bytes = HeapChain::read_row(storage, existing_rid.page_id, existing_rid.slot_id)?;
        let Some(bytes) = row_bytes else { continue };
        let old_values = crate::table::decode_row_from_bytes(&bytes, schema_cols)?;
        conflict = Some((existing_rid, old_values));
        break;
    }

    let Some((old_rid, existing_row)) = conflict else {
        return Ok(OdkuOutcome::Inserted);
    };

    // Apply the ODKU assignments. `Column` → existing row; `VALUES(col)`
    // → proposed row. Everything else follows the usual evaluator.
    let mut new_row = existing_row.clone();
    for (target_idx, rhs) in assignments_resolved {
        let v = eval_odku_assignment_rhs(rhs, &existing_row, proposed_row)?;
        if *target_idx < new_row.len() {
            new_row[*target_idx] = v;
        }
    }

    // Same constraint + FK pipeline as a plain UPDATE.
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

    // Parent-side enforcement when a referenced key moved (match UPDATE).
    let parent_key_changed = {
        let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap.clone())?;
        let parent_fks = reader.list_fk_constraints_referencing(resolved.def.id)?;
        parent_fks.iter().any(|fk| {
            existing_row.get(fk.parent_col_idx as usize) != new_row.get(fk.parent_col_idx as usize)
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

    // No-change detection — `affected_rows += 0` per MySQL spec.
    if new_row == existing_row {
        return Ok(OdkuOutcome::UpdatedNoChange);
    }

    // Perform the heap update (delete old + insert new; may change RID).
    let coerced_new = crate::table::coerce_values_with_ctx(new_row.clone(), schema_cols, ctx, 1)?;
    let new_rid = crate::table::TableEngine::update_row(
        storage,
        txn,
        conn_txn,
        &resolved.def,
        schema_cols,
        old_rid,
        coerced_new.clone(),
    )?;

    // Secondary-index maintenance — reuse the UPDATE executor's helper so
    // every edge case (partial indexes, FK auto-indexes, GIN, unique
    // re-check) is covered identically.
    let compiled_index_exprs =
        crate::partial_index::compile_index_exprs(secondary_indexes, schema_cols)?;
    if !secondary_indexes.is_empty() {
        let update_pairs = vec![(old_rid, existing_row.clone(), new_rid, coerced_new.clone())];
        apply_update_index_maintenance(
            secondary_indexes,
            compiled_preds,
            &compiled_index_exprs,
            &update_pairs,
            storage,
            txn,
            conn_txn,
            bloom,
            snap.clone(),
        )?;
    }

    ctx.stats.on_rows_changed(resolved.def.id, 1);
    Ok(OdkuOutcome::UpdatedChanged)
}

/// Pre-resolves the ODKU assignment list: turns each `Assignment { column,
/// value }` into `(col_idx, expr)` where both `Column` and `InsertValue`
/// names have been matched against the target schema.
fn resolve_odku_assignments(
    assignments: &[Assignment],
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    table_name: &str,
) -> Result<Vec<(usize, Expr)>, DbError> {
    let mut out = Vec::with_capacity(assignments.len());
    for a in assignments {
        let target_idx = schema_cols
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&a.column))
            .ok_or_else(|| DbError::ColumnNotFound {
                name: a.column.clone(),
                table: table_name.to_string(),
            })?;
        let rhs = resolve_odku_expr(a.value.clone(), schema_cols, table_name)?;
        out.push((target_idx, rhs));
    }
    Ok(out)
}

/// Resolves `Expr::Column` and `Expr::InsertValue` references in `expr`
/// against the target table's schema, preserving the distinction between
/// existing-row (Column) and proposed-row (InsertValue) references.
fn resolve_odku_expr(
    expr: Expr,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    table_name: &str,
) -> Result<Expr, DbError> {
    match expr {
        Expr::Column { col_idx: _, name } => {
            let idx = schema_cols
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(&name))
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: name.clone(),
                    table: table_name.to_string(),
                })?;
            Ok(Expr::Column { col_idx: idx, name })
        }
        Expr::InsertValue { col_idx: _, name } => {
            let idx = schema_cols
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(&name))
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: name.clone(),
                    table: format!("VALUES() in {table_name}"),
                })?;
            Ok(Expr::InsertValue { col_idx: idx, name })
        }
        Expr::UnaryOp { op, operand } => Ok(Expr::UnaryOp {
            op,
            operand: Box::new(resolve_odku_expr(*operand, schema_cols, table_name)?),
        }),
        Expr::BinaryOp { op, left, right } => Ok(Expr::BinaryOp {
            op,
            left: Box::new(resolve_odku_expr(*left, schema_cols, table_name)?),
            right: Box::new(resolve_odku_expr(*right, schema_cols, table_name)?),
        }),
        Expr::IsNull { expr, negated } => Ok(Expr::IsNull {
            expr: Box::new(resolve_odku_expr(*expr, schema_cols, table_name)?),
            negated,
        }),
        Expr::Function { name, args } => {
            let resolved_args: Result<Vec<_>, _> = args
                .into_iter()
                .map(|a| resolve_odku_expr(a, schema_cols, table_name))
                .collect();
            Ok(Expr::Function {
                name,
                args: resolved_args?,
            })
        }
        Expr::Cast { expr, target } => Ok(Expr::Cast {
            expr: Box::new(resolve_odku_expr(*expr, schema_cols, table_name)?),
            target,
        }),
        other => Ok(other),
    }
}
