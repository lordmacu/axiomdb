fn execute_delete_ctx(
    stmt: DeleteStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let resolved = resolve_table_cached(
        storage,
        txn,
        ctx,
        &stmt.table,
    )?;

    let secondary_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    let snap = txn.active_snapshot()?;

    // Check if any FK constraint references THIS table as the parent.
    // If so, we must scan rows (to get parent key values) and cannot use the fast path.
    let has_fk_references = {
        let mut reader = CatalogReader::new(storage, snap)?;
        !reader
            .list_fk_constraints_referencing(resolved.def.id)?
            .is_empty()
    };

    // No-WHERE + no FK parent references → bulk-empty fast path (Phase 5.16).
    // This replaces the old "no secondary indexes" gate: PK + UNIQUE + composite
    // indexes are all handled by root rotation, not per-row B-Tree deletes.
    if stmt.where_clause.is_none() && !has_fk_references {
        // Collect all indexes with columns (PK, UNIQUE, non-unique, FK auto-indexes).
        let all_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
            .indexes
            .iter()
            .filter(|i| !i.columns.is_empty())
            .cloned()
            .collect();

        let plan = plan_bulk_empty_table(storage, &resolved.def, &all_indexes, snap)?;
        let count = plan.visible_row_count;

        apply_bulk_empty_table(storage, txn, bloom, &resolved.def, plan)?;

        // Invalidate session schema cache so the next query reloads the new roots.
        ctx.invalidate_all();

        // Release deferred pages now if we're in immediate-commit mode.
        // In group-commit mode this is handled by the CommitCoordinator.
        // We use a best-effort release here; group-commit path does not hold
        // an active txn at this point, so active_txn_id() == None.
        if let Some(committed_txn_id) = txn.active_txn_id() {
            // Still inside an explicit transaction — pages freed at outer COMMIT.
            let _ = committed_txn_id; // suppress unused warning
        }

        return Ok(QueryResult::Affected {
            count,
            last_insert_id: None,
        });
    }

    // Candidate discovery (Phase 6.3b): use index when predicate is sargable.
    let schema_cols = resolved.columns.clone();
    let to_delete: Vec<(RecordId, Vec<Value>)> = if let Some(ref wc) = stmt.where_clause {
        let effective_coll = ctx.effective_collation();
        let delete_access = crate::planner::plan_delete_candidates_ctx(
            wc,
            &secondary_indexes,
            &schema_cols,
            effective_coll,
        );
        collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &delete_access,
            storage,
            snap,
            &resolved.def,
            bloom,
        )?
    } else {
        // No WHERE and has_fk_references=true (bulk-empty path already returned
        // for the no-WHERE + no-FK case). Full scan: all rows qualify.
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?
    };

    // FK parent enforcement: must run BEFORE heap delete so RESTRICT can abort
    // cleanly and CASCADE/SET NULL can still read/update child rows.
    if has_fk_references && !to_delete.is_empty() {
        crate::fk_enforcement::enforce_fk_on_parent_delete(
            &to_delete,
            resolved.def.id,
            storage,
            txn,
            bloom,
            0,
        )?;
    }

    // Batch-delete from heap: each page read+written once instead of 3× per row.
    let rids_only: Vec<RecordId> = to_delete.iter().map(|(rid, _)| *rid).collect();
    let count = TableEngine::delete_rows_batch(storage, txn, &resolved.def, &rids_only)?;

    // MVCC deferred index deletion (PostgreSQL model): ALL index entries are left
    // in place during DELETE — PK, UNIQUE, FK auto-indexes, and non-unique alike.
    // Dead entries are filtered by heap visibility checks on read (7.3b).
    // VACUUM (7.11) cleans dead entries from all index types.
    //
    // Why safe for PK/UNIQUE: has_visible_duplicate() checks heap visibility
    // before raising UniqueViolation, so INSERT after DELETE of same key works.
    //
    // Why safe for FK auto-indexes: FK enforcement now checks heap visibility
    // (is_slot_visible) before raising ForeignKeyParentViolation.

    // Track row changes for stats staleness (Phase 6.11).
    if count > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, count);
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── Column mask for lazy decode ───────────────────────────────────────────────

