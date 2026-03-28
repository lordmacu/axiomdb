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
        stmt.table.schema.as_deref(),
        &stmt.table.name,
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

    // Index maintenance: batch B-Tree delete per index (Phase 5.19).
    // Collect all delete keys per index in one pass, sort them, then call
    // delete_many_in once per index — O(N + height) instead of O(N log N).
    if !secondary_indexes.is_empty() {
        let compiled_preds =
            crate::partial_index::compile_index_predicates(&secondary_indexes, &schema_cols)?;
        let mut secondary_indexes = secondary_indexes; // shadow as mut for root sync
        let key_buckets = crate::index_maintenance::collect_delete_keys_by_index(
            &secondary_indexes,
            &to_delete,
            &compiled_preds,
        )?;
        let updated = crate::index_maintenance::delete_many_from_indexes(
            &mut secondary_indexes,
            key_buckets,
            storage,
            bloom,
        )?;
        let mut any_root_changed = false;
        for (index_id, new_root) in updated {
            CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
            any_root_changed = true;
        }
        if any_root_changed {
            ctx.invalidate_all();
        }
    }

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
            // Full heap scan — existing behavior.
            let rows = TableEngine::scan_table(storage, table_def, schema_cols, snap, None)?;
            rows.into_iter()
                .filter_map(|(rid, values)| match eval(where_clause, &values) {
                    Ok(v) if is_truthy(&v) => Some(Ok((rid, values))),
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect::<Result<_, DbError>>()
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
    let mut secondary_indexes: Vec<IndexDef> = resolved
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

    // Index maintenance: batch B-Tree delete per index (Phase 5.19).
    if !secondary_indexes.is_empty() {
        let compiled_preds =
            crate::partial_index::compile_index_predicates(&secondary_indexes, &schema_cols)?;
        let key_buckets = crate::index_maintenance::collect_delete_keys_by_index(
            &secondary_indexes,
            &to_delete,
            &compiled_preds,
        )?;
        let updated = crate::index_maintenance::delete_many_from_indexes(
            &mut secondary_indexes,
            key_buckets,
            storage,
            &mut noop_bloom,
        )?;
        for (index_id, new_root) in updated {
            CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
        }
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── CREATE TABLE ─────────────────────────────────────────────────────────────
