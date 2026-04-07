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
