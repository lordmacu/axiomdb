//! Query planner — selects the access method for a `SELECT` statement.
//!
//! The planner is a simple pattern-matching rewrite that detects whether the
//! `WHERE` clause matches a predicate on an indexed column and substitutes
//! a B-Tree lookup for a full table scan.
//!
//! ## Access methods
//!
//! - [`AccessMethod::Scan`] — full sequential scan (default).
//! - [`AccessMethod::IndexLookup`] — point lookup via B-Tree; used when
//!   `WHERE col = <literal>` and `col` is the first column of a non-primary index.
//! - [`AccessMethod::IndexRange`] — range scan via B-Tree; used when
//!   `WHERE col > lo AND col < hi` (or `>=`, `<=`) on an indexed column.
//!
//! ## Limitations (Phase 6.3)
//!
//! - Only single-column indexes are used (first column of the index).
//! - Only simple `col = literal` or range predicates are recognized.
//! - OR predicates, JOINs, and subqueries always fall through to `Scan`.
//! - Indexes with empty `columns` field (pre-6.1) are ignored.

use axiomdb_catalog::{ColumnDef, IndexDef, StatsDef};
use axiomdb_types::Value;

use crate::{
    expr::{BinaryOp, Expr},
    session::StaleStatsTracker,
};

// ── Statistics cost constants (Phase 6.10) ───────────────────────────────────

/// Fraction of rows below which an index scan beats a full scan.
/// Derived from PostgreSQL: seq_page_cost=1 / random_page_cost=4 ≈ 0.25.
/// AxiomDB uses 0.20 (slightly conservative for embedded single-file storage).
const INDEX_SELECTIVITY_THRESHOLD: f64 = 0.20;

/// Fallback NDV when no statistics exist (PostgreSQL `DEFAULT_NUM_DISTINCT`).
const DEFAULT_NUM_DISTINCT: i64 = 200;

/// Tables with fewer than this many rows always use a full scan.
/// Index overhead dominates for very small tables.
const SMALL_TABLE_THRESHOLD: u64 = 1_000;

// ── AccessMethod ─────────────────────────────────────────────────────────────

/// The access method chosen by the planner for a single table scan.
#[derive(Debug, Clone, PartialEq)]
pub enum AccessMethod {
    /// Full sequential scan — read every row from the heap.
    Scan,

    /// Point lookup: look up exactly one key in the B-Tree and read
    /// the corresponding heap row.
    IndexLookup {
        /// The index to use.
        index_def: IndexDef,
        /// Pre-encoded key bytes (via `encode_index_key`).
        key: Vec<u8>,
    },

    /// Range scan: iterate over B-Tree entries between `lo` and `hi`
    /// (both inclusive; `None` means unbounded).
    IndexRange {
        /// The index to use.
        index_def: IndexDef,
        /// Lower bound key (inclusive, already encoded).
        lo: Option<Vec<u8>>,
        /// Upper bound key (inclusive, already encoded).
        hi: Option<Vec<u8>>,
    },

    /// Index-only scan: all needed columns are in the index key columns (Phase 6.13).
    ///
    /// The executor decodes column values from the B-Tree key bytes instead of
    /// fetching the full heap row. A lightweight MVCC check (slot header only)
    /// is still performed on the heap to verify row visibility.
    IndexOnlyScan {
        /// The covering index.
        index_def: IndexDef,
        /// Lower bound key (inclusive). Equal to `hi` for point lookups.
        lo: Vec<u8>,
        /// Upper bound key (inclusive, `None` = unbounded; for point lookup: `Some(lo.clone())`).
        hi: Option<Vec<u8>>,
        /// Number of columns in the index key (= `index_def.columns.len()`).
        n_key_cols: usize,
        /// For each needed SELECT column (in output order): the position within
        /// the decoded key values (0 = first key column, 1 = second, …).
        needed_key_positions: Vec<usize>,
    },
}

