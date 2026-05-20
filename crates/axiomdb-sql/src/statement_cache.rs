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

use axiomdb_catalog::CatalogReader;
use axiomdb_core::error::DbError;
use axiomdb_storage::StorageEngine;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use crate::ast::{
    DeleteStmt, FromClause, GroupByClause, InsertSource, InsertStmt, JoinCondition, SelectItem,
    SelectStmt, Stmt, UpdateStmt,
};
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

/// Returns `true` when `sql` begins with the `SELECT` keyword (case-insensitive,
/// leading whitespace ignored, word-boundary enforced).
///
/// Used to gate the statement-cache fast path in both the embedded and wire
/// paths. Conservative: `(SELECT ...)`, `WITH ... SELECT`, `SELECTED`, etc.
/// all return `false` and fall through to the legacy pipeline (always correct).
pub fn sql_starts_with_select_keyword(sql: &str) -> bool {
    let s = sql.trim_start().as_bytes();
    s.len() >= 6
        && s[0].eq_ignore_ascii_case(&b'S')
        && s[1].eq_ignore_ascii_case(&b'E')
        && s[2].eq_ignore_ascii_case(&b'L')
        && s[3].eq_ignore_ascii_case(&b'E')
        && s[4].eq_ignore_ascii_case(&b'C')
        && s[5].eq_ignore_ascii_case(&b'T')
        && s.get(6)
            .is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_')
}

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

fn walk_from_extract(from: &mut FromClause, out: &mut Vec<Value>) {
    match from {
        FromClause::Unnest(unnest) => {
            for expr in &mut unnest.exprs {
                walk_expr_extract(expr, out);
            }
        }
        FromClause::GenerateSeries(gs) => {
            walk_expr_extract(&mut gs.start, out);
            walk_expr_extract(&mut gs.stop, out);
            if let Some(ref mut step) = gs.step {
                walk_expr_extract(step, out);
            }
        }
        _ => {}
    }
}