/// Builds a boolean mask over `n_cols` columns. `mask[i]` is `true` if column
/// `i` is referenced by any expression in `exprs`. Used by `execute_select_ctx`
/// and `execute_delete_ctx` to tell `scan_table` which columns to decode.
///
/// Conservative: any [`SelectItem::Wildcard`] or [`SelectItem::QualifiedWildcard`]
/// in the query's SELECT list will cause the caller to pass `None` instead (full
/// decode), so this function is only called when the select list is fully
/// resolved to column expressions.
/// Collect column indices referenced in an expression into a boolean mask.
/// Used to build the two-phase decode mask: only decode WHERE columns first.
fn collect_where_columns(e: &Expr, mask: &mut [bool]) {
    match e {
        Expr::Column { col_idx, .. } => {
            if *col_idx < mask.len() {
                mask[*col_idx] = true;
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_where_columns(left, mask);
            collect_where_columns(right, mask);
        }
        Expr::UnaryOp { operand, .. } => {
            collect_where_columns(operand, mask);
        }
        Expr::Function { args, .. } => {
            for arg in args {
                collect_where_columns(arg, mask);
            }
        }
        _ => {}
    }
}

fn collect_delete_candidates(
    where_clause: &Expr,
    _indexes: &[axiomdb_catalog::IndexDef],
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    access: &crate::planner::AccessMethod,
    storage: &mut dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    table_def: &axiomdb_catalog::TableDef,
    bloom: &crate::bloom::BloomRegistry,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    use crate::planner::AccessMethod;

    match access {
        AccessMethod::Scan | AccessMethod::IndexOnlyScan { .. } => {
            // Optimized scan: compile a BatchPredicate for zero-alloc raw-byte
            // predicate evaluation, zone map page skipping, and two-phase decode
            // (only decode WHERE columns first, full decode only for matching rows).
            // Mirrors the SELECT path in select.rs for parity.
            let col_types: Vec<axiomdb_types::DataType> = schema_cols
                .iter()
                .map(|c| crate::table::column_type_to_data_type(c.col_type))
                .collect();
            let batch_pred = crate::eval::batch::try_compile(where_clause, &col_types);
            let zm_pred =
                crate::planner::extract_zone_map_predicate(where_clause, schema_cols);

            // Build WHERE column mask for two-phase decode: only decode columns
            // referenced in the WHERE clause first, skip full decode for non-matching rows.
            let n_cols = schema_cols.len();
            let where_col_mask = {
                let mut mask = vec![false; n_cols];
                collect_where_columns(where_clause, &mut mask);
                if mask.iter().filter(|&&b| b).count() < n_cols {
                    Some(mask)
                } else {
                    None
                }
            };

            TableEngine::scan_table_filtered(
                storage,
                table_def,
                schema_cols,
                snap,
                |values| match eval(where_clause, values) {
                    Ok(v) => is_truthy(&v),
                    Err(_) => true, // include on error, let caller handle
                },
                zm_pred.as_ref().map(|(ci, p)| (*ci, p)),
                where_col_mask.as_deref(),
                batch_pred.as_ref(),
            )
        }

        AccessMethod::IndexLookup { index_def, key } => {
            // Point lookup via B-Tree → batch heap read → WHERE recheck.
            let candidate_rids: Vec<RecordId> = if index_def.is_unique {
                if index_def.is_unique && !bloom.might_exist(index_def.index_id, key) {
                    vec![]
                } else {
                    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                        None => vec![],
                        Some(rid) => vec![rid],
                    }
                }
            } else {
                // Non-unique: key||RID format — range [key||0..0, key||FF..FF].
                let lo = rid_lo(key);
                let hi = rid_hi(key);
                BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?
                    .into_iter()
                    .map(|(rid, _)| rid)
                    .collect()
            };

            let batch_rows =
                TableEngine::read_rows_batch(storage, schema_cols, &candidate_rids)?;
            let mut result = Vec::with_capacity(candidate_rids.len());
            for (rid, maybe_values) in candidate_rids.into_iter().zip(batch_rows) {
                if let Some(values) = maybe_values {
                    if is_truthy(&eval(where_clause, &values)?) {
                        result.push((rid, values));
                    }
                }
            }
            Ok(result)
        }

        AccessMethod::IndexRange { index_def, lo, hi } => {
            // Range scan via B-Tree → batch heap read → WHERE recheck.
            let (lo_adj, hi_adj);
            let (lo_ref, hi_ref) = if index_def.is_unique {
                (lo.as_deref(), hi.as_deref())
            } else {
                lo_adj = lo.as_deref().map(rid_lo);
                hi_adj = hi.as_deref().map(rid_hi);
                (lo_adj.as_deref(), hi_adj.as_deref())
            };
            let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;

            let candidate_rids: Vec<RecordId> = pairs.into_iter().map(|(rid, _)| rid).collect();
            let batch_rows =
                TableEngine::read_rows_batch(storage, schema_cols, &candidate_rids)?;
            let mut result = Vec::with_capacity(candidate_rids.len());
            for (rid, maybe_values) in candidate_rids.into_iter().zip(batch_rows) {
                if let Some(values) = maybe_values {
                    if is_truthy(&eval(where_clause, &values)?) {
                        result.push((rid, values));
                    }
                }
            }
            Ok(result)
        }
    }
}