// ── plan_select ──────────────────────────────────────────────────────────────

/// Chooses an [`AccessMethod`] for the given `WHERE` clause and available indexes.
///
/// Returns [`AccessMethod::Scan`] if no suitable index is found.
///
/// `table_stats` contains per-column statistics for the table being scanned.
/// An empty slice means "no statistics available" — the planner always uses
/// indexes (conservative: never wrong, just potentially suboptimal).
///
/// `stale_tracker` is used to:
/// 1. Register the row count baseline when stats are loaded (for Phase 6.11).
/// 2. Use `DEFAULT_NUM_DISTINCT` instead of catalog stats for stale tables.
pub fn plan_select(
    where_clause: Option<&Expr>,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    table_id: u32,
    table_stats: &[StatsDef],
    stale_tracker: &mut StaleStatsTracker,
    // Column indices needed in the SELECT output. Empty = SELECT * → no index-only scan.
    select_col_idxs: &[u16],
) -> AccessMethod {
    use crate::key_encoding::encode_index_key;

    let expr = match where_clause {
        Some(e) => e,
        None => return AccessMethod::Scan,
    };

    // ── Rule 0: composite equality (N ≥ 2 columns) ────────────────────────
    // Must run before Rule 1 to prefer composite indexes over single-column.
    if let Some(am) = plan_composite_eq(expr, indexes, columns) {
        if let AccessMethod::IndexRange {
            ref index_def,
            ref lo,
            ..
        } = am
        {
            if !stats_cost_gate(index_def, columns, table_id, table_stats, stale_tracker) {
                return AccessMethod::Scan;
            }
            // Index-only scan upgrade: composite key covers all SELECT columns.
            if index_covers_query(index_def, select_col_idxs) {
                if let Some(lo_key) = lo.clone() {
                    return AccessMethod::IndexOnlyScan {
                        n_key_cols: index_def.columns.len(),
                        needed_key_positions: build_key_positions(index_def, select_col_idxs),
                        hi: Some(lo_key.clone()),
                        lo: lo_key,
                        index_def: index_def.clone(),
                    };
                }
            }
        }
        return am;
    }

    // ── Rule 1: col = literal ─────────────────────────────────────────────
    if let Some((col_name, value)) = extract_eq_col_literal(expr) {
        if let Some(idx) = find_index_on_col(col_name, indexes, columns, Some(expr)) {
            if let Ok(key) = encode_index_key(&[value]) {
                // Cost gate: skip index if selectivity too low (Phase 6.10).
                if stats_cost_gate(idx, columns, table_id, table_stats, stale_tracker) {
                    // Index-only scan upgrade (Phase 6.13): all SELECT cols in key.
                    if index_covers_query(idx, select_col_idxs) {
                        return AccessMethod::IndexOnlyScan {
                            index_def: idx.clone(),
                            lo: key.clone(),
                            hi: Some(key), // point lookup: lo == hi
                            n_key_cols: idx.columns.len(),
                            needed_key_positions: build_key_positions(idx, select_col_idxs),
                        };
                    }
                    if idx.columns.len() == 1 {
                        return AccessMethod::IndexLookup {
                            index_def: idx.clone(),
                            key,
                        };
                    } else {
                        let mut hi = key.clone();
                        hi.extend_from_slice(&[0xFF; crate::key_encoding::MAX_INDEX_KEY]);
                        return AccessMethod::IndexRange {
                            index_def: idx.clone(),
                            lo: Some(key),
                            hi: Some(hi),
                        };
                    }
                }
            }
        }
    }

    // ── Rule 2: col > lo AND col < hi (or >=, <=) ─────────────────────────
    if let Some((idx, lo_val, hi_val)) = extract_range(expr, indexes, columns, Some(expr)) {
        // Cost gate: range scans are even less selective — apply same threshold.
        if stats_cost_gate(idx, columns, table_id, table_stats, stale_tracker) {
            let lo = lo_val.and_then(|v| encode_index_key(&[v]).ok());
            let hi = hi_val.and_then(|v| encode_index_key(&[v]).ok());
            return AccessMethod::IndexRange {
                index_def: idx.clone(),
                lo,
                hi,
            };
        }
    }

    AccessMethod::Scan
}