fn walk_stmt_extract(stmt: &mut Stmt, out: &mut Vec<Value>) {
    match stmt {
        Stmt::Select(s) => {
            // FROM-clause SRFs: normalize_select_srf moves literals here during
            // analysis, so we extract them before analysis.
            if let Some(ref mut from) = s.from {
                walk_from_extract(from, out);
            }
            for join in &mut s.joins {
                walk_from_extract(&mut join.table, out);
            }
            for item in &mut s.columns {
                if let SelectItem::Expr { expr, .. } = item {
                    walk_expr_extract(expr, out);
                }
            }
            if let Some(ref mut wc) = s.where_clause {
                walk_expr_extract(wc, out);
            }
            // LIMIT and OFFSET can differ between otherwise identical queries.
            if let Some(ref mut lim) = s.limit {
                walk_expr_extract(lim, out);
            }
            if let Some(ref mut off) = s.offset {
                walk_expr_extract(off, out);
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

fn hash_from(from: &FromClause, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    std::mem::discriminant(from).hash(h);
    match from {
        FromClause::Table(t) => {
            t.name.hash(h);
            t.schema.hash(h);
        }
        FromClause::Unnest(unnest) => {
            (unnest.exprs.len() as u32).hash(h);
            for expr in &unnest.exprs {
                hash_expr(expr, h);
            }
        }
        FromClause::GenerateSeries(gs) => {
            hash_expr(&gs.start, h);
            hash_expr(&gs.stop, h);
            if let Some(ref step) = gs.step {
                true.hash(h);
                hash_expr(step, h);
            } else {
                false.hash(h);
            }
        }
        _ => {}
    }
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
            // DISTINCT / DISTINCT ON
            s.distinct.hash(h);
            (s.distinct_on.len() as u32).hash(h);
            for expr in &s.distinct_on {
                hash_expr(expr, h);
            }
            // FROM clause: distinguishes tables and SRF variants (UNNEST, GENERATE_SERIES).
            // After extract_literals, SRF args are Params — hash their shapes.
            if let Some(ref from) = s.from {
                true.hash(h);
                hash_from(from, h);
            } else {
                false.hash(h);
            }
            // JOINs: type + table shape (including SRFs in joins)
            (s.joins.len() as u32).hash(h);
            for join in &s.joins {
                std::mem::discriminant(&join.join_type).hash(h);
                hash_from(&join.table, h);
            }
            // SELECT columns
            (s.columns.len() as u32).hash(h);
            for col in &s.columns {
                std::mem::discriminant(col).hash(h);
                if let SelectItem::Expr { expr, .. } = col {
                    hash_expr(expr, h);
                }
            }
            // WHERE
            if let Some(ref wc) = s.where_clause {
                hash_expr(wc, h);
            }
            // GROUP BY: discriminant + all key exprs (column refs differ between queries)
            std::mem::discriminant(&s.group_by).hash(h);
            match &s.group_by {
                GroupByClause::Simple(exprs) | GroupByClause::WithRollup(exprs) => {
                    for expr in exprs {
                        hash_expr(expr, h);
                    }
                }
                _ => {}
            }
            // ORDER BY: full expr shape + direction (SortOrder doesn't impl Hash)
            (s.order_by.len() as u32).hash(h);
            for ob in &s.order_by {
                hash_expr(&ob.expr, h);
                std::mem::discriminant(&ob.order).hash(h);
            }
            // LIMIT / OFFSET: hash actual Param shape, not just presence
            if let Some(ref lim) = s.limit {
                true.hash(h);
                hash_expr(lim, h);
            } else {
                false.hash(h);
            }
            if let Some(ref off) = s.offset {
                true.hash(h);
                hash_expr(off, h);
            } else {
                false.hash(h);
            }
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
        Expr::Cast { expr, target } => {
            target.hash(h);
            hash_expr(expr, h);
        }
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

    // Walk expressions inside a FROM clause. `normalize_select_srf` can move
    // UNNEST / GenerateSeries expressions from the SELECT list into the FROM
    // clause during analysis — `sub_expr` must follow them there.
    fn sub_from(from: &mut FromClause, params: &[Value]) {
        match from {
            FromClause::Unnest(unnest) => {
                for expr in &mut unnest.exprs {
                    sub_expr(expr, params);
                }
            }
            FromClause::GenerateSeries(gs) => {
                sub_expr(&mut gs.start, params);
                sub_expr(&mut gs.stop, params);
                if let Some(ref mut step) = gs.step {
                    sub_expr(step, params);
                }
            }
            // Other variants (Table, Subquery, JsonTable, etc.) either have no
            // Params (Table) or are not reached via extract_literals (Subquery in
            // FROM is not walked at parse time).
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
            // Walk FROM / JOINs: analysis may move SELECT-column literals here.
            if let Some(ref mut from) = s.from {
                sub_from(from, params);
            }
            for join in &mut s.joins {
                sub_from(&mut join.table, params);
                if let JoinCondition::On(ref mut cond) = join.condition {
                    sub_expr(cond, params);
                }
            }
            if let Some(ref mut lim) = s.limit {
                sub_expr(lim, params);
            }
            if let Some(ref mut off) = s.offset {
                sub_expr(off, params);
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

/// Returns `true` only when EVERY construct in `stmt` is fully and
/// consistently handled by the cache trio: `walk_stmt_extract` (literal
/// extraction), `hash_stmt`/`hash_expr` (shape hashing), and
/// `substitute_params` (literal restoration).
///
/// This is a deliberately conservative **whitelist**. Any expression
/// variant or clause we don't explicitly cover makes the statement
/// ineligible, so it falls back to the always-correct legacy
/// analyze + execute path.
///
/// Why fail closed: the shape hash keys cached plans. If a construct's
/// distinguishing data is NOT in the hash (e.g. `GroupConcat.separator`
/// and its per-aggregate `ORDER BY`, `XmlQuery.xpath`, a FROM-clause
/// subquery's body, `GroupByClause::Sets` / `WithRollup`, `HAVING`,
/// `lock_clause`), two structurally different queries hash equal and the
/// cache returns the WRONG plan. Rather than enumerate every exotic
/// field in the hash (fragile — a new AST variant silently collides),
/// we only cache statements built from the small set of constructs the
/// trio provably round-trips.
///
/// The hot paths the cache exists for — point lookups, range scans,
/// simple grouped aggregates — are all covered. Exotic queries
/// (GROUP_CONCAT, XML/JSON table functions, GROUPING SETS/CUBE/ROLLUP,
/// `FOR UPDATE`, FROM-subqueries) skip the cache; they are rare and the
/// per-call analyze cost is irrelevant for them.
fn is_cache_eligible(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Select(s) => select_is_cache_eligible(s),
        Stmt::Insert(s) => insert_is_cache_eligible(s),
        Stmt::Update(s) => update_is_cache_eligible(s),
        Stmt::Delete(s) => delete_is_cache_eligible(s),
        _ => false,
    }
}

/// An expression is "cache-simple" iff every node is one of the variants
/// that `walk_expr_extract`, `hash_expr`, and `sub_expr` ALL handle
/// recursively and identically. Anything else (GroupConcat, ArrayAgg,
/// Grouping, Case, Window, Collate, Xml*, SqlJsonQuery, ArrayConstructor,
/// Subscript, subqueries, …) returns `false`.
fn expr_is_cache_simple(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::Param { .. } | Expr::Column { .. } => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_is_cache_simple(left) && expr_is_cache_simple(right)
        }
        Expr::UnaryOp { operand, .. } => expr_is_cache_simple(operand),
        Expr::IsNull { expr, .. } => expr_is_cache_simple(expr),
        Expr::Between {
            expr, low, high, ..
        } => expr_is_cache_simple(expr) && expr_is_cache_simple(low) && expr_is_cache_simple(high),
        Expr::In { expr, list, .. } => {
            expr_is_cache_simple(expr) && list.iter().all(expr_is_cache_simple)
        }
        Expr::Like { expr, pattern, .. } => {
            expr_is_cache_simple(expr) && expr_is_cache_simple(pattern)
        }
        Expr::Function { args, .. } => args.iter().all(expr_is_cache_simple),
        Expr::Cast { expr, .. } => expr_is_cache_simple(expr),
        _ => false,
    }
}

/// A FROM item is cache-simple iff it is a plain table or an SRF
/// (UNNEST / GENERATE_SERIES) whose argument expressions are cache-simple.
/// Subqueries, JSON_TABLE, XMLTABLE, VALUES, PIVOT, etc. are excluded —
/// their bodies are not part of the shape hash.
fn from_is_cache_simple(f: &FromClause) -> bool {
    match f {
        FromClause::Table(_) => true,
        FromClause::Unnest(u) => u.exprs.iter().all(expr_is_cache_simple),
        FromClause::GenerateSeries(gs) => {
            expr_is_cache_simple(&gs.start)
                && expr_is_cache_simple(&gs.stop)
                && gs.step.as_ref().is_none_or(expr_is_cache_simple)
        }
        _ => false,
    }
}

fn select_is_cache_eligible(s: &SelectStmt) -> bool {
    // Clauses with data the shape hash / substitute do not cover.
    if !s.with_ctes.is_empty()
        || !s.distinct_on.is_empty()
        || !s.hints.is_empty()
        || s.calc_found_rows
        || s.having.is_some()
        || s.lock_clause.is_some()
        || !s.set_op_rest.is_empty()
        || s.into_outfile.is_some()
    {
        return false;
    }
    // GROUP BY: only None or plain Simple(simple exprs).
    match &s.group_by {
        GroupByClause::None => {}
        GroupByClause::Simple(exprs) => {
            if !exprs.iter().all(expr_is_cache_simple) {
                return false;
            }
        }
        GroupByClause::WithRollup(_) | GroupByClause::Sets { .. } => return false,
    }
    for col in &s.columns {
        match col {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {}
            SelectItem::Expr { expr, .. } => {
                if !expr_is_cache_simple(expr) {
                    return false;
                }
            }
        }
    }
    if let Some(ref from) = s.from {
        if !from_is_cache_simple(from) {
            return false;
        }
    }
    for join in &s.joins {
        if !from_is_cache_simple(&join.table) {
            return false;
        }
        if let JoinCondition::On(ref cond) = join.condition {
            if !expr_is_cache_simple(cond) {
                return false;
            }
        }
    }
    if let Some(ref wc) = s.where_clause {
        if !expr_is_cache_simple(wc) {
            return false;
        }
    }
    if !s.order_by.iter().all(|ob| expr_is_cache_simple(&ob.expr)) {
        return false;
    }
    if let Some(ref lim) = s.limit {
        if !expr_is_cache_simple(lim) {
            return false;
        }
    }
    if let Some(ref off) = s.offset {
        if !expr_is_cache_simple(off) {
            return false;
        }
    }
    true
}

fn insert_is_cache_eligible(s: &InsertStmt) -> bool {
    // Only plain `INSERT ... VALUES (...)` with cache-simple row exprs.
    // Upsert / RETURNING / INSERT..SELECT carry exprs the trio doesn't walk.
    if s.ignore || s.replace || !s.returning.is_empty() || s.on_conflict.is_some() {
        return false;
    }
    if s.on_duplicate_update.is_some() {
        return false;
    }
    match &s.source {
        InsertSource::Values(rows) => rows.iter().all(|row| row.iter().all(expr_is_cache_simple)),
        _ => false,
    }
}

fn update_is_cache_eligible(s: &UpdateStmt) -> bool {
    if !s.joins.is_empty() || !s.order_by.is_empty() || s.limit.is_some() || !s.returning.is_empty()
    {
        return false;
    }
    s.assignments.iter().all(|a| expr_is_cache_simple(&a.value))
        && s.where_clause.as_ref().is_none_or(expr_is_cache_simple)
}

fn delete_is_cache_eligible(s: &DeleteStmt) -> bool {
    if s.target.is_some()
        || !s.joins.is_empty()
        || !s.order_by.is_empty()
        || s.limit.is_some()
        || !s.returning.is_empty()
    {
        return false;
    }
    s.where_clause.as_ref().is_none_or(expr_is_cache_simple)
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
///
/// `read_only`: when `true`, the statement is executed through
/// [`crate::execute_read_only_with_ctx`] — a pure-snapshot reader that does
/// NOT open/commit a transaction. The MySQL wire server's concurrent
/// read path (`execute_read_query`) requires this: routing an autocommit
/// SELECT through the write executor (`execute_with_ctx`) opens an implicit
/// txn whose snapshot can race a just-committed write on the same
/// connection, intermittently returning stale/empty results. When `false`,
/// the write-capable [`crate::execute_with_ctx`] is used (embedded path and
/// the wire `&mut self` path, which may carry staged writes in an explicit
/// transaction). Only pass `true` when there is provably no open
/// write-transaction with staged rows.
pub fn run_cached(
    sql: &str,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &BloomRegistry,
    schema_cache: &mut SchemaCache,
    session: &mut SessionContext,
    read_only: bool,
) -> Result<QueryResult, DbError> {
    use crate::parse_with_sql_mode;
    let stmt = parse_with_sql_mode(sql, None, session.sql_mode_flags())?;
    run_cached_stmt(stmt, storage, txn, bloom, schema_cache, session, read_only)
}

/// Same as [`run_cached`] but takes an already-parsed statement, so callers
/// that must inspect the AST before deciding to use the cache (e.g. the wire
/// server, which routes `FOR UPDATE`/`FOR SHARE` selects to the lock-manager
/// executor) don't pay for a second parse. See [`run_cached`] for `read_only`.
pub fn run_cached_stmt(
    mut stmt: Stmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &BloomRegistry,
    schema_cache: &mut SchemaCache,
    session: &mut SessionContext,
    read_only: bool,
) -> Result<QueryResult, DbError> {
    use crate::analyze_cached_with_defaults;

    // Choose the executor: read-only snapshot reader vs write-capable executor.
    fn exec_final(
        read_only: bool,
        stmt: Stmt,
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        bloom: &BloomRegistry,
        session: &mut SessionContext,
    ) -> Result<QueryResult, DbError> {
        if read_only {
            crate::execute_read_only_with_ctx(stmt, storage, txn, bloom, session)
        } else {
            crate::execute_with_ctx(stmt, storage, txn, bloom, session)
        }
    }

    // Snapshot is needed both for analyze and for the deps probe; reuse.
    let snap = if let Some(ref ct) = session.conn_txn {
        txn.active_snapshot(ct)
    } else {
        txn.snapshot()
    };

    // DDL / non-cacheable / not-fully-supported: legacy path, no shape
    // extraction. The eligibility gate fails closed for any construct whose
    // distinguishing data is not in the shape hash (prevents wrong-plan reuse).
    if !is_cache_eligible(&stmt) {
        let analyzed = analyze_cached_with_defaults(
            stmt,
            storage,
            snap,
            session.effective_database(),
            session.current_schema(),
            schema_cache,
        )?;
        return exec_final(read_only, analyzed, storage, txn, bloom, session);
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
                let analyzed = analyze_cached_with_defaults(
                    stmt,
                    storage,
                    snap.clone(),
                    session.effective_database(),
                    session.current_schema(),
                    schema_cache,
                )?;
                let mut reader = CatalogReader::new(storage, snap)?;
                let deps =
                    extract_table_deps(&analyzed, &mut reader, session.effective_database())?;
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
    exec_final(read_only, executable, storage, txn, bloom, session)
}
