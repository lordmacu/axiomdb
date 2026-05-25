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
    pub(crate) catalog_epoch_at_build: u64,
    /// `write_commit_seq` at build — together with `catalog_epoch` the full DDL-invalidation
    /// check (catches another session's committed DDL). Both are scalar reads — no String,
    /// no HashMap, no SipHash (unlike the per-row `resolve_table_cached`).
    pub(crate) write_seq_at_build: u64,
    /// The table resolved ONCE at prepare, reused per execute while the validator holds so
    /// each execute SKIPS `resolve_table_cached` (its per-row ~3 String allocs + SipHash —
    /// the #1 bulk-insert cost per the perf profile). `None` ⇒ no cached resolve (the fast
    /// path resolves internally, unchanged). Safe to cache: it is the SCHEMA
    /// (def/columns/indexes/constraints/fks); the live clustered root is fetched separately
    /// (at flush), and any DDL bumps the validator. Only set for CLUSTERED tables whose only
    /// index is the PRIMARY KEY — SECONDARY index roots mutate per insert (a cached resolve
    /// would be stale), while the PK entry is safe (the insert reads only its columns, never
    /// its root).
    pub(crate) resolved: Option<std::sync::Arc<ResolvedTable>>,
}

impl PreparedInsertPlan {
    /// `true` while no DDL has run since the plan was built (`catalog_epoch` unchanged),
    /// so the fast path is schema-safe. On `false` the caller MUST use the generic path
    /// (which re-resolves); the plan may then be rebuilt.
    pub fn is_current(&self, ctx: &SessionContext) -> bool {
        self.catalog_epoch_at_build == ctx.catalog_epoch()
    }

    /// The table resolved at prepare, IFF still valid: no DDL since build (both
    /// `catalog_epoch` AND `write_commit_seq` unchanged) AND a cached resolve exists
    /// (no-secondary-index table). `None` ⇒ the caller resolves via `resolve_insert_target`
    /// (the unchanged path) — so a DDL between executes can never use a stale resolve.
    pub fn resolved_if_current(
        &self,
        ctx: &SessionContext,
        txn: &TxnManager,
    ) -> Option<&std::sync::Arc<ResolvedTable>> {
        if self.catalog_epoch_at_build == ctx.catalog_epoch()
            && self.write_seq_at_build == txn.write_commit_seq()
        {
            self.resolved.as_ref()
        } else {
            None
        }
    }
}

