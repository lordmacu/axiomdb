//! Plan dependency tracking for OID-based cache invalidation.
//!
//! ## Purpose
//!
//! When a SQL statement is compiled (parsed + analyzed), we record every
//! catalog object it depends on. The plan cache uses these dependencies to
//! detect stale entries without flushing the entire cache on any DDL event.
//!
//! ## Design
//!
//! Mirrors PostgreSQL's `plancache.c` dual-dependency design:
//! - `PlanDeps::tables` — `(TableId, schema_version)` for every table
//!   referenced by the statement, snapshotted at compile time.
//! - `PlanDeps::items` — non-table catalog objects (index OIDs, future:
//!   function OIDs, type OIDs). Currently tracks index dependencies.
//!
//! Staleness check (`PlanDeps::is_stale`): for each table dep, the current
//! `TableDef::schema_version` is read from the catalog. A mismatch (or a
//! missing table) means the plan must be recompiled. This is a lazy check —
//! performed at lookup time, not pushed eagerly — with an additional eager
//! push from `PlanCache::invalidate_table` after each DDL event
//! (belt-and-suspenders).
//!
//! ## Scope
//!
//! DDL statements (`CreateTable`, `DropTable`, `CreateIndex`, …) return an
//! empty `PlanDeps` — they are never cached.

use std::collections::HashMap;

use axiomdb_catalog::{
    schema::{TableId, DEFAULT_DATABASE_NAME},
    CatalogReader,
};
use axiomdb_core::error::DbError;

use crate::{
    ast::{
        DeleteStmt, FromClause, InsertSource, InsertStmt, JoinClause, SelectItem, SelectStmt, Stmt,
        TableRef, UpdateStmt,
    },
    expr::Expr,
};

// ── InvalItem ────────────────────────────────────────────────────────────────

/// A non-table catalog dependency.
///
/// Mirrors PostgreSQL's `PlanInvalItem`. Currently only index OIDs are tracked.
/// Function OIDs and type OIDs are reserved for Phase 6 (UDFs).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InvalItem {
    pub kind: InvalItemKind,
    /// Catalog object ID (index_id; future: function_oid, type_oid).
    pub object_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvalItemKind {
    /// An index that the planner chose to use. If this index is dropped, the
    /// plan must be recompiled to pick a different access method.
    Index,
    // Function and Type are reserved for Phase 6 (UDFs / type system extensions).
}

// ── PlanDeps ─────────────────────────────────────────────────────────────────

/// All catalog dependencies of a compiled statement.
///
/// Stored in every `CachedPlanSource` and `PreparedStatement`. The staleness
/// check verifies each dependency against the live catalog before a cached
/// plan is reused.
#[derive(Debug, Clone, Default)]
pub struct PlanDeps {
    /// `(table_id, schema_version_at_compile_time)` for every table
    /// referenced by this statement. Deduplicated by `table_id`.
    ///
    /// A dep is stale when:
    ///   - The table no longer exists (`get_table_schema_version` returns `None`).
    ///   - The table's current `schema_version` differs from the cached value.
    pub tables: Vec<(TableId, u64)>,

    /// Non-table dependencies. Currently index OIDs chosen by the access
    /// method selector. Validated by checking that the index still exists
    /// in the catalog.
    pub items: Vec<InvalItem>,
}

impl PlanDeps {
    /// Returns `true` if any dependency has become stale.
    ///
    /// Stale conditions:
    /// - A referenced table was dropped (`None` from catalog).
    /// - A referenced table's `schema_version` was bumped by DDL.
    /// - (Future) A referenced index was dropped.
    ///
    /// This is the hot path — called on every cache lookup. It scans the
    /// catalog heap for each dep, so `tables.len()` should stay small (1-3
    /// for typical OLTP queries).
    pub fn is_stale(&self, reader: &mut CatalogReader<'_>) -> Result<bool, DbError> {
        for &(table_id, cached_ver) in &self.tables {
            match reader.get_table_schema_version(table_id)? {
                // Table was dropped — plan is definitely stale.
                None => return Ok(true),
                // Version bumped by DDL since compile time.
                Some(cur) if cur != cached_ver => return Ok(true),
                _ => {}
            }
        }
        // Index items: verify each index still exists in the catalog.
        // Placeholder for Phase 5+ when CatalogReader::index_exists is added.
        for _item in &self.items {
            // TODO(Phase 5): check index existence via catalog
        }
        Ok(false)
    }

    /// Returns `true` if this statement has no catalog dependencies.
    /// DDL statements and parameter-less constant queries have empty deps.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty() && self.items.is_empty()
    }
}

// ── Dependency extractor ─────────────────────────────────────────────────────