// ── Index-only scan coverage (Phase 6.13) ────────────────────────────────────

/// Returns `true` if all columns in `select_col_idxs` are covered by the index
/// KEY columns. Used to decide if an index-only scan is safe.
///
/// Only key columns count (not INCLUDE columns — B-Tree leaf storage for INCLUDE
/// is deferred to Phase 6.15). For Phase 6.13, coverage means every needed col
/// is directly decodable from the encoded key bytes.
fn index_covers_query(index_def: &IndexDef, select_col_idxs: &[u16]) -> bool {
    if select_col_idxs.is_empty() {
        return false; // SELECT * or unknown — never use index-only
    }
    let key_cols: std::collections::HashSet<u16> =
        index_def.columns.iter().map(|c| c.col_idx).collect();
    select_col_idxs.iter().all(|col| key_cols.contains(col))
}

/// Builds the `needed_key_positions` vector for `IndexOnlyScan`.
/// For each column in `select_col_idxs`, finds its position in `index.columns`.
fn build_key_positions(index_def: &IndexDef, select_col_idxs: &[u16]) -> Vec<usize> {
    select_col_idxs
        .iter()
        .filter_map(|col_idx| index_def.columns.iter().position(|c| c.col_idx == *col_idx))
        .collect()
}

// ── Statistics cost gate (Phase 6.10) ────────────────────────────────────────

/// Returns `true` if an index scan is worth using given the column statistics.
///
/// Uses `selectivity = 1 / NDV` for equality predicates. If selectivity is
/// above `INDEX_SELECTIVITY_THRESHOLD` (0.20 = 20% of rows), a full table
/// scan is cheaper. For small tables or when no stats exist, the function
/// conservatively returns `true` (use the index — never wrong, just possibly
/// suboptimal).
///
/// Also sets the staleness baseline if stats are loaded here for the first time.
fn stats_cost_gate(
    index_def: &IndexDef,
    _columns: &[ColumnDef],
    table_id: u32,
    table_stats: &[StatsDef],
    stale_tracker: &mut StaleStatsTracker,
) -> bool {
    // No stats → always use index (conservative default).
    if table_stats.is_empty() {
        return true;
    }

    // Find the first indexed column's col_idx.
    let col_idx = match index_def.columns.first() {
        Some(c) => c.col_idx,
        None => return true,
    };

    // Find stats for this column.
    let stats = match table_stats.iter().find(|s| s.col_idx == col_idx) {
        Some(s) => s,
        None => return true, // no stats for this column → use index
    };

    // Register baseline for Phase 6.11 staleness tracking.
    stale_tracker.set_baseline(table_id, stats.row_count);

    // Small table: always scan (index overhead not worth it).
    if stats.row_count < SMALL_TABLE_THRESHOLD {
        return false;
    }

    // Compute NDV (handle dual-encoding and zero/unknown).
    let ndv = if stats.ndv > 0 {
        stats.ndv
    } else {
        DEFAULT_NUM_DISTINCT
    };

    // selectivity = 1 / NDV for equality predicates.
    let selectivity = 1.0 / (ndv.max(1) as f64);
    selectivity <= INDEX_SELECTIVITY_THRESHOLD
}

// ── Rule 0: composite equality planner ───────────────────────────────────────

/// Collects all atomic `col = literal` equality conditions reachable via
/// AND-clauses in `expr`. Stops at OR, NOT, or any non-equality operator.
fn collect_eq_conditions(expr: &Expr) -> Vec<(&str, Value)> {
    match expr {
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut v = collect_eq_conditions(left);
            v.extend(collect_eq_conditions(right));
            v
        }
        other => extract_eq_col_literal(other).into_iter().collect(),
    }
}