/// Diagnostic counter: rows executed via the prepared-INSERT fast path. Used by tests
/// (assert the fast path was taken) and the bench. Process-global + relaxed — a
/// monotonic hit count, not a correctness signal.
static FAST_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reads the prepared-INSERT fast-path hit counter.
pub fn prepared_insert_fast_hits() -> u64 {
    FAST_HITS.load(std::sync::atomic::Ordering::Relaxed)
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
pub fn try_prepare_insert_plan(
    analyzed: &Stmt,
    ctx: &mut SessionContext,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
) -> Option<PreparedInsertPlan> {
    match analyzed {
        Stmt::Insert(s) if insert_is_fast_eligible(s) => {
            // Resolve the table ONCE at prepare (outside any per-row loop). Cache it ONLY for
            // CLUSTERED tables whose only index is the PRIMARY KEY (no SECONDARY indexes).
            // Secondary index roots mutate per insert, so a cached resolve would carry stale
            // roots; the PK index entry is safe to cache because the clustered insert reads
            // only its COLUMNS (key encoding) — never its `root_page_id` (the live clustered
            // root is fetched separately at flush). Tables with secondary indexes (or heap
            // tables) still take the fast path, just resolving per execute (unchanged — no
            // regression). On resolve error (table missing etc.) `resolved` is None and the
            // per-execute path reports it.
            // A/B knob (read once here at prepare, NEVER per row): force the carry off so the
            // fast path re-resolves per row — isolates the carry-resolved-table win in the bench
            // (vs `AXIOMDB_NO_PREPARED_FAST`, which would disable the whole fast path).
            let resolved = if std::env::var_os("AXIOMDB_NO_CARRY_RESOLVE").is_some() {
                None
            } else {
                resolve_table_cached(storage, txn, ctx, None, &s.table)
                    .ok()
                    .filter(|rt| rt.def.is_clustered() && rt.indexes.iter().all(|i| i.is_primary))
            };
            Some(PreparedInsertPlan {
                catalog_epoch_at_build: ctx.catalog_epoch(),
                write_seq_at_build: txn.write_commit_seq(),
                resolved,
            })
        }
        _ => None,
    }
}

/// Executes an eligible, schema-current prepared INSERT on the fast path: it replicates
/// the generic dispatch INSERT arm (`exec_dispatch.rs` — resolve once →
/// `execute_insert_ctx_with_resolved` → statement triggers → epoch invalidate) WITHOUT
/// the `execute_with_ctx_locked` / `dispatch_ctx` per-statement scaffolding.
///
/// Preconditions (the caller MUST verify): `stmt` is the (param-substituted) eligible
/// INSERT this plan was built for, the plan [`is_current`](PreparedInsertPlan::is_current),
/// and a `conn_txn` is open (`ctx.conn_txn.is_some()` — the explicit-transaction staged
/// path). Result + transactional semantics are identical to the generic path.
///
/// Step 1b reuses `execute_insert_ctx_with_resolved` for full correctness; Step 5 trims
/// the inner loop further (reuse `ExecutionContext`, a value template, skip the per-row
/// epoch invalidate).
pub fn execute_prepared_insert(
    stmt: InsertStmt,
    cached_resolved: Option<std::sync::Arc<ResolvedTable>>,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    FAST_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let exec_ctx = ExecutionContext::new(storage, txn, bloom, None);
    // Stamp the active txn so any frames written by this statement carry it (mirrors
    // dispatch_ctx). Caller verified `conn_txn` is open.
    let txn_id = ctx
        .conn_txn
        .as_ref()
        .expect("execute_prepared_insert: caller verified conn_txn is open")
        .txn_id;
    let _txn_stamp = TxnStamp::new(storage, txn_id);

    let table_ref = stmt.table.clone();
    let mut conn = ctx
        .conn_txn
        .take()
        .expect("execute_prepared_insert: caller verified conn_txn is open");
    // Carry-resolved-table: reuse the prepare-time resolve when the caller passed it (skips
    // resolve_table_cached's per-row String allocs + SipHash); else resolve here (unchanged).
    let resolved_res = match cached_resolved {
        Some(rt) => Ok(rt),
        None => resolve_insert_target(&stmt, &exec_ctx, &mut conn, ctx),
    };
    // Carry the resolved table into the statement-trigger pass too, so it skips the per-row
    // get_table (String key + SipHash). Arc::clone is a refcount bump, not an allocation.
    let resolved_for_triggers = resolved_res.as_ref().ok().map(std::sync::Arc::clone);
    let r = match resolved_res {
        Ok(resolved) => {
            let table_id = resolved.def.id;
            execute_insert_ctx_with_resolved(stmt, &exec_ctx, &mut conn, ctx, resolved)
                .map(|qr| (qr, table_id))
        }
        Err(e) => Err(e),
    };
    ctx.conn_txn = Some(conn);
    let (result, table_id) =
        r.map_err(|e| translate_exclusion_violation_ctx(e, &exec_ctx, ctx, &table_ref))?;
    let result = run_statement_triggers_for_result(
        TriggerEvent::Insert,
        &table_ref,
        resolved_for_triggers.as_deref(),
        result,
        &exec_ctx,
        ctx,
    )?;
    // Clear the epoch mark so the next access re-validates after this write's root changes
    // (matches the generic Insert arm).
    ctx.invalidate_table_epoch_for_id(table_id);
    Ok(result)
}

#[cfg(test)]
mod prepared_insert_tests {
    use super::*;

    fn parse(sql: &str) -> Stmt {
        let ctx = SessionContext::default();
        crate::parse_with_sql_mode(sql, None, ctx.sql_mode_flags())
            .unwrap_or_else(|e| panic!("parse({sql}): {e}"))
    }

    /// The eligibility gate (`insert_is_fast_eligible`) over a parsed statement. The full
    /// [`try_prepare_insert_plan`] (which also resolves the table once and caches it) needs a
    /// live `storage`/`txn`, so it is exercised by the embedded integration tests
    /// (`fast_execute_skips_resolve`, `ddl_between_executes_*`) where a real `Db` exists.
    fn is_eligible(sql: &str) -> bool {
        matches!(&parse(sql), Stmt::Insert(s) if insert_is_fast_eligible(s))
    }

    #[test]
    fn plain_values_insert_is_eligible() {
        assert!(
            is_eligible("INSERT INTO t VALUES (1, 2, 3)"),
            "plain INSERT ... VALUES is eligible"
        );
    }

    #[test]
    fn multi_row_values_insert_is_eligible() {
        assert!(is_eligible("INSERT INTO t VALUES (1), (2), (3)"));
    }

    #[test]
    fn conflict_resolution_inserts_are_ineligible() {
        for sql in [
            "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
            "REPLACE INTO t VALUES (1)",
            "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE x = 1",
            "INSERT IGNORE INTO t VALUES (1)",
        ] {
            assert!(!is_eligible(sql), "ineligible (conflict): {sql}");
        }
    }

    #[test]
    fn returning_and_query_source_inserts_are_ineligible() {
        assert!(
            !is_eligible("INSERT INTO t (x) VALUES (1) RETURNING x"),
            "RETURNING is ineligible"
        );
        assert!(
            !is_eligible("INSERT INTO t SELECT * FROM u"),
            "INSERT ... SELECT is ineligible"
        );
    }

    #[test]
    fn non_insert_is_ineligible() {
        assert!(!is_eligible("SELECT 1"));
    }
}
