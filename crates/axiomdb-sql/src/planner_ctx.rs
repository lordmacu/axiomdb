// ── Session-aware planner entry point ────────────────────────────────────────

/// Session-collation-aware wrapper around [`plan_select`].
///
/// When `collation` is non-binary (e.g. `Es`), any candidate access method
/// whose correctness depends on binary text ordering for a TEXT column is
/// rejected and replaced with a full [`AccessMethod::Scan`].
///
/// This prevents silently missing rows when the session fold (`es`) does not
/// match the binary key order stored in the index.
///
/// Non-text indexes and non-text predicates are unaffected.
pub fn plan_select_ctx(
    where_clause: Option<&Expr>,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    table_id: u32,
    table_stats: &[StatsDef],
    stale_tracker: &mut StaleStatsTracker,
    select_col_idxs: &[u16],
    collation: SessionCollation,
) -> AccessMethod {
    let am = plan_select(
        where_clause,
        indexes,
        columns,
        table_id,
        table_stats,
        stale_tracker,
        select_col_idxs,
    );

    if collation == SessionCollation::Binary {
        return am;
    }

    // Reject any index access method that touches a TEXT column key.
    // The check is conservative: if the leading index column is TEXT, fall back.
    let text_col_idxs: std::collections::HashSet<u16> = columns
        .iter()
        .filter(|col| col.col_type == ColumnType::Text)
        .map(|col| col.col_idx)
        .collect();

    let uses_text_index = match &am {
        AccessMethod::IndexLookup { index_def, .. }
        | AccessMethod::IndexRange { index_def, .. }
        | AccessMethod::IndexOnlyScan { index_def, .. } => index_def
            .columns
            .first()
            .map(|c| text_col_idxs.contains(&c.col_idx))
            .unwrap_or(false),
        // GIN uses specialised JSONB term encoding — not affected by text collation.
        AccessMethod::GinScan { .. } | AccessMethod::Scan => false,
    };

    if uses_text_index {
        AccessMethod::Scan
    } else {
        am
    }
}

/// Re-plans a single-table SELECT against a hinted index when one was supplied.
///
/// If the named index exists and is compatible with the query predicate, the
/// hinted access method is returned. If the named index exists but is not
/// compatible with the predicate, the original `base` plan is kept.
#[allow(clippy::too_many_arguments)]
pub fn apply_select_index_hint_ctx(
    base: AccessMethod,
    hinted_index_name: &str,
    where_clause: Option<&Expr>,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    table_id: u32,
    table_stats: &[StatsDef],
    stale_tracker: &mut StaleStatsTracker,
    select_col_idxs: &[u16],
    collation: SessionCollation,
) -> Result<AccessMethod, DbError> {
    let hinted_index = indexes
        .iter()
        .find(|idx| idx.name.eq_ignore_ascii_case(hinted_index_name))
        .ok_or_else(|| DbError::IndexNotFound {
            name: hinted_index_name.to_string(),
        })?;

    let hinted = plan_select_ctx(
        where_clause,
        std::slice::from_ref(hinted_index),
        columns,
        table_id,
        table_stats,
        stale_tracker,
        select_col_idxs,
        collation,
    );

    Ok(match hinted {
        AccessMethod::Scan => base,
        other => other,
    })
}

// ── DELETE-specific candidate planner (Phase 6.3b) ───────────────────────────

/// Chooses the best index-access method for discovering DELETE candidate rows.
///
/// Unlike [`plan_select`] this planner:
/// - **never applies `stats_cost_gate`** — avoiding a full heap scan is always
///   beneficial for DELETE, even when the predicate matches many rows.
/// - **never returns `IndexOnlyScan`** — DELETE always needs full row values for
///   `WHERE` recheck, FK enforcement, and secondary-index maintenance.
/// - **does not require `select_col_idxs`** — DELETE has no projection list.
///
/// Returns [`AccessMethod::Scan`] when no usable index exists.
pub fn plan_delete_candidates(
    where_clause: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
) -> AccessMethod {
    use crate::key_encoding::encode_index_key;

    // Rule 0: composite equality on multiple columns (preferred over single).
    if let Some(am) = plan_composite_eq(where_clause, indexes, columns) {
        // Never return IndexOnlyScan for DELETE — full rows always needed.
        return match am {
            AccessMethod::IndexOnlyScan {
                index_def, lo, hi, ..
            } => AccessMethod::IndexRange {
                index_def,
                lo: Some(lo),
                hi,
                lo_inclusive: true,
                hi_inclusive: true,
                covers_predicate: false,
            },
            other => other,
        };
    }

    // Rule 1: col = literal
    if let Some((col_name, value)) = extract_eq_col_literal(where_clause) {
        if let Some(idx) = find_index_on_col(col_name, indexes, columns, Some(where_clause), false)
        {
            if let Ok(key) = encode_index_key(&[value]) {
                // No cost gate for DELETE — always use the index.
                if idx.columns.len() == 1 {
                    return AccessMethod::IndexLookup {
                        index_def: idx.clone(),
                        key,
                        // DELETE/UPDATE always recheck the full WHERE on fetched
                        // rows, so coverage is irrelevant here — keep it false.
                        covers_predicate: false,
                    };
                } else {
                    let mut hi = key.clone();
                    hi.extend_from_slice(&[0xFF; crate::key_encoding::MAX_INDEX_KEY]);
                    return AccessMethod::IndexRange {
                        index_def: idx.clone(),
                        lo: Some(key),
                        hi: Some(hi),
                        lo_inclusive: true,
                        hi_inclusive: true,
                        covers_predicate: false,
                    };
                }
            }
        }
    }

    // Rule 2: col > lo AND col < hi (or >=, <=)
    if let Some((idx, (lo_val, _lo_incl), (hi_val, _hi_incl))) =
        extract_range(where_clause, indexes, columns, Some(where_clause),false)
    {
        // No cost gate for DELETE.
        let lo = lo_val.and_then(|v| encode_index_key(&[v]).ok());
        let hi = hi_val.and_then(|v| encode_index_key(&[v]).ok());
        return AccessMethod::IndexRange {
            index_def: idx.clone(),
            lo,
            hi,
            lo_inclusive: true,
            hi_inclusive: true,
            covers_predicate: false,
        };
    }

    AccessMethod::Scan
}