/// Rule 0: try to match WHERE AND-clauses to the leading columns of a composite
/// index (≥ 2 columns). Returns `IndexRange { lo=hi=composite_key }` if a
/// composite match with at least 2 columns is found.
///
/// `IndexRange lo=hi` is used instead of `IndexLookup` because composite
/// non-unique indexes may have multiple rows per composite key — range scan
/// correctly returns all of them, while `lookup_in` only returns one.
fn plan_composite_eq(
    expr: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
) -> Option<AccessMethod> {
    use crate::key_encoding::encode_index_key;

    let eq_conds = collect_eq_conditions(expr);
    if eq_conds.len() < 2 {
        return None; // single-column → Rule 1 handles it
    }

    for idx in indexes.iter().filter(|i| {
        // Skip primary, FK auto-indexes, single-column indexes, partial indexes.
        !i.is_primary && !i.is_fk_index && i.columns.len() >= 2 && i.predicate.is_none()
    }) {
        let mut key_parts: Vec<Value> = Vec::new();

        // Try to match leading columns of the index to equality conditions.
        // Stops at the first unmatched column (prefix property).
        for idx_col in &idx.columns {
            let col_name = columns
                .iter()
                .find(|c| c.col_idx == idx_col.col_idx)
                .map(|c| c.name.as_str())?;

            match eq_conds.iter().find(|(name, _)| *name == col_name) {
                Some((_, val)) => key_parts.push(val.clone()),
                None => break, // gap in leading columns — can't use this index
            }
        }

        if key_parts.len() >= 2 {
            if let Ok(key) = encode_index_key(&key_parts) {
                // Also check partial index predicate implication (same as Rule 1).
                // For Phase 6.9, we already filtered out partial indexes above (predicate.is_none()).
                return Some(AccessMethod::IndexRange {
                    index_def: idx.clone(),
                    lo: Some(key.clone()),
                    hi: Some(key),
                });
            }
        }
    }
    None
}

// ── Helper: extract col = literal from WHERE ─────────────────────────────────

/// Returns `(col_name, value)` if `expr` is `col = literal` or `literal = col`.
fn extract_eq_col_literal(expr: &Expr) -> Option<(&str, Value)> {
    if let Expr::BinaryOp {
        op: BinaryOp::Eq,
        left,
        right,
    } = expr
    {
        // col = literal
        if let (Expr::Column { name, .. }, Expr::Literal(v)) = (left.as_ref(), right.as_ref()) {
            return Some((name.as_str(), v.clone()));
        }
        // literal = col
        if let (Expr::Literal(v), Expr::Column { name, .. }) = (left.as_ref(), right.as_ref()) {
            return Some((name.as_str(), v.clone()));
        }
    }
    None
}

/// Returns the first non-primary index whose first column matches `col_name`
/// and whose partial index predicate (if any) is implied by the query WHERE.
///
/// `query_where` is the full WHERE clause of the query, used for partial index
/// predicate implication checking (Phase 6.7).
fn find_index_on_col<'a>(
    col_name: &str,
    indexes: &'a [IndexDef],
    columns: &[ColumnDef],
    query_where: Option<&Expr>,
) -> Option<&'a IndexDef> {
    // Find the col_idx for this column name.
    let col_idx = columns.iter().find(|c| c.name == col_name)?.col_idx;

    // Find a non-primary index whose first column is this col_idx AND whose
    // predicate (if any) is implied by the query WHERE clause.
    indexes.iter().find(|idx| {
        if idx.is_primary {
            return false;
        }
        // FK auto-indexes use composite keys (fk_val | RecordId) — never usable
        // for plain SELECT column = value lookups.
        if idx.is_fk_index {
            return false;
        }
        if idx.columns.first().map(|c| c.col_idx) != Some(col_idx) {
            return false;
        }
        // Partial index guard (Phase 6.7): only use if predicate is implied.
        if let Some(pred_sql) = &idx.predicate {
            crate::partial_index::predicate_implied_by_query(pred_sql, query_where, columns)
        } else {
            true // full index — always usable
        }
    })
}