fn execute_delete(
    stmt: DeleteStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let snap = txn.active_snapshot()?;

    // Use the already-loaded indexes from the resolved table (cached by SchemaCache).
    // Must be `mut` so we can keep root_page_id in sync as rows are deleted.
    let secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    // No-op bloom for the non-ctx path (bloom is managed by execute_with_ctx callers).
    let mut noop_bloom = crate::bloom::BloomRegistry::new();

    // Check if any FK constraint references THIS table as the parent.
    // If so, fall through to the row-by-row path so RESTRICT/CASCADE still fires.
    let has_fk_references = {
        let mut reader = CatalogReader::new(storage, snap)?;
        !reader
            .list_fk_constraints_referencing(resolved.def.id)?
            .is_empty()
    };

    // No-WHERE + no parent-FK references → bulk-empty fast path (Phase 5.16).
    if stmt.where_clause.is_none() && !has_fk_references {
        let plan = plan_bulk_empty_table(storage, &resolved.def, &secondary_indexes, snap)?;
        let count = plan.visible_row_count;
        apply_bulk_empty_table(storage, txn, &mut noop_bloom, &resolved.def, plan)?;
        return Ok(QueryResult::Affected {
            count,
            last_insert_id: None,
        });
    }

    // Candidate discovery (Phase 6.3b): index path when predicate is sargable.
    let schema_cols = resolved.columns.clone();
    let to_delete: Vec<(RecordId, Vec<Value>)> = if let Some(ref wc) = stmt.where_clause {
        let delete_access =
            crate::planner::plan_delete_candidates(wc, &secondary_indexes, &schema_cols);
        collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &delete_access,
            storage,
            snap,
            &resolved.def,
            &noop_bloom,
        )?
    } else {
        // No WHERE + has_fk_references=true — full scan, all rows qualify.
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?
    };

    // Batch-delete from heap: each page read+written once instead of 3× per row.
    let rids_only: Vec<RecordId> = to_delete.iter().map(|(rid, _)| *rid).collect();
    let count = TableEngine::delete_rows_batch(storage, txn, &resolved.def, &rids_only)?;

    // MVCC deferred index deletion (PostgreSQL model): all index entries left in
    // place. Dead entries filtered by heap visibility. VACUUM cleans later.

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── CREATE TABLE ─────────────────────────────────────────────────────────────