/// Chooses the best index-access method for discovering UPDATE candidate rows.
///
/// UPDATE uses the same candidate-discovery rules as DELETE:
/// - no `stats_cost_gate`
/// - no `IndexOnlyScan`
/// - full `WHERE` is rechecked later on fetched rows
/// - PRIMARY KEY, UNIQUE, secondary, and eligible partial indexes are allowed
pub fn plan_update_candidates(
    where_clause: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
) -> AccessMethod {
    use crate::key_encoding::encode_index_key;

    if let Some(am) = plan_composite_eq(where_clause, indexes, columns) {
        return match am {
            AccessMethod::IndexOnlyScan {
                index_def, lo, hi, ..
            } => AccessMethod::IndexRange {
                index_def,
                lo: Some(lo),
                hi,
                lo_inclusive: true,
                hi_inclusive: true,
                covers_predicate: false,
            },
            other => other,
        };
    }

    if let Some((col_name, value)) = extract_eq_col_literal(where_clause) {
        if let Some(idx) = find_index_on_col(col_name, indexes, columns, Some(where_clause), true) {
            if let Ok(key) = encode_index_key(&[value]) {
                if idx.columns.len() == 1 {
                    return AccessMethod::IndexLookup {
                        index_def: idx.clone(),
                        key,
                        // DELETE/UPDATE always recheck the full WHERE on fetched
                        // rows, so coverage is irrelevant here — keep it false.
                        covers_predicate: false,
                    };
                } else {
                    let mut hi = key.clone();
                    hi.extend_from_slice(&[0xFF; crate::key_encoding::MAX_INDEX_KEY]);
                    return AccessMethod::IndexRange {
                        index_def: idx.clone(),
                        lo: Some(key),
                        hi: Some(hi),
                        lo_inclusive: true,
                        hi_inclusive: true,
                        covers_predicate: false,
                    };
                }
            }
        }
    }

    if let Some((idx, (lo_val, _lo_incl), (hi_val, _hi_incl))) =
        extract_range(where_clause, indexes, columns, Some(where_clause),true)
    {
        let lo = lo_val.and_then(|v| encode_index_key(&[v]).ok());
        let hi = hi_val.and_then(|v| encode_index_key(&[v]).ok());
        return AccessMethod::IndexRange {
            index_def: idx.clone(),
            lo,
            hi,
            lo_inclusive: true,
            hi_inclusive: true,
            covers_predicate: false,
        };
    }

    AccessMethod::Scan
}

/// Session-collation-aware DELETE candidate planner.
///
/// Wraps [`plan_delete_candidates`] and rejects any access method that depends
/// on binary text ordering when the session collation is non-binary (e.g. `Es`).
/// This prevents silently missing rows when the index key order does not match
/// the folded comparison semantics in use.
pub fn plan_delete_candidates_ctx(
    where_clause: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    collation: SessionCollation,
) -> AccessMethod {
    let am = plan_delete_candidates(where_clause, indexes, columns);

    if collation == SessionCollation::Binary {
        return am;
    }

    // Reject text indexes under non-binary collation (same guard as plan_select_ctx).
    let text_col_idxs: std::collections::HashSet<u16> = columns
        .iter()
        .filter(|col| col.col_type == ColumnType::Text)
        .map(|col| col.col_idx)
        .collect();

    let uses_text_index = match &am {
        AccessMethod::IndexLookup { index_def, .. }
        | AccessMethod::IndexRange { index_def, .. } => index_def
            .columns
            .first()
            .map(|c| text_col_idxs.contains(&c.col_idx))
            .unwrap_or(false),
        AccessMethod::GinScan { .. } | AccessMethod::Scan | AccessMethod::IndexOnlyScan { .. } => {
            false
        }
    };

    if uses_text_index {
        AccessMethod::Scan
    } else {
        am
    }
}

/// Session-collation-aware UPDATE candidate planner.
///
/// Wraps [`plan_update_candidates`] and rejects any access method that depends
/// on binary text ordering when the session collation is non-binary.
pub fn plan_update_candidates_ctx(
    where_clause: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    collation: SessionCollation,
) -> AccessMethod {
    let am = plan_update_candidates(where_clause, indexes, columns);

    if collation == SessionCollation::Binary {
        return am;
    }

    let text_col_idxs: std::collections::HashSet<u16> = columns
        .iter()
        .filter(|col| col.col_type == ColumnType::Text)
        .map(|col| col.col_idx)
        .collect();

    let uses_text_index = match &am {
        AccessMethod::IndexLookup { index_def, .. }
        | AccessMethod::IndexRange { index_def, .. } => index_def
            .columns
            .first()
            .map(|c| text_col_idxs.contains(&c.col_idx))
            .unwrap_or(false),
        AccessMethod::GinScan { .. } | AccessMethod::Scan | AccessMethod::IndexOnlyScan { .. } => {
            false
        }
    };

    if uses_text_index {
        AccessMethod::Scan
    } else {
        am
    }
}