// ── Helper: extract range predicate ──────────────────────────────────────────

/// Returns `(index, lo_value, hi_value)` if `expr` is `col > lo AND col < hi`
/// (or with `>=` / `<=`).
fn extract_range<'a>(
    expr: &Expr,
    indexes: &'a [IndexDef],
    columns: &[ColumnDef],
    query_where: Option<&Expr>,
) -> Option<(&'a IndexDef, Option<Value>, Option<Value>)> {
    // expr must be `AND(left, right)`.
    let (lhs, rhs) = match expr {
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => (left.as_ref(), right.as_ref()),
        _ => return None,
    };

    // Each side must be a comparison: col >/< literal.
    let (col1, bound1) = extract_range_side(lhs)?;
    let (col2, bound2) = extract_range_side(rhs)?;

    // Both sides must reference the same column.
    if col1 != col2 {
        return None;
    }

    let idx = find_index_on_col(col1, indexes, columns, query_where)?;
    // bound1 = lo side, bound2 = hi side (order may be loose but correct for 6.3)
    Some((idx, bound1, bound2))
}

/// Returns `(col_name, bound_value)` for range comparison operators.
fn extract_range_side(expr: &Expr) -> Option<(&str, Option<Value>)> {
    if let Expr::BinaryOp { op, left, right } = expr {
        match op {
            BinaryOp::Gt | BinaryOp::GtEq => {
                // col > literal  →  lo = literal
                if let (Expr::Column { name, .. }, Expr::Literal(v)) =
                    (left.as_ref(), right.as_ref())
                {
                    return Some((name.as_str(), Some(v.clone())));
                }
                // literal < col  →  lo = literal (mirrored)
                if let (Expr::Literal(v), Expr::Column { name, .. }) =
                    (left.as_ref(), right.as_ref())
                {
                    return Some((name.as_str(), Some(v.clone())));
                }
            }
            BinaryOp::Lt | BinaryOp::LtEq => {
                // col < literal  →  hi = literal
                if let (Expr::Column { name, .. }, Expr::Literal(v)) =
                    (left.as_ref(), right.as_ref())
                {
                    return Some((name.as_str(), Some(v.clone())));
                }
                // literal > col  →  hi = literal (mirrored)
                if let (Expr::Literal(v), Expr::Column { name, .. }) =
                    (left.as_ref(), right.as_ref())
                {
                    return Some((name.as_str(), Some(v.clone())));
                }
            }
            _ => {}
        }
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_catalog::{ColumnType, IndexColumnDef, SortOrder};
    use axiomdb_types::Value;

    fn make_col(name: &str, col_idx: u16) -> ColumnDef {
        ColumnDef {
            table_id: 1,
            col_idx,
            name: name.to_string(),
            col_type: ColumnType::Int,
            nullable: false,
            auto_increment: false,
        }
    }

    fn make_index(name: &str, col_idx: u16, is_primary: bool) -> IndexDef {
        IndexDef {
            index_id: 1,
            table_id: 1,
            name: name.to_string(),
            root_page_id: 10,
            is_unique: false,
            is_primary,
            columns: vec![IndexColumnDef {
                col_idx,
                order: SortOrder::Asc,
            }],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
        }
    }

    fn col_expr(name: &str) -> Expr {
        // col_idx 0 — the value doesn't matter for planner matching (uses name)
        Expr::Column {
            col_idx: 0,
            name: name.to_string(),
        }
    }

    #[test]
    fn test_no_where_returns_scan() {
        let cols = vec![make_col("id", 0)];
        let idxs = vec![make_index("id_idx", 0, false)];
        assert_eq!(
            plan_select(
                None,
                &idxs,
                &cols,
                1,
                &[],
                &mut StaleStatsTracker::default(),
                &[]
            ),
            AccessMethod::Scan
        );
    }

    #[test]
    fn test_eq_on_indexed_col_returns_lookup() {
        let cols = vec![make_col("id", 0)];
        let idxs = vec![make_index("id_idx", 0, false)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("id")),
            right: Box::new(Expr::Literal(Value::Int(42))),
        };
        let am = plan_select(
            Some(&expr),
            &idxs,
            &cols,
            1,
            &[],
            &mut StaleStatsTracker::default(),
            &[],
        );
        assert!(matches!(am, AccessMethod::IndexLookup { .. }));
    }

    #[test]
    fn test_eq_on_primary_key_returns_scan() {
        // Primary key indexes are not used by the planner (Phase 6.3).
        let cols = vec![make_col("id", 0)];
        let idxs = vec![make_index("pk", 0, true)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("id")),
            right: Box::new(Expr::Literal(Value::Int(1))),
        };
        let am = plan_select(
            Some(&expr),
            &idxs,
            &cols,
            1,
            &[],
            &mut StaleStatsTracker::default(),
            &[],
        );
        assert_eq!(am, AccessMethod::Scan);
    }

    #[test]
    fn test_eq_on_non_indexed_col_returns_scan() {
        let cols = vec![make_col("id", 0), make_col("name", 1)];
        let idxs = vec![make_index("id_idx", 0, false)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("name")),
            right: Box::new(Expr::Literal(Value::Text("alice".into()))),
        };
        let am = plan_select(
            Some(&expr),
            &idxs,
            &cols,
            1,
            &[],
            &mut StaleStatsTracker::default(),
            &[],
        );
        assert_eq!(am, AccessMethod::Scan);
    }

    #[test]
    fn test_no_indexes_returns_scan() {
        let cols = vec![make_col("id", 0)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("id")),
            right: Box::new(Expr::Literal(Value::Int(1))),
        };
        let am = plan_select(
            Some(&expr),
            &[],
            &cols,
            1,
            &[],
            &mut StaleStatsTracker::default(),
            &[],
        );
        assert_eq!(am, AccessMethod::Scan);
    }

    #[test]
    fn test_index_with_no_columns_ignored() {
        let cols = vec![make_col("id", 0)];
        let idx_no_cols = IndexDef {
            index_id: 1,
            table_id: 1,
            name: "old_idx".into(),
            root_page_id: 10,
            is_unique: false,
            is_primary: false,
            columns: vec![], // old format — no column info
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
        };
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("id")),
            right: Box::new(Expr::Literal(Value::Int(1))),
        };
        let am = plan_select(
            Some(&expr),
            &[idx_no_cols],
            &cols,
            1,
            &[],
            &mut StaleStatsTracker::default(),
            &[],
        );
        assert_eq!(am, AccessMethod::Scan);
    }

    #[test]
    fn test_range_on_indexed_col_returns_index_range() {
        let cols = vec![make_col("age", 0)];
        let idxs = vec![make_index("age_idx", 0, false)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::And,
            left: Box::new(Expr::BinaryOp {
                op: BinaryOp::Gt,
                left: Box::new(col_expr("age")),
                right: Box::new(Expr::Literal(Value::Int(20))),
            }),
            right: Box::new(Expr::BinaryOp {
                op: BinaryOp::Lt,
                left: Box::new(col_expr("age")),
                right: Box::new(Expr::Literal(Value::Int(30))),
            }),
        };
        let am = plan_select(
            Some(&expr),
            &idxs,
            &cols,
            1,
            &[],
            &mut StaleStatsTracker::default(),
            &[],
        );
        assert!(matches!(am, AccessMethod::IndexRange { .. }));
    }
}
