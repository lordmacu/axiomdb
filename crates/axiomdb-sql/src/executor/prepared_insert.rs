// Specialized prepared-INSERT execute (parity lever #1 — perf-sqlite-gap).
//
// SQLite's VDBE compiles a statement once at `prepare` and per row runs only
// `OP_MakeRecord` + `OP_Insert` (`research/sqlite/src/vdbe.c`). AxiomDB's generic
// executor instead pays the per-statement scaffolding (`ExecutionContext::new`, the
// `Stmt` matches, the per-statement resolve/setup) on *every* row of a prepared INSERT.
// This module is the fast path: an explicit [`PreparedStatement`] over an eligible
// INSERT caches a resolved plan once and per row does only bind + codec + enqueue,
// revalidated against the **DDL-only** `catalog_epoch` (NOT `schema_version`, which
// bumps on every clustered root split — the 6e mistake).
//
// Step 1a (this commit) lands the plan type + eligibility gate; the execute path,
// `PreparedStatement` routing, and the differential-equivalence test follow in 1b
// (`specs/fase-perf-sqlite-gap/plan-prepared-insert-execute.md`).

/// A cached, DDL-revalidated plan for executing an eligible prepared INSERT on the
/// fast path. Built lazily on first execute from the analyzed statement; revalidated
/// before each fast execute via `catalog_epoch`.
#[derive(Debug, Clone)]
pub struct PreparedInsertPlan {
    /// `catalog_epoch` when this plan was built. A fast execute runs only while the
    /// session's epoch still equals this (no DDL since); on mismatch the caller falls
    /// back to the generic path and the plan is rebuilt.
    // `allow(dead_code)`: read by `execute_prepared_insert` in step 1b (the epoch recheck);
    // only the eligibility test reads it in 1a.
    #[allow(dead_code)]
    pub(crate) catalog_epoch_at_build: u64,
    // 1b adds: table_id, is_clustered, col_positions, primary_idx, value_template.
}

/// Whether an INSERT statement is eligible for the specialized fast path. Only the
/// simple, common shape qualifies — anything needing conflict resolution, a row
/// projection, a query source, or per-row machinery beyond plain staging falls back
/// to the generic executor (which is always correct).
fn insert_is_fast_eligible(s: &InsertStmt) -> bool {
    matches!(s.source, InsertSource::Values(_))
        && s.returning.is_empty()
        && !s.replace
        && !s.ignore
        && s.on_duplicate_update.is_none()
        && s.on_conflict.is_none()
}

/// Builds a [`PreparedInsertPlan`] for `analyzed` if it is an eligible INSERT, stamping
/// the current `catalog_epoch`. `None` ⇒ the caller must use the generic execute path.
pub fn try_prepare_insert_plan(analyzed: &Stmt, ctx: &SessionContext) -> Option<PreparedInsertPlan> {
    match analyzed {
        Stmt::Insert(s) if insert_is_fast_eligible(s) => Some(PreparedInsertPlan {
            catalog_epoch_at_build: ctx.catalog_epoch(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod prepared_insert_tests {
    use super::*;

    fn parse(sql: &str) -> Stmt {
        let ctx = SessionContext::default();
        crate::parse_with_sql_mode(sql, None, ctx.sql_mode_flags())
            .unwrap_or_else(|e| panic!("parse({sql}): {e}"))
    }

    #[test]
    fn plain_values_insert_is_eligible() {
        let ctx = SessionContext::default();
        let plan = try_prepare_insert_plan(&parse("INSERT INTO t VALUES (1, 2, 3)"), &ctx);
        assert!(plan.is_some(), "plain INSERT ... VALUES is eligible");
        assert_eq!(plan.unwrap().catalog_epoch_at_build, ctx.catalog_epoch());
    }

    #[test]
    fn multi_row_values_insert_is_eligible() {
        let ctx = SessionContext::default();
        assert!(try_prepare_insert_plan(&parse("INSERT INTO t VALUES (1), (2), (3)"), &ctx).is_some());
    }

    #[test]
    fn conflict_resolution_inserts_are_ineligible() {
        let ctx = SessionContext::default();
        for sql in [
            "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
            "REPLACE INTO t VALUES (1)",
            "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE x = 1",
            "INSERT IGNORE INTO t VALUES (1)",
        ] {
            assert!(
                try_prepare_insert_plan(&parse(sql), &ctx).is_none(),
                "ineligible (conflict): {sql}"
            );
        }
    }

    #[test]
    fn returning_and_query_source_inserts_are_ineligible() {
        let ctx = SessionContext::default();
        assert!(
            try_prepare_insert_plan(&parse("INSERT INTO t (x) VALUES (1) RETURNING x"), &ctx)
                .is_none(),
            "RETURNING is ineligible"
        );
        assert!(
            try_prepare_insert_plan(&parse("INSERT INTO t SELECT * FROM u"), &ctx).is_none(),
            "INSERT ... SELECT is ineligible"
        );
    }

    #[test]
    fn non_insert_is_ineligible() {
        let ctx = SessionContext::default();
        assert!(try_prepare_insert_plan(&parse("SELECT 1"), &ctx).is_none());
    }
}
