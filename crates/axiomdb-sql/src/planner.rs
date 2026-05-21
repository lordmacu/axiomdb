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
//!   `WHERE col = <literal>` and `col` is the first column of a usable index.
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
use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use axiomdb_catalog::ColumnType;

use crate::{
    expr::{BinaryOp, Expr, UnaryOp},
    session::{SessionCollation, StaleStatsTracker},
};

include!("planner_types.rs");
include!("planner_select.rs");
include!("planner_ctx.rs");
include!("planner_zone.rs");

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
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
            enum_type_name: None,
            array_element_type: None,
            array_ndims: None,
            identity_kind: axiomdb_catalog::IdentityKind::None,
        }
    }

    fn make_text_col(name: &str, col_idx: u16) -> ColumnDef {
        ColumnDef {
            table_id: 1,
            col_idx,
            name: name.to_string(),
            col_type: ColumnType::Text,
            nullable: false,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
            enum_type_name: None,
            array_element_type: None,
            array_ndims: None,
            identity_kind: axiomdb_catalog::IdentityKind::None,
        }
    }

    fn make_bool_col(name: &str, col_idx: u16) -> ColumnDef {
        ColumnDef {
            table_id: 1,
            col_idx,
            name: name.to_string(),
            col_type: ColumnType::Bool,
            nullable: false,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
            enum_type_name: None,
            array_element_type: None,
            array_ndims: None,
            identity_kind: axiomdb_catalog::IdentityKind::None,
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
                expr: None,
            }],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        }
    }

    fn make_expression_index(
        name: &str,
        col_idx: u16,
        expr_sql: &str,
        predicate: Option<&str>,
    ) -> IndexDef {
        IndexDef {
            index_id: 1,
            table_id: 1,
            name: name.to_string(),
            root_page_id: 10,
            is_unique: false,
            is_primary: false,
            columns: vec![IndexColumnDef {
                col_idx,
                order: SortOrder::Asc,
                expr: Some(expr_sql.to_string()),
            }],
            predicate: predicate.map(|s| s.to_string()),
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        }
    }

    fn col_expr(name: &str) -> Expr {
        // col_idx 0 — the value doesn't matter for planner matching (uses name)
        Expr::Column {
            col_idx: 0,
            name: name.to_string(),
        }
    }

    fn make_stats(col_idx: u16, row_count: u64, ndv: i64) -> StatsDef {
        StatsDef {
            table_id: 1,
            col_idx,
            row_count,
            ndv,
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
    fn test_eq_on_primary_key_returns_lookup() {
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
        assert!(matches!(am, AccessMethod::IndexLookup { .. }));
    }

    #[test]
    fn test_eq_on_primary_key_bypasses_stats_cost_gate() {
        let cols = vec![make_col("id", 0)];
        let idxs = vec![make_index("pk", 0, true)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("id")),
            right: Box::new(Expr::Literal(Value::Int(1))),
        };
        let stats = vec![make_stats(0, 50, 1)];
        let am = plan_select(
            Some(&expr),
            &idxs,
            &cols,
            1,
            &stats,
            &mut StaleStatsTracker::default(),
            &[],
        );
        assert!(
            matches!(am, AccessMethod::IndexLookup { .. }),
            "pk equality should ignore small-table/selectivity scan bias"
        );
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
            index_type: 0,
            pages_per_range: 128,
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

    #[test]
    fn test_range_on_primary_key_returns_index_range_without_stats() {
        let cols = vec![make_col("id", 0)];
        let idxs = vec![make_index("pk", 0, true)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::And,
            left: Box::new(Expr::BinaryOp {
                op: BinaryOp::GtEq,
                left: Box::new(col_expr("id")),
                right: Box::new(Expr::Literal(Value::Int(10))),
            }),
            right: Box::new(Expr::BinaryOp {
                op: BinaryOp::Lt,
                left: Box::new(col_expr("id")),
                right: Box::new(Expr::Literal(Value::Int(20))),
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
        match am {
            AccessMethod::IndexRange {
                lo_inclusive,
                hi_inclusive,
                covers_predicate,
                ..
            } => {
                assert!(lo_inclusive, ">= is an inclusive lower bound");
                assert!(!hi_inclusive, "< is an exclusive upper bound");
                assert!(
                    covers_predicate,
                    "a pure two-sided PK range reproduces the whole WHERE"
                );
            }
            other => panic!("expected IndexRange, got {other:?}"),
        }
    }

    #[test]
    fn test_single_sided_gt_is_exclusive_lower_and_covering() {
        let cols = vec![make_col("id", 0)];
        let idxs = vec![make_index("pk", 0, true)];
        // WHERE id > 50  (single-sided, exclusive lower, open upper)
        let expr = Expr::BinaryOp {
            op: BinaryOp::Gt,
            left: Box::new(col_expr("id")),
            right: Box::new(Expr::Literal(Value::Int(50))),
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
        match am {
            AccessMethod::IndexRange {
                lo,
                hi,
                lo_inclusive,
                covers_predicate,
                ..
            } => {
                assert!(lo.is_some(), "lower bound present");
                assert!(hi.is_none(), "open upper bound");
                assert!(!lo_inclusive, "> is an exclusive lower bound");
                assert!(covers_predicate, "single-sided range covers the WHERE");
            }
            other => panic!("expected IndexRange, got {other:?}"),
        }
    }

    #[test]
    fn test_expression_partial_index_used_when_query_implies_predicate() {
        let cols = vec![make_text_col("email", 0), make_bool_col("active", 1)];
        let idxs = vec![make_expression_index(
            "idx_lower_email_active",
            0,
            "LOWER(email)",
            Some("active = TRUE"),
        )];
        let expr = Expr::BinaryOp {
            op: BinaryOp::And,
            left: Box::new(Expr::BinaryOp {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Function {
                    name: "LOWER".into(),
                    args: vec![col_expr("email")],
                }),
                right: Box::new(Expr::Literal(Value::Text("alice@example.com".into()))),
            }),
            right: Box::new(Expr::BinaryOp {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Column {
                    col_idx: 1,
                    name: "active".into(),
                }),
                right: Box::new(Expr::Literal(Value::Bool(true))),
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
        match am {
            AccessMethod::IndexRange { index_def, .. } => {
                assert_eq!(index_def.name, "idx_lower_email_active");
            }
            other => panic!("expected IndexRange for partial expression index, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_select_ctx_keeps_int_primary_key_lookup_under_non_binary_collation() {
        let cols = vec![make_col("id", 0)];
        let idxs = vec![make_index("pk", 0, true)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("id")),
            right: Box::new(Expr::Literal(Value::Int(7))),
        };
        let am = plan_select_ctx(
            Some(&expr),
            &idxs,
            &cols,
            1,
            &[],
            &mut StaleStatsTracker::default(),
            &[],
            SessionCollation::Es,
        );
        assert!(matches!(am, AccessMethod::IndexLookup { .. }));
    }

    #[test]
    fn test_plan_select_ctx_rejects_text_primary_key_under_non_binary_collation() {
        let cols = vec![make_text_col("id", 0)];
        let idxs = vec![make_index("pk", 0, true)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("id")),
            right: Box::new(Expr::Literal(Value::Text("jose".into()))),
        };
        let am = plan_select_ctx(
            Some(&expr),
            &idxs,
            &cols,
            1,
            &[],
            &mut StaleStatsTracker::default(),
            &[],
            SessionCollation::Es,
        );
        assert_eq!(am, AccessMethod::Scan);
    }

    #[test]
    fn test_plan_update_candidates_uses_primary_key_equality() {
        let cols = vec![make_col("id", 0)];
        let idxs = vec![make_index("pk", 0, true)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("id")),
            right: Box::new(Expr::Literal(Value::Int(9))),
        };
        let am = plan_update_candidates(&expr, &idxs, &cols);
        assert!(matches!(am, AccessMethod::IndexLookup { .. }));
    }

    #[test]
    fn test_plan_update_candidates_uses_primary_key_range() {
        let cols = vec![make_col("id", 0)];
        let idxs = vec![make_index("pk", 0, true)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::And,
            left: Box::new(Expr::BinaryOp {
                op: BinaryOp::GtEq,
                left: Box::new(col_expr("id")),
                right: Box::new(Expr::Literal(Value::Int(10))),
            }),
            right: Box::new(Expr::BinaryOp {
                op: BinaryOp::Lt,
                left: Box::new(col_expr("id")),
                right: Box::new(Expr::Literal(Value::Int(20))),
            }),
        };
        let am = plan_update_candidates(&expr, &idxs, &cols);
        assert!(matches!(am, AccessMethod::IndexRange { .. }));
    }

    #[test]
    fn test_plan_update_candidates_ctx_rejects_text_primary_key_under_non_binary_collation() {
        let cols = vec![make_text_col("name", 0)];
        let idxs = vec![make_index("pk_name", 0, true)];
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(col_expr("name")),
            right: Box::new(Expr::Literal(Value::Text("jose".into()))),
        };
        let am = plan_update_candidates_ctx(&expr, &idxs, &cols, SessionCollation::Es);
        assert_eq!(am, AccessMethod::Scan);
    }
}
