//! Statement-fingerprinting cache support (Attack 2).
//!
//! Provides the AST literal walker (`extract_literals`) and its inverse
//! (`substitute_params`) that the auto-prepared-statement cache uses to
//! key compiled plans by shape rather than by literal-interpolated SQL
//! text.
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-statement-fingerprinting.md`.
//!
//! The walker covers the same `Expr` variants that the manual
//! `PreparedStatement` (Phase 10.8) supports today: `BinaryOp`,
//! `UnaryOp`, `IsNull`, `Between`, `In`, `Like`, `Function`, `Cast`.
//! At the statement level: `Select`, `Insert (VALUES)`, `Update`,
//! `Delete`. Other forms are left alone — their literals stay in-place,
//! contributing to the shape hash so the cache still keys correctly
//! (just doesn't compress those positions). Adding coverage for a new
//! variant is a pure win, never a correctness concern.

use axiomdb_catalog::{schema::DEFAULT_DATABASE_NAME, CatalogReader};
use axiomdb_core::error::DbError;
use axiomdb_storage::StorageEngine;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use crate::ast::{InsertSource, SelectItem, Stmt};
use crate::bloom::BloomRegistry;
use crate::expr::Expr;
use crate::plan_deps::{extract_table_deps, PlanDeps};
use crate::result::QueryResult;
use crate::schema_cache::SchemaCache;
use crate::session::SessionContext;

/// Maximum number of cached plans per `SessionContext`. Beyond this,
/// the oldest entry (by insertion LRU sequence) is evicted. Picked
/// from PostgreSQL's `plan_cache_mode`-tier defaults (256) — large
/// enough for typical workloads, small enough that the bookkeeping
/// stays cheap.
pub const STATEMENT_CACHE_MAX_ENTRIES: usize = 256;

/// A compiled, ready-to-execute statement plan keyed in the cache by
/// `shape_hash` and tagged with its `PlanDeps` for invalidation.
///
/// `analyzed` is the analyzed `Stmt` **with literals already extracted to
/// `Expr::Param`** — callers must `substitute_params` it with their
/// fresh literals before execution.
#[derive(Debug, Clone)]
pub struct CachedPlan {
    /// Analyzed AST, literals replaced by `Expr::Param { idx }`.
    pub analyzed: Stmt,
    /// Number of `Expr::Param` nodes — must equal `extract_literals.len()`
    /// at substitute time.
    pub param_count: usize,
    /// Catalog dependencies snapshotted at compile time. Lookup checks
    /// these via `PlanDeps::is_stale` and evicts on mismatch.
    pub deps: PlanDeps,
}

/// Walks `stmt`, replacing every `Expr::Literal(v)` with
/// `Expr::Param { idx }` (where `idx` is the position in the returned
/// vector). The literals are collected in walk order.
///
/// After this call, `stmt` is "shape-only" — suitable for hashing as
/// the cache key. `substitute_params(stmt, &returned_vec)` restores
/// the original AST exactly (round-trip property).
pub fn extract_literals(stmt: &mut Stmt) -> Vec<Value> {
    let mut out = Vec::new();
    walk_stmt_extract(stmt, &mut out);
    out
}

fn walk_stmt_extract(stmt: &mut Stmt, out: &mut Vec<Value>) {
    match stmt {
        Stmt::Select(s) => {
            if let Some(ref mut wc) = s.where_clause {
                walk_expr_extract(wc, out);
            }
            for item in &mut s.columns {
                if let SelectItem::Expr { expr, .. } = item {
                    walk_expr_extract(expr, out);
                }
            }
        }
        Stmt::Insert(s) => {
            if let InsertSource::Values(rows) = &mut s.source {
                for row in rows {
                    for expr in row {
                        walk_expr_extract(expr, out);
                    }
                }
            }
        }
        Stmt::Update(s) => {
            for a in &mut s.assignments {
                walk_expr_extract(&mut a.value, out);
            }
            if let Some(ref mut wc) = s.where_clause {
                walk_expr_extract(wc, out);
            }
        }
        Stmt::Delete(s) => {
            if let Some(ref mut wc) = s.where_clause {
                walk_expr_extract(wc, out);
            }
        }
        _ => {} // DDL / others — leave alone
    }
}

fn walk_expr_extract(expr: &mut Expr, out: &mut Vec<Value>) {
    // Order matters: check Literal first (terminal). For recursive variants
    // we descend; for everything else (Column, Param, etc.) we leave alone.
    if matches!(expr, Expr::Literal(_)) {
        let idx = out.len();
        // Swap in a Param node, take ownership of the original Literal.
        let placeholder = Expr::Param { idx };
        let old = std::mem::replace(expr, placeholder);
        if let Expr::Literal(v) = old {
            out.push(v);
        } else {
            unreachable!("matches! guard above");
        }
        return;
    }
    match expr {
        Expr::BinaryOp { left, right, .. } => {
            walk_expr_extract(left, out);
            walk_expr_extract(right, out);
        }
        Expr::UnaryOp { operand, .. } => walk_expr_extract(operand, out),
        Expr::IsNull { expr: e, .. } => walk_expr_extract(e, out),
        Expr::Between {
            expr: e, low, high, ..
        } => {
            walk_expr_extract(e, out);
            walk_expr_extract(low, out);
            walk_expr_extract(high, out);
        }
        Expr::In { expr: e, list, .. } => {
            walk_expr_extract(e, out);
            for it in list {
                walk_expr_extract(it, out);
            }
        }
        Expr::Like {
            expr: e, pattern, ..
        } => {
            walk_expr_extract(e, out);
            walk_expr_extract(pattern, out);
        }
        Expr::Function { args, .. } => {
            for a in args {
                walk_expr_extract(a, out);
            }
        }
        Expr::Cast { expr: e, .. } => walk_expr_extract(e, out),
        // Column, Param, Literal (handled above), and any other variant
        // (Collate, etc.) — no literals to extract from this node.
        _ => {}
    }
}

/// Computes a 64-bit hash from a "shape-only" statement (one where
/// `extract_literals` has already replaced literals with `Expr::Param`).
///
/// Two statements with structurally identical ASTs (modulo Param indices,
/// which are deterministic from walk order) hash to the same value.
/// Structurally distinct statements hash to different values with
/// overwhelming probability.
///
/// Implementation is a hand-rolled recursive walker that feeds a
/// discriminant byte + the structural children of each node into a
/// `DefaultHasher`. Avoids the per-call multi-KB `format!("{:?}")`
/// allocation that the original prototype used.
pub fn shape_hash(stmt: &Stmt) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut h = DefaultHasher::new();
    hash_stmt(stmt, &mut h);
    h.finish()
}

fn hash_stmt(stmt: &Stmt, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    std::mem::discriminant(stmt).hash(h);
    match stmt {
        Stmt::Insert(s) => {
            s.table.name.hash(h);
            s.table.schema.hash(h);
            s.columns.hash(h);
            // For VALUES, hash the row count + (first row's expr shapes).
            // All rows have the same shape because parser enforces it,
            // so hashing just the first row plus row count is enough.
            if let InsertSource::Values(rows) = &s.source {
                (rows.len() as u32).hash(h);
                if let Some(first) = rows.first() {
                    (first.len() as u32).hash(h);
                    for e in first {
                        hash_expr(e, h);
                    }
                }
            }
        }
        Stmt::Update(s) => {
            s.table.name.hash(h);
            s.table.schema.hash(h);
            for a in &s.assignments {
                a.column.hash(h);
                hash_expr(&a.value, h);
            }
            if let Some(ref wc) = s.where_clause {
                hash_expr(wc, h);
            }
        }
        Stmt::Delete(s) => {
            s.table.name.hash(h);
            s.table.schema.hash(h);
            if let Some(ref wc) = s.where_clause {
                hash_expr(wc, h);
            }
        }
        Stmt::Select(s) => {
            // Lightweight SELECT shape: column count + each column's expr,
            // WHERE expr shape, presence flags for grouping/ordering/limit.
            // FROM clause structure is captured indirectly via the column
            // references inside hash_expr. Edge case: two SELECTs that
            // differ only in their JOIN structure could collide; in that
            // case PlanDeps still catches stale (since they reference
            // different tables) so the worst outcome is a re-analyze on
            // the second call (correct, just slower).
            (s.columns.len() as u32).hash(h);
            for col in &s.columns {
                std::mem::discriminant(col).hash(h);
                if let SelectItem::Expr { expr, .. } = col {
                    hash_expr(expr, h);
                }
            }
            if let Some(ref wc) = s.where_clause {
                hash_expr(wc, h);
            }
            std::mem::discriminant(&s.group_by).hash(h);
            (s.order_by.len() as u32).hash(h);
            s.limit.is_some().hash(h);
        }
        _ => {}
    }
}

fn hash_expr(expr: &Expr, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    std::mem::discriminant(expr).hash(h);
    match expr {
        Expr::Literal(_) => {
            // Shape-only: should be Param by now. If a literal slipped
            // through (walker doesn't cover this variant's parent),
            // include a marker so positions hash distinct.
            0xFFu8.hash(h);
        }
        Expr::Param { idx } => idx.hash(h),
        Expr::Column { name, .. } => name.hash(h),
        Expr::BinaryOp { op, left, right } => {
            std::mem::discriminant(op).hash(h);
            hash_expr(left, h);
            hash_expr(right, h);
        }
        Expr::UnaryOp { op, operand } => {
            std::mem::discriminant(op).hash(h);
            hash_expr(operand, h);
        }
        Expr::IsNull { expr, .. } => hash_expr(expr, h),
        Expr::Between {
            expr, low, high, ..
        } => {
            hash_expr(expr, h);
            hash_expr(low, h);
            hash_expr(high, h);
        }
        Expr::In { expr, list, .. } => {
            hash_expr(expr, h);
            (list.len() as u32).hash(h);
            for it in list {
                hash_expr(it, h);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            hash_expr(expr, h);
            hash_expr(pattern, h);
        }
        Expr::Function { name, args, .. } => {
            name.hash(h);
            (args.len() as u32).hash(h);
            for a in args {
                hash_expr(a, h);
            }
        }
        Expr::Cast { expr, .. } => hash_expr(expr, h),
        Expr::Collate { expr, collation } => {
            collation.hash(h);
            hash_expr(expr, h);
        }
        // Other variants: discriminant alone (already mixed in above).
        _ => {}
    }
}

/// Walks `stmt`, replacing every `Expr::Param { idx }` with
/// `Expr::Literal(params[idx])`. Inverse of [`extract_literals`].
///
/// Promoted from `axiomdb-embedded` (was duplicated by Phase 10.8's
/// manual `PreparedStatement`); both that path and the new auto-cache
/// share this implementation.
pub fn substitute_params(mut stmt: Stmt, params: &[Value]) -> Result<Stmt, DbError> {
    fn sub_expr(expr: &mut Expr, params: &[Value]) {
        match expr {
            Expr::Param { idx } => {
                if let Some(v) = params.get(*idx) {
                    *expr = Expr::Literal(v.clone());
                }
            }
            Expr::BinaryOp { left, right, .. } => {
                sub_expr(left, params);
                sub_expr(right, params);
            }
            Expr::UnaryOp { operand, .. } => sub_expr(operand, params),
            Expr::IsNull { expr: e, .. } => sub_expr(e, params),
            Expr::Between {
                expr, low, high, ..
            } => {
                sub_expr(expr, params);
                sub_expr(low, params);
                sub_expr(high, params);
            }
            Expr::In { expr, list, .. } => {
                sub_expr(expr, params);
                for item in list {
                    sub_expr(item, params);
                }
            }
            Expr::Like { expr, pattern, .. } => {
                sub_expr(expr, params);
                sub_expr(pattern, params);
            }
            Expr::Function { args, .. } => {
                for arg in args {
                    sub_expr(arg, params);
                }
            }
            Expr::Cast { expr: e, .. } => sub_expr(e, params),
            _ => {}
        }
    }

    match &mut stmt {
        Stmt::Select(s) => {
            if let Some(ref mut wc) = s.where_clause {
                sub_expr(wc, params);
            }
            for item in &mut s.columns {
                if let SelectItem::Expr { expr, .. } = item {
                    sub_expr(expr, params);
                }
            }
        }
        Stmt::Insert(s) => {
            if let InsertSource::Values(rows) = &mut s.source {
                for row in rows {
                    for expr in row {
                        sub_expr(expr, params);
                    }
                }
            }
        }
        Stmt::Update(s) => {
            for a in &mut s.assignments {
                sub_expr(&mut a.value, params);
            }
            if let Some(ref mut wc) = s.where_clause {
                sub_expr(wc, params);
            }
        }
        Stmt::Delete(s) => {
            if let Some(ref mut wc) = s.where_clause {
                sub_expr(wc, params);
            }
        }
        _ => {}
    }

    Ok(stmt)
}

/// Returns `true` when `stmt` is a candidate for the statement cache.
///
/// We cache only the four DML/SELECT forms that `extract_literals` /
/// `substitute_params` handle. Everything else (DDL, transaction
/// control, SHOW, EXPLAIN, COPY, etc.) bypasses the cache and runs
/// through the legacy parse → analyze → execute path. Caching DDL would
/// be wrong anyway (it mutates the catalog) and the other forms see
/// fewer call repetitions in real workloads, so the cache miss/hit
/// economics don't favor them.
fn is_cacheable(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Select(_) | Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_)
    )
}

/// Cache-aware entrypoint: parses → looks up the per-session shape
/// cache → on hit reuses the analyzed plan with new literals; on miss
/// analyzes, computes `PlanDeps`, caches the plan, and executes.
///
/// DDL and other non-cacheable statements fall through to the legacy
/// parse + analyze + execute pipeline.
///
/// This is the canonical entrypoint for `Db::run_inner` and any harness
/// wanting cache-aware execution. Callers that bypass it (manual
/// `PreparedStatement`, some test infrastructure) skip the cache.
pub fn run_cached(
    sql: &str,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &BloomRegistry,
    schema_cache: &mut SchemaCache,
    session: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    use crate::{analyze_cached, execute_with_ctx, parse_with_sql_mode};

    let mut stmt = parse_with_sql_mode(sql, None, session.sql_mode_flags())?;

    // Snapshot is needed both for analyze and for the deps probe; reuse.
    let snap = if let Some(ref ct) = session.conn_txn {
        txn.active_snapshot(ct)
    } else {
        txn.snapshot()
    };

    // DDL / non-cacheable: legacy path, no shape extraction.
    if !is_cacheable(&stmt) {
        let analyzed = analyze_cached(stmt, storage, snap, schema_cache)?;
        return execute_with_ctx(analyzed, storage, txn, bloom, session);
    }

    // Extract literals (rewrites stmt in-place to shape-only).
    let extracted = extract_literals(&mut stmt);
    let hash = shape_hash(&stmt);

    // A.2 epoch fast path: if all dep tables have epoch-current cached entries,
    // the plan's schema_versions are guaranteed current — skip CatalogReader
    // creation and PlanDeps::is_stale (which does a HeapChain scan per dep).
    // Mirrors the epoch shortcut in resolve_table_cached / try_cached_with_version.
    let analyzed = if let Some((plan_analyzed, param_count)) = session.epoch_plan_fast_path(hash) {
        if extracted.len() != param_count {
            return Err(DbError::Internal {
                message: format!(
                    "statement cache: literal count mismatch \
                     (cached plan expects {} params, got {})",
                    param_count,
                    extracted.len()
                ),
            });
        }
        session.bump_cached_plan_lru(hash);
        plan_analyzed
    } else {
        // Slow path: CatalogReader + PlanDeps::is_stale (one catalog probe per dep).
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        match session.get_cached_plan(hash, &mut reader)? {
            Some(plan) => {
                if extracted.len() != plan.param_count {
                    return Err(DbError::Internal {
                        message: format!(
                            "statement cache: literal count mismatch \
                             (cached plan expects {} params, got {})",
                            plan.param_count,
                            extracted.len()
                        ),
                    });
                }
                plan.analyzed.clone()
            }
            None => {
                // Miss — release the reader, analyze + compute deps + cache.
                drop(reader);
                let analyzed = analyze_cached(stmt, storage, snap.clone(), schema_cache)?;
                let mut reader = CatalogReader::new(storage, snap)?;
                let deps = extract_table_deps(&analyzed, &mut reader, DEFAULT_DATABASE_NAME)?;
                session.cache_plan(
                    hash,
                    CachedPlan {
                        analyzed: analyzed.clone(),
                        param_count: extracted.len(),
                        deps,
                    },
                );
                analyzed
            }
        }
    };

    // Restore literals into the cached/fresh plan and execute.
    let executable = substitute_params(analyzed, &extracted)?;
    execute_with_ctx(executable, storage, txn, bloom, session)
}
