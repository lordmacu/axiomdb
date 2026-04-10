// ── Zone map predicate extraction (Phase 8.3b) ─────────────────────────────

/// Extracts a `ZoneMapPredicate` from a simple WHERE clause for page-level skip.
///
/// Only handles simple numeric comparisons on a single column:
/// - `col = literal` → `Eq(val)`
/// - `col > literal` → `Gt(val)`, `col >= literal` → `GtEq(val)`
/// - `col < literal` → `Lt(val)`, `col <= literal` → `LtEq(val)`
///
/// Returns `None` if the WHERE clause is too complex or involves non-numeric types.
/// The returned predicate is used by `scan_table_filtered` to skip entire pages
/// whose zone map (min/max) is outside the predicate range.
/// Returns `(col_idx, predicate)` so the scan can verify the zone map's
/// tracked column matches the predicate column.
pub fn extract_zone_map_predicate(
    expr: &Expr,
    _columns: &[axiomdb_catalog::ColumnDef],
) -> Option<(usize, axiomdb_storage::zone_map::ZoneMapPredicate)> {
    use axiomdb_storage::zone_map::ZoneMapPredicate;

    match expr {
        Expr::BinaryOp { op, left, right } => {
            if *op == BinaryOp::And {
                return extract_zone_map_predicate(left, _columns)
                    .or_else(|| extract_zone_map_predicate(right, _columns));
            }

            // Extract col_idx from the Column expr.
            let (col_idx, val, is_reversed) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { col_idx, .. }, Expr::Literal(v)) => (*col_idx, v, false),
                (Expr::Literal(v), Expr::Column { col_idx, .. }) => (*col_idx, v, true),
                _ => return None,
            };

            let int_val = match val {
                Value::Int(n) => *n as i64,
                Value::BigInt(n) => *n,
                Value::Bool(b) => {
                    if *b {
                        1
                    } else {
                        0
                    }
                }
                _ => return None,
            };

            let pred = match (op, is_reversed) {
                (BinaryOp::Eq, _) => Some(ZoneMapPredicate::Eq(int_val)),
                (BinaryOp::Gt, false) | (BinaryOp::Lt, true) => Some(ZoneMapPredicate::Gt(int_val)),
                (BinaryOp::GtEq, false) | (BinaryOp::LtEq, true) => {
                    Some(ZoneMapPredicate::GtEq(int_val))
                }
                (BinaryOp::Lt, false) | (BinaryOp::Gt, true) => Some(ZoneMapPredicate::Lt(int_val)),
                (BinaryOp::LtEq, false) | (BinaryOp::GtEq, true) => {
                    Some(ZoneMapPredicate::LtEq(int_val))
                }
                _ => None,
            };
            pred.map(|p| (col_idx, p))
        }
        _ => None,
    }
}

// ── BRIN index predicate extraction (Phase 11.1b) ───────────────────────────

/// Extracts a BRIN comparison op + literal value from a WHERE clause for a
/// specific column. Used by the planner to decide which page ranges to skip.
pub fn extract_brin_predicate(
    expr: &Expr,
    brin_col_idx: usize,
) -> Option<(axiomdb_storage::brin::BrinCmpOp, i64)> {
    use axiomdb_storage::brin::BrinCmpOp;

    match expr {
        Expr::BinaryOp { op, left, right } => {
            if *op == BinaryOp::And {
                // For AND, try both sides. Take the first match.
                return extract_brin_predicate(left, brin_col_idx)
                    .or_else(|| extract_brin_predicate(right, brin_col_idx));
            }

            let (col_idx, val, is_reversed) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { col_idx, .. }, Expr::Literal(v)) => (*col_idx, v, false),
                (Expr::Literal(v), Expr::Column { col_idx, .. }) => (*col_idx, v, true),
                _ => return None,
            };

            if col_idx != brin_col_idx {
                return None;
            }

            let int_val = match val {
                Value::Int(n) => *n as i64,
                Value::BigInt(n) => *n,
                Value::Date(d) => *d as i64,
                Value::Timestamp(t) => *t,
                Value::Bool(b) => if *b { 1i64 } else { 0 },
                Value::Real(f) => f.to_bits() as i64,
                _ => return None,
            };

            let brin_op = match (op, is_reversed) {
                (BinaryOp::Eq, _) => Some(BrinCmpOp::Eq),
                (BinaryOp::Gt, false) | (BinaryOp::Lt, true) => Some(BrinCmpOp::Gt),
                (BinaryOp::GtEq, false) | (BinaryOp::LtEq, true) => Some(BrinCmpOp::Ge),
                (BinaryOp::Lt, false) | (BinaryOp::Gt, true) => Some(BrinCmpOp::Lt),
                (BinaryOp::LtEq, false) | (BinaryOp::GtEq, true) => Some(BrinCmpOp::Le),
                _ => None,
            };
            brin_op.map(|op| (op, int_val))
        }
        _ => None,
    }
}

/// Builds a set of qualifying page IDs from BRIN index summaries.
///
/// For each BRIN index on the table, checks if the WHERE clause references the
/// indexed column. If so, reads the BRIN summaries and returns only the page
/// ranges that might contain matching rows.
///
/// Returns `None` if no BRIN index applies (full scan needed).
/// Returns `Some(set)` of qualifying page ranges → scan only those pages.
pub fn build_brin_page_skip_set(
    indexes: &[axiomdb_catalog::IndexDef],
    where_clause: Option<&Expr>,
    storage: &dyn axiomdb_storage::StorageEngine,
) -> Option<std::collections::HashSet<u32>> {
    let where_expr = where_clause?;

    for idx in indexes {
        if idx.index_type != 1 || idx.columns.is_empty() {
            continue; // not BRIN
        }
        let brin_col_idx = idx.columns[0].col_idx as usize;
        let (op, literal) = extract_brin_predicate(where_expr, brin_col_idx)?;

        // Read summaries from the BRIN metapage.
        let summaries = axiomdb_storage::brin::read_summaries(storage, idx.root_page_id).ok()?;

        // Find qualifying ranges.
        let ranges = axiomdb_storage::brin::qualifying_ranges(&summaries, op, literal);
        let range_set: std::collections::HashSet<u32> = ranges.into_iter().collect();
        return Some(range_set);
    }
    None
}