/// Walks `stmt` and returns all catalog dependencies.
///
/// Resolves every `TableRef` in the statement to its `(TableId, schema_version)`
/// using a live `CatalogReader`. Resolution uses `database` as the default
/// database and `DEFAULT_DATABASE_NAME` as the default schema when not
/// specified in the ref.
///
/// DDL statements return `PlanDeps::default()` — they are never cached.
///
/// # Errors
/// - `DbError::TableNotFound` if a referenced table does not exist. In this
///   case the statement should not be cached (the analyzer would have already
///   returned this error; this path handles defensive re-checking at cache time).
/// - Catalog I/O errors propagated from `CatalogReader`.
pub fn extract_table_deps(
    stmt: &Stmt,
    reader: &mut CatalogReader<'_>,
    database: &str,
) -> Result<PlanDeps, DbError> {
    let mut collector = DepCollector::new(reader, database);
    collector.visit_stmt(stmt)?;
    Ok(collector.finish())
}

// ── DepCollector ─────────────────────────────────────────────────────────────

struct DepCollector<'r, 'db> {
    reader: &'r mut CatalogReader<'db>,
    database: &'r str,
    /// Accumulates (table_id → schema_version). HashMap deduplicates
    /// self-joins and repeated table references automatically.
    seen: HashMap<TableId, u64>,
    items: Vec<InvalItem>,
}

impl<'r, 'db> DepCollector<'r, 'db> {
    fn new(reader: &'r mut CatalogReader<'db>, database: &'r str) -> Self {
        Self {
            reader,
            database,
            seen: HashMap::new(),
            items: Vec::new(),
        }
    }

    fn finish(self) -> PlanDeps {
        PlanDeps {
            tables: self.seen.into_iter().collect(),
            items: self.items,
        }
    }

    // ── Stmt dispatch ────────────────────────────────────────────────────────

    fn visit_stmt(&mut self, stmt: &Stmt) -> Result<(), DbError> {
        match stmt {
            Stmt::Select(s) => self.visit_select(s),
            Stmt::Insert(s) => self.visit_insert(s),
            Stmt::Update(s) => self.visit_update(s),
            Stmt::Delete(s) => self.visit_delete(s),
            // DDL — never cached; return empty deps.
            Stmt::CreateTable(_)
            | Stmt::CreateDatabase(_)
            | Stmt::CreateSchema(_)
            | Stmt::CreateIndex(_)
            | Stmt::DropTable(_)
            | Stmt::DropDatabase(_)
            | Stmt::DropIndex(_)
            | Stmt::TruncateTable(_)
            | Stmt::AlterTable(_)
            | Stmt::Analyze(_) => Ok(()),
            // Introspection — catalog meta tables, no user data table deps.
            Stmt::ShowTables(_) | Stmt::ShowDatabases(_) | Stmt::ShowColumns(_) => Ok(()),
            // Transaction control — no deps.
            Stmt::Begin
            | Stmt::Commit
            | Stmt::Rollback
            | Stmt::Savepoint(_)
            | Stmt::RollbackToSavepoint(_)
            | Stmt::ReleaseSavepoint(_) => Ok(()),
            // Maintenance.
            Stmt::Vacuum(v) => {
                if let Some(tref) = &v.table {
                    self.visit_tableref(tref)?;
                }
                Ok(())
            }
            // EXPLAIN: recurse into the wrapped statement.
            Stmt::Explain(inner) => self.visit_stmt(inner),
            // Session — no catalog table deps.
            Stmt::Set(_) | Stmt::UseDatabase(_) => Ok(()),
        }
    }

    // ── DML visitors ─────────────────────────────────────────────────────────

    fn visit_select(&mut self, s: &SelectStmt) -> Result<(), DbError> {
        if let Some(from) = &s.from {
            self.visit_from_clause(from)?;
        }
        self.visit_joins(&s.joins)?;
        // Recurse into subqueries embedded in expressions.
        self.visit_exprs_in_select(s)
    }

    fn visit_insert(&mut self, s: &InsertStmt) -> Result<(), DbError> {
        self.visit_tableref(&s.table)?;
        // INSERT ... SELECT — target table dep already added above.
        if let InsertSource::Select(sel) = &s.source {
            self.visit_select(sel)?;
        }
        Ok(())
    }

    fn visit_update(&mut self, s: &UpdateStmt) -> Result<(), DbError> {
        self.visit_tableref(&s.table)?;
        if let Some(w) = &s.where_clause {
            self.visit_expr(w)?;
        }
        Ok(())
    }

    fn visit_delete(&mut self, s: &DeleteStmt) -> Result<(), DbError> {
        self.visit_tableref(&s.table)?;
        if let Some(w) = &s.where_clause {
            self.visit_expr(w)?;
        }
        Ok(())
    }

    // ── FROM / JOIN ──────────────────────────────────────────────────────────

    fn visit_from_clause(&mut self, from: &FromClause) -> Result<(), DbError> {
        match from {
            FromClause::Table(tref) => self.visit_tableref(tref),
            FromClause::Subquery { query, .. } => self.visit_select(query),
        }
    }

    fn visit_joins(&mut self, joins: &[JoinClause]) -> Result<(), DbError> {
        for join in joins {
            self.visit_from_clause(&join.table)?;
        }
        Ok(())
    }

    // ── Expression traversal (for scalar subqueries) ─────────────────────────

    /// Walks SELECT columns, WHERE, HAVING, ORDER BY for `Expr::Subquery` nodes.
    fn visit_exprs_in_select(&mut self, s: &SelectStmt) -> Result<(), DbError> {
        if let Some(w) = &s.where_clause {
            self.visit_expr(w)?;
        }
        if let Some(h) = &s.having {
            self.visit_expr(h)?;
        }
        for item in &s.columns {
            if let SelectItem::Expr { expr, .. } = item {
                self.visit_expr(expr)?;
            }
        }
        for ob in &s.order_by {
            self.visit_expr(&ob.expr)?;
        }
        for gb in &s.group_by {
            self.visit_expr(gb)?;
        }
        if let Some(lim) = &s.limit {
            self.visit_expr(lim)?;
        }
        if let Some(off) = &s.offset {
            self.visit_expr(off)?;
        }
        Ok(())
    }

    fn visit_expr(&mut self, expr: &Expr) -> Result<(), DbError> {
        match expr {
            // Subquery forms — recurse into the inner SELECT.
            Expr::Subquery(sel) => self.visit_select(sel),
            Expr::InSubquery { expr, query, .. } => {
                self.visit_expr(expr)?;
                self.visit_select(query)
            }
            Expr::Exists { query, .. } => self.visit_select(query),

            // Composite expressions — recurse into operands.
            Expr::BinaryOp { left, right, .. } => {
                self.visit_expr(left)?;
                self.visit_expr(right)
            }
            Expr::UnaryOp { operand, .. } => self.visit_expr(operand),
            Expr::Function { args, .. } => {
                for arg in args {
                    self.visit_expr(arg)?;
                }
                Ok(())
            }
            Expr::In { expr, list, .. } => {
                self.visit_expr(expr)?;
                for e in list {
                    self.visit_expr(e)?;
                }
                Ok(())
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.visit_expr(expr)?;
                self.visit_expr(low)?;
                self.visit_expr(high)
            }
            Expr::Like { expr, pattern, .. } => {
                self.visit_expr(expr)?;
                self.visit_expr(pattern)
            }
            Expr::IsNull { expr, .. } => self.visit_expr(expr),
            Expr::Cast { expr, .. } => self.visit_expr(expr),
            Expr::Case {
                operand,
                when_thens,
                else_result,
            } => {
                if let Some(op) = operand {
                    self.visit_expr(op)?;
                }
                for (cond, result) in when_thens {
                    self.visit_expr(cond)?;
                    self.visit_expr(result)?;
                }
                if let Some(e) = else_result {
                    self.visit_expr(e)?;
                }
                Ok(())
            }
            Expr::GroupConcat { expr, order_by, .. } => {
                self.visit_expr(expr)?;
                for (e, _) in order_by {
                    self.visit_expr(e)?;
                }
                Ok(())
            }
            // Leaf nodes — no sub-structure to traverse.
            Expr::Column { .. }
            | Expr::OuterColumn { .. }
            | Expr::Literal(_)
            | Expr::Param { .. } => Ok(()),
        }
    }

    // ── TableRef resolution ──────────────────────────────────────────────────

    fn visit_tableref(&mut self, tref: &TableRef) -> Result<(), DbError> {
        let db = tref.database.as_deref().unwrap_or(self.database);
        let schema = tref.schema.as_deref().unwrap_or(DEFAULT_DATABASE_NAME);

        match self.reader.get_table_in_database(db, schema, &tref.name)? {
            Some(table_def) => {
                // Dedup by table_id — self-joins and aliased refs to the same
                // table produce a single dep entry, not two.
                self.seen
                    .entry(table_def.id)
                    .or_insert(table_def.schema_version);
                Ok(())
            }
            None => {
                // Table not found — the analyzer should have already returned
                // this error. Surface it so the caller knows not to cache.
                Err(DbError::TableNotFound {
                    name: tref.name.clone(),
                })
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `InvalItem` equality and hash are consistent.
    #[test]
    fn inval_item_eq() {
        let a = InvalItem {
            kind: InvalItemKind::Index,
            object_id: 7,
        };
        let b = InvalItem {
            kind: InvalItemKind::Index,
            object_id: 7,
        };
        let c = InvalItem {
            kind: InvalItemKind::Index,
            object_id: 8,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    /// `PlanDeps::is_empty` reflects both tables and items.
    #[test]
    fn plan_deps_is_empty() {
        let empty = PlanDeps::default();
        assert!(empty.is_empty());

        let with_table = PlanDeps {
            tables: vec![(1, 1)],
            items: vec![],
        };
        assert!(!with_table.is_empty());

        let with_item = PlanDeps {
            tables: vec![],
            items: vec![InvalItem {
                kind: InvalItemKind::Index,
                object_id: 3,
            }],
        };
        assert!(!with_item.is_empty());
    }
}
