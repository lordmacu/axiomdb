// ── AUTO_INCREMENT sequence state ─────────────────────────────────────────────

thread_local! {
    /// Per-table AUTO_INCREMENT sequence counter (TableId → next value to assign).
    /// Initialized lazily: on first auto-insert, the executor scans the table to
    /// find MAX(auto_col) and seeds the counter from MAX+1.
    static AUTO_INC_SEQ: RefCell<StdHashMap<u32, u64>> = RefCell::new(StdHashMap::new());

    /// The last auto-generated ID produced by this thread.
    /// Read by `LAST_INSERT_ID()` / `lastval()` in the expression evaluator.
    static THREAD_LAST_INSERT_ID: Cell<u64> = const { Cell::new(0) };

    /// Pre-LIMIT row count stored by SQL_CALC_FOUND_ROWS (4.5e).
    /// Read by `FOUND_ROWS()` function.
    static THREAD_FOUND_ROWS: Cell<u64> = const { Cell::new(0) };

    /// Per-thread `ConnectionTxn` for the legacy single-connection `execute()` API.
    ///
    /// When `execute(BEGIN, ...)` is called, the `ConnectionTxn` is stored here so
    /// that subsequent `execute(INSERT/UPDATE/..., ...)` calls can retrieve it to
    /// pass down to executor functions that need the connection-level state.
    /// Consumed by `execute(COMMIT/ROLLBACK, ...)`.
    static EXECUTE_CONN: RefCell<Option<ConnectionTxn>> = const { RefCell::new(None) };
}

/// Returns the value of `LAST_INSERT_ID()` for the current thread.
/// Exported so `eval.rs` can call it from `eval_function`.
pub(crate) fn last_insert_id_value() -> u64 {
    THREAD_LAST_INSERT_ID.with(|v| v.get())
}

/// Returns the next AUTO_INCREMENT value for `col_idx` in `table_def`
/// and bumps the per-table cache. On first call (cache miss), scans the
/// table to seed from `MAX(col) + 1`. Mirrors `next_auto_inc_ctx` in
/// `insert_heap_ctx.rs:94-123` but exposed as a public helper so the
/// embedded `Appender` (Attack 7 v1.1) can use the same machinery
/// without going through the SQL pipeline.
///
/// `col_idx` must be the column index of an `AUTO_INCREMENT` column in
/// `table_def`. Caller is responsible for asserting that.
///
/// Returns a `u64` so the caller can map it into either `Value::Int`
/// or `Value::BigInt` based on the column's declared type.
pub fn next_auto_increment_value(
    storage: &dyn axiomdb_storage::StorageEngine,
    txn: &axiomdb_wal::TxnManager,
    conn_txn: &ConnectionTxn,
    table_def: &axiomdb_catalog::schema::TableDef,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    col_idx: usize,
) -> Result<u64, axiomdb_core::error::DbError> {
    let table_id = table_def.id;
    let cached = AUTO_INC_SEQ.with(|seq| seq.borrow().get(&table_id).copied());
    if let Some(next) = cached {
        AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
        return Ok(next);
    }
    let snap = txn.active_snapshot(conn_txn);
    let rows = crate::table::TableEngine::scan_table(storage, table_def, schema_cols, snap, None)?;
    let max_existing: u64 = rows
        .iter()
        .filter_map(|(_, vals)| vals.get(col_idx))
        .filter_map(|v| match v {
            axiomdb_types::Value::Int(n) => Some(*n as u64),
            axiomdb_types::Value::BigInt(n) => Some(*n as u64),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let next = max_existing + 1;
    AUTO_INC_SEQ.with(|seq| seq.borrow_mut().insert(table_id, next + 1));
    Ok(next)
}

/// Sets the value of `LAST_INSERT_ID()` for the current thread.
/// Called by `LAST_INSERT_ID(expr)` 1-arg form (4.14b).
pub(crate) fn set_last_insert_id(id: u64) {
    THREAD_LAST_INSERT_ID.with(|v| v.set(id));
}

/// Returns the `FOUND_ROWS()` value for the current thread (4.5e).
pub(crate) fn found_rows_value() -> u64 {
    THREAD_FOUND_ROWS.with(|v| v.get())
}

/// Stores the pre-LIMIT row count for `FOUND_ROWS()` (4.5e).
pub(crate) fn set_found_rows(n: u64) {
    THREAD_FOUND_ROWS.with(|v| v.set(n));
}

/// Returns the correct snapshot for analyzing a statement before calling [`execute`].
///
/// When an explicit transaction is active (i.e. `execute(BEGIN, ...)` was called),
/// returns the active transaction snapshot — which includes uncommitted DDL/DML from
/// that transaction so that `CREATE TABLE` + `INSERT` in the same `BEGIN...COMMIT`
/// block work correctly.
///
/// Falls back to `txn.snapshot()` (committed data only) when no active transaction
/// is pending in the thread-local.
///
/// This is the correct function to use in test helpers that call `analyze()` before
/// `execute()`.
pub fn execute_snapshot(txn: &axiomdb_wal::TxnManager) -> axiomdb_core::TransactionSnapshot {
    EXECUTE_CONN.with(|cell| {
        if let Some(conn) = cell.borrow().as_ref() {
            txn.active_snapshot(conn)
        } else {
            txn.snapshot()
        }
    })
}

// ── Subquery materialization cache ────────────────────────────────────────────

/// Cache for uncorrelated subquery results.
///
/// When a subquery contains no `OuterColumn` references, its result is identical
/// for every outer row. We materialize it once and reuse the result, turning
/// O(n × cost(inner)) into O(n + cost(inner)).
///
/// Keyed by the pointer address of the `SelectStmt` AST node, which is stable
/// within a single query evaluation (the AST lives on the heap and is not moved).
type SubqueryCache = HashMap<usize, QueryResult>;

/// Cached `InSubquerySet` for O(1) membership tests in `IN (SELECT …)`.
///
/// Keyed identically to `SubqueryCache` (AST pointer address). Built once on
/// first access, then reused for all subsequent outer rows.
type InSetCache = HashMap<usize, InSubquerySet>;

/// Cache for correlated scalar subquery results keyed by parameter values.
///
/// Inspired by MariaDB's `Expression_cache_tmptable` (sql_expression_cache.h):
/// when a correlated scalar subquery like `(SELECT SUM(amount) FROM orders
/// WHERE user_id = outer.id)` is evaluated per outer row, the result depends
/// only on the correlated parameter values (`outer.id`). We cache by:
///
///   (AST pointer, hash of OuterColumn values) → QueryResult
///
/// This turns O(n × cost(inner)) into O(distinct_keys × cost(inner)) which
/// is often O(n) when every outer row has a unique key, but avoids re-execution
/// for duplicate key values and amortizes the overhead of substitute_outer +
/// execute_select_ctx.
///
/// For the common benchmark pattern (1:N join with unique outer PK), this cache
/// has ~100% miss rate and doesn't help. The real win comes from avoiding the
/// `substitute_outer(stmt.clone(), ...)` overhead by pre-extracting the
/// correlated column indices and using a direct HashMap lookup.
type CorrelatedCache = HashMap<(usize, u64), QueryResult>;

// ── Phase 11.12: Correlated subquery materialization ─────────────────────────

/// Info extracted when a correlated subquery matches the materializable pattern:
/// single equijoin `inner.col = OuterColumn(idx)` with an aggregate result.
struct MaterializableInfo {
    /// Index into the outer row that provides the join key.
    outer_col_idx: usize,
    /// Resolved column index in the inner query that is equi-joined.
    inner_col_idx: usize,
    /// Column name in the inner query (for SELECT/GROUP BY).
    inner_col_name: String,
}

/// Materialized lookup table: outer key → subquery result row.
type MaterializedMap = HashMap<crate::eval::core::HashableValue, Vec<Value>>;

/// Per-subquery materialization state: (outer_col_idx, map, column_meta).
/// `None` means the pattern didn't match — don't retry.
type MaterializedEntry = Option<(usize, MaterializedMap, Vec<crate::result::ColumnMeta>)>;

/// Cache of materialization attempts, keyed by AST pointer.
type MaterializedCache = HashMap<usize, MaterializedEntry>;

/// Detects if a correlated scalar subquery matches the materializable pattern:
/// - Single `inner.col = OuterColumn(idx)` in WHERE
/// - No other OuterColumn refs anywhere in the statement
/// - No LIMIT / OFFSET
///
/// PostgreSQL rewrites these at planner level (`convert_ANY_sublink_to_join`).
/// AxiomDB detects at execution time (cheaper, no planner changes).
fn detect_materializable_pattern(stmt: &SelectStmt) -> Option<MaterializableInfo> {
    // Must have a WHERE clause.
    let where_clause = stmt.where_clause.as_ref()?;

    // Must not have LIMIT or OFFSET (semantics would change).
    if stmt.limit.is_some() || stmt.offset.is_some() {
        return None;
    }

    // Find a single `col = OuterColumn(idx)` or `OuterColumn(idx) = col` in WHERE.
    // We only handle the top-level AND conjuncts — nested OR is not materializable.
    let mut found: Option<(usize, usize, String)> = None;
    let mut other_outer_in_where = false;

    fn scan_eq(expr: &Expr, found: &mut Option<(usize, usize, String)>, other: &mut bool) {
        match expr {
            Expr::BinaryOp {
                op: BinaryOp::Eq,
                left,
                right,
            } => {
                // Check: OuterColumn = Column or Column = OuterColumn
                // Only depth-0 refs (immediate parent) qualify for this opt;
                // deeper correlation is handled by the generic row-by-row path.
                match (left.as_ref(), right.as_ref()) {
                    (
                        Expr::OuterColumn {
                            col_idx: outer_idx,
                            depth: 0,
                            ..
                        },
                        Expr::Column {
                            col_idx: inner_idx,
                            name,
                            ..
                        },
                    )
                    | (
                        Expr::Column {
                            col_idx: inner_idx,
                            name,
                            ..
                        },
                        Expr::OuterColumn {
                            col_idx: outer_idx,
                            depth: 0,
                            ..
                        },
                    ) => {
                        if found.is_none() {
                            *found = Some((*outer_idx, *inner_idx, name.clone()));
                        } else {
                            *other = true;
                        }
                        return;
                    }
                    _ => {}
                }
                if expr_has_outer_ref(left) || expr_has_outer_ref(right) {
                    *other = true;
                }
            }
            Expr::BinaryOp {
                op: BinaryOp::And,
                left,
                right,
            } => {
                scan_eq(left, found, other);
                scan_eq(right, found, other);
            }
            _ => {
                if expr_has_outer_ref(expr) {
                    *other = true;
                }
            }
        }
    }

    scan_eq(where_clause, &mut found, &mut other_outer_in_where);

    if other_outer_in_where {
        return None;
    }

    let (outer_col_idx, inner_col_idx, inner_col_name) = found?;

    // Verify no OuterColumn refs in SELECT, HAVING, GROUP BY, ORDER BY.
    let has_outer_elsewhere = stmt.columns.iter().any(|item| {
        if let SelectItem::Expr { expr, .. } = item {
            expr_has_outer_ref(expr)
        } else {
            false
        }
    }) || stmt.having.as_ref().is_some_and(expr_has_outer_ref)
        || stmt.group_by.exprs().iter().any(expr_has_outer_ref)
        || stmt.order_by.iter().any(|o| expr_has_outer_ref(&o.expr))
        || stmt.distinct_on.iter().any(expr_has_outer_ref);

    if has_outer_elsewhere {
        return None;
    }

    Some(MaterializableInfo {
        outer_col_idx,
        inner_col_idx,
        inner_col_name,
    })
}

/// Materializes the inner query by rewriting it with GROUP BY on the join column,
/// executing it ONCE, and building a HashMap for O(1) lookup per outer row.
///
/// Example:
///   Input:  SELECT SUM(amount) FROM orders WHERE user_id = OuterColumn(0)
///   Rewrite: SELECT user_id, SUM(amount) FROM orders GROUP BY user_id
///   Output: HashMap { 1 → [500], 2 → [300], ... }
fn materialize_correlated_subquery(
    stmt: &SelectStmt,
    info: &MaterializableInfo,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<(MaterializedMap, Vec<crate::result::ColumnMeta>), DbError> {
    use crate::eval::core::HashableValue;

    // Build the rewritten query: add join column to SELECT + GROUP BY, remove equijoin from WHERE.
    let mut rewritten = stmt.clone();

    // Add the inner join column as the FIRST select item.
    let join_col_expr = Expr::Column {
        col_idx: info.inner_col_idx,
        name: info.inner_col_name.clone(),
    };
    rewritten.columns.insert(
        0,
        SelectItem::Expr {
            expr: join_col_expr.clone(),
            alias: Some("__mat_key".into()),
        },
    );

    // Add GROUP BY on the join column (if not already present).
    let already_grouped = rewritten.group_by.exprs().iter().any(|e| match e {
        Expr::Column { col_idx, .. } => *col_idx == info.inner_col_idx,
        _ => false,
    });
    if !already_grouped {
        use crate::ast::GroupByClause;
        let mut exprs = rewritten.group_by.exprs().to_vec();
        exprs.insert(0, join_col_expr);
        rewritten.group_by = GroupByClause::Simple(exprs);
    }

    // Remove the equijoin predicate from WHERE (replace OuterColumn refs with always-true).
    rewritten.where_clause = strip_outer_equijoin(&rewritten.where_clause);

    // Execute the rewritten query once.
    let exec_ctx = ExecutionContext::new(storage, txn, bloom, None);
    let conn = ctx.conn_txn.take();
    let result = execute_select_ctx(rewritten, &exec_ctx, conn.as_ref(), ctx);
    ctx.conn_txn = conn;
    let result = result?;

    match result {
        QueryResult::Rows { columns, rows } => {
            let mut map: MaterializedMap = HashMap::with_capacity(rows.len());
            // The first column is the join key, remaining columns are the original result.
            for row in rows {
                if row.is_empty() {
                    continue;
                }
                let key = HashableValue(row[0].clone());
                let value_row: Vec<Value> = row[1..].to_vec();
                map.insert(key, value_row);
            }
            // Column metadata without the join key column.
            let result_cols = if columns.len() > 1 {
                columns[1..].to_vec()
            } else {
                vec![]
            };
            Ok((map, result_cols))
        }
        _ => Err(DbError::Internal {
            message: "materialized subquery returned non-rows result".into(),
        }),
    }
}

/// Strips the `OuterColumn = col` equijoin from a WHERE clause, leaving only
/// non-correlated predicates. Returns None if nothing remains.
fn strip_outer_equijoin(where_clause: &Option<Expr>) -> Option<Expr> {
    let expr = where_clause.as_ref()?;
    strip_outer_eq_inner(expr)
}

fn strip_outer_eq_inner(expr: &Expr) -> Option<Expr> {
    match expr {
        Expr::BinaryOp {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            let left_is_outer = matches!(left.as_ref(), Expr::OuterColumn { .. });
            let right_is_outer = matches!(right.as_ref(), Expr::OuterColumn { .. });
            let is_outer_eq = left_is_outer || right_is_outer;
            if is_outer_eq {
                None // Remove this predicate.
            } else {
                Some(expr.clone())
            }
        }
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let l = strip_outer_eq_inner(left);
            let r = strip_outer_eq_inner(right);
            match (l, r) {
                (Some(l), Some(r)) => Some(Expr::BinaryOp {
                    op: BinaryOp::And,
                    left: Box::new(l),
                    right: Box::new(r),
                }),
                (Some(e), None) | (None, Some(e)) => Some(e),
                (None, None) => None,
            }
        }
        _ => Some(expr.clone()),
    }
}

/// Extracts the `OuterColumn` indices referenced by a `SelectStmt`.
/// Used to compute cache keys for correlated subqueries.
fn extract_outer_col_indices(stmt: &SelectStmt) -> Vec<usize> {
    let mut indices = Vec::new();
    fn walk_expr(expr: &Expr, indices: &mut Vec<usize>) {
        match expr {
            Expr::OuterColumn { col_idx, .. } => {
                #[allow(clippy::collapsible_match)]
                if !indices.contains(col_idx) {
                    indices.push(*col_idx);
                }
            }
            Expr::UnaryOp { operand, .. } => walk_expr(operand, indices),
            Expr::BinaryOp { left, right, .. } => {
                walk_expr(left, indices);
                walk_expr(right, indices);
            }
            Expr::IsNull { expr, .. } | Expr::IsBoolean { expr, .. } | Expr::Cast { expr, .. } => {
                walk_expr(expr, indices)
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                walk_expr(expr, indices);
                walk_expr(low, indices);
                walk_expr(high, indices);
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                walk_expr(expr, indices);
                walk_expr(pattern, indices);
                if let Some(e) = escape {
                    walk_expr(e, indices);
                }
            }
            Expr::In { expr, list, .. } => {
                walk_expr(expr, indices);
                for e in list {
                    walk_expr(e, indices);
                }
            }
            Expr::Function { args, .. } => {
                for a in args {
                    walk_expr(a, indices);
                }
            }
            Expr::Case {
                operand,
                when_thens,
                else_result,
                ..
            } => {
                if let Some(op) = operand {
                    walk_expr(op, indices);
                }
                for (w, t) in when_thens {
                    walk_expr(w, indices);
                    walk_expr(t, indices);
                }
                if let Some(e) = else_result {
                    walk_expr(e, indices);
                }
            }
            Expr::Subquery(inner) => walk_stmt(inner, indices),
            Expr::InSubquery { expr, query, .. } => {
                walk_expr(expr, indices);
                walk_stmt(query, indices);
            }
            Expr::Exists { query, .. } => walk_stmt(query, indices),
            _ => {}
        }
    }
    fn walk_stmt(stmt: &SelectStmt, indices: &mut Vec<usize>) {
        for item in &stmt.columns {
            if let SelectItem::Expr { expr, .. } = item {
                walk_expr(expr, indices);
            }
        }
        if let Some(ref wc) = stmt.where_clause {
            walk_expr(wc, indices);
        }
        if let Some(ref h) = stmt.having {
            walk_expr(h, indices);
        }
        for e in stmt.group_by.exprs() {
            walk_expr(e, indices);
        }
        for ob in &stmt.order_by {
            walk_expr(&ob.expr, indices);
        }
        for join in &stmt.joins {
            if let JoinCondition::On(ref e) = join.condition {
                walk_expr(e, indices);
            }
        }
    }
    walk_stmt(stmt, &mut indices);
    indices.sort_unstable();
    indices
}

/// Computes a hash key from the outer row values at the given column indices.
fn hash_outer_params(outer_row: &[Value], col_indices: &[usize]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for &idx in col_indices {
        let val = outer_row.get(idx).cloned().unwrap_or(Value::Null);
        crate::eval::core::HashableValue(val).hash(&mut hasher);
    }
    hasher.finish()
}

/// Returns `true` if the expression tree contains any `Expr::OuterColumn` node,
/// meaning the subquery is correlated with an enclosing scope.
fn expr_has_outer_ref(expr: &Expr) -> bool {
    match expr {
        Expr::OuterColumn { .. } => true,
        Expr::UnaryOp { operand, .. } => expr_has_outer_ref(operand),
        Expr::BinaryOp { left, right, .. } => expr_has_outer_ref(left) || expr_has_outer_ref(right),
        Expr::IsNull { expr, .. } => expr_has_outer_ref(expr),
        Expr::IsBoolean { expr, .. } => expr_has_outer_ref(expr),
        Expr::Between {
            expr, low, high, ..
        } => expr_has_outer_ref(expr) || expr_has_outer_ref(low) || expr_has_outer_ref(high),
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_has_outer_ref(expr)
                || expr_has_outer_ref(pattern)
                || escape.as_ref().is_some_and(|e| expr_has_outer_ref(e))
        }
        Expr::In { expr, list, .. } => {
            expr_has_outer_ref(expr) || list.iter().any(expr_has_outer_ref)
        }
        Expr::Function { args, .. } => args.iter().any(expr_has_outer_ref),
        Expr::Case {
            operand,
            when_thens,
            else_result,
            ..
        } => {
            operand.as_ref().is_some_and(|e| expr_has_outer_ref(e))
                || when_thens
                    .iter()
                    .any(|(w, t)| expr_has_outer_ref(w) || expr_has_outer_ref(t))
                || else_result.as_ref().is_some_and(|e| expr_has_outer_ref(e))
        }
        Expr::Cast { expr, .. } => expr_has_outer_ref(expr),
        Expr::Subquery(inner) => stmt_has_outer_ref(inner),
        Expr::InSubquery { expr, query, .. } => {
            expr_has_outer_ref(expr) || stmt_has_outer_ref(query)
        }
        Expr::Exists { query, .. } => stmt_has_outer_ref(query),
        Expr::GroupConcat { expr, order_by, .. } => {
            expr_has_outer_ref(expr) || order_by.iter().any(|(e, _)| expr_has_outer_ref(e))
        }
        Expr::ArrayAgg { expr, order_by, .. } => {
            expr_has_outer_ref(expr) || order_by.iter().any(|(e, _)| expr_has_outer_ref(e))
        }
        Expr::Grouping { args, .. } => args.iter().any(expr_has_outer_ref),
        // Leaves: Literal, Column, Default, Param — no outer refs.
        _ => false,
    }
}

/// Returns `true` if the `SelectStmt` references any `OuterColumn` anywhere.
fn stmt_has_outer_ref(stmt: &SelectStmt) -> bool {
    // Check columns (SELECT list).
    for item in &stmt.columns {
        if let SelectItem::Expr { expr, .. } = item {
            if expr_has_outer_ref(expr) {
                return true;
            }
        }
    }
    // WHERE clause.
    if let Some(ref wc) = stmt.where_clause {
        if expr_has_outer_ref(wc) {
            return true;
        }
    }
    // HAVING clause.
    if let Some(ref h) = stmt.having {
        if expr_has_outer_ref(h) {
            return true;
        }
    }
    // GROUP BY expressions.
    if stmt.group_by.exprs().iter().any(expr_has_outer_ref) {
        return true;
    }
    // ORDER BY expressions.
    if stmt.order_by.iter().any(|ob| expr_has_outer_ref(&ob.expr)) {
        return true;
    }
    // JOIN conditions.
    for join in &stmt.joins {
        if let JoinCondition::On(ref e) = join.condition {
            if expr_has_outer_ref(e) {
                return true;
            }
        }
    }
    false
}

// ── Subquery execution support ────────────────────────────────────────────────

/// Walks a `SelectStmt` AST and substitutes `Expr::OuterColumn` nodes that
/// refer to the caller's scope with `Expr::Literal(outer_row[col_idx])`.
///
/// Called once per outer row for correlated subqueries. Uncorrelated subqueries
/// contain no `OuterColumn` nodes — `substitute_outer` is a no-op for them.
///
/// Nested subqueries are walked with `binding_depth + 1` so that refs pointing
/// to the caller's scope through multiple layers of nesting are still resolved
/// correctly (GAP-C.8).
/// Phase 21.9 — public within the crate so joins.rs can use it for
/// LATERAL-correlated subquery materialization.
pub(crate) fn substitute_outer(stmt: SelectStmt, outer_row: &[Value]) -> SelectStmt {
    substitute_outer_at(stmt, outer_row, 0)
}

fn substitute_outer_at(
    mut stmt: SelectStmt,
    outer_row: &[Value],
    binding_depth: u16,
) -> SelectStmt {
    stmt.where_clause = stmt
        .where_clause
        .map(|e| subst_expr(e, outer_row, binding_depth));
    stmt.columns = stmt
        .columns
        .into_iter()
        .map(|item| match item {
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: subst_expr(expr, outer_row, binding_depth),
                alias,
            },
            other => other,
        })
        .collect();
    stmt.having = stmt.having.map(|e| subst_expr(e, outer_row, binding_depth));
    {
        use crate::ast::GroupByClause;
        let resolved: Vec<_> = stmt.group_by.exprs().iter()
            .map(|e| subst_expr(e.clone(), outer_row, binding_depth))
            .collect();
        stmt.group_by = match stmt.group_by {
            GroupByClause::Simple(_) => GroupByClause::Simple(resolved),
            GroupByClause::WithRollup(_) => GroupByClause::WithRollup(resolved),
            GroupByClause::Sets { sets, .. } => GroupByClause::Sets { universe: resolved, sets },
            GroupByClause::None => GroupByClause::None,
        };
    }
    stmt.order_by = stmt
        .order_by
        .into_iter()
        .map(|mut item| {
            item.expr = subst_expr(item.expr, outer_row, binding_depth);
            item
        })
        .collect();
    stmt.distinct_on = stmt
        .distinct_on
        .into_iter()
        .map(|e| subst_expr(e, outer_row, binding_depth))
        .collect();
    stmt.joins = stmt
        .joins
        .into_iter()
        .map(|mut join| {
            use crate::ast::JoinCondition;
            join.condition = match join.condition {
                JoinCondition::On(e) => JoinCondition::On(subst_expr(e, outer_row, binding_depth)),
                other => other,
            };
            join
        })
        .collect();
    stmt
}

/// Recursively replaces `OuterColumn` nodes whose `depth` matches
/// `binding_depth` with `Literal` values from `outer_row`. OuterColumn nodes
/// at greater depth (referring to scopes further out than the current binding)
/// are left intact — they will be bound by an outer substitution pass.
///
/// Supports correlation at arbitrary nesting depth (GAP-C.8): when recursing
/// into a nested subquery, `binding_depth` is incremented so that deep refs
/// to the caller's row resolve through multiple scope layers.
fn subst_expr(expr: Expr, outer_row: &[Value], binding_depth: u16) -> Expr {
    match expr {
        Expr::OuterColumn {
            col_idx,
            name,
            depth,
        } => {
            if depth == binding_depth {
                Expr::Literal(outer_row.get(col_idx).cloned().unwrap_or(Value::Null))
            } else {
                Expr::OuterColumn {
                    col_idx,
                    name,
                    depth,
                }
            }
        }
        Expr::UnaryOp { op, operand } => Expr::UnaryOp {
            op,
            operand: Box::new(subst_expr(*operand, outer_row, binding_depth)),
        },
        Expr::BinaryOp { op, left, right } => Expr::BinaryOp {
            op,
            left: Box::new(subst_expr(*left, outer_row, binding_depth)),
            right: Box::new(subst_expr(*right, outer_row, binding_depth)),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(subst_expr(*expr, outer_row, binding_depth)),
            negated,
        },
        Expr::IsBoolean {
            expr,
            value,
            negated,
        } => Expr::IsBoolean {
            expr: Box::new(subst_expr(*expr, outer_row, binding_depth)),
            value,
            negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(subst_expr(*expr, outer_row, binding_depth)),
            low: Box::new(subst_expr(*low, outer_row, binding_depth)),
            high: Box::new(subst_expr(*high, outer_row, binding_depth)),
            negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
            escape,
        } => Expr::Like {
            expr: Box::new(subst_expr(*expr, outer_row, binding_depth)),
            pattern: Box::new(subst_expr(*pattern, outer_row, binding_depth)),
            negated,
            escape: escape.map(|e| Box::new(subst_expr(*e, outer_row, binding_depth))),
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: Box::new(subst_expr(*expr, outer_row, binding_depth)),
            list: list
                .into_iter()
                .map(|e| subst_expr(e, outer_row, binding_depth))
                .collect(),
            negated,
        },
        Expr::Function { name, args } => Expr::Function {
            name,
            args: args
                .into_iter()
                .map(|a| subst_expr(a, outer_row, binding_depth))
                .collect(),
        },
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => Expr::Case {
            operand: operand.map(|e| Box::new(subst_expr(*e, outer_row, binding_depth))),
            when_thens: when_thens
                .into_iter()
                .map(|(w, t)| {
                    (
                        subst_expr(w, outer_row, binding_depth),
                        subst_expr(t, outer_row, binding_depth),
                    )
                })
                .collect(),
            else_result: else_result.map(|e| Box::new(subst_expr(*e, outer_row, binding_depth))),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(subst_expr(*expr, outer_row, binding_depth)),
            target,
        },
        Expr::Subquery(inner) => Expr::Subquery(Box::new(substitute_outer_at(
            *inner,
            outer_row,
            binding_depth + 1,
        ))),
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(subst_expr(*expr, outer_row, binding_depth)),
            query: Box::new(substitute_outer_at(*query, outer_row, binding_depth + 1)),
            negated,
        },
        Expr::Exists { query, negated } => Expr::Exists {
            query: Box::new(substitute_outer_at(*query, outer_row, binding_depth + 1)),
            negated,
        },
        Expr::GroupConcat {
            expr,
            distinct,
            order_by,
            separator,
        } => Expr::GroupConcat {
            expr: Box::new(subst_expr(*expr, outer_row, binding_depth)),
            distinct,
            order_by: order_by
                .into_iter()
                .map(|(e, dir)| (subst_expr(e, outer_row, binding_depth), dir))
                .collect(),
            separator,
        },
        Expr::ArrayAgg {
            expr,
            distinct,
            order_by,
        } => Expr::ArrayAgg {
            expr: Box::new(subst_expr(*expr, outer_row, binding_depth)),
            distinct,
            order_by: order_by
                .into_iter()
                .map(|(e, dir)| (subst_expr(e, outer_row, binding_depth), dir))
                .collect(),
        },
        Expr::Grouping { args, universe_indices } => Expr::Grouping {
            args: args.into_iter().map(|a| subst_expr(a, outer_row, binding_depth)).collect(),
            universe_indices,
        },
        other => other,
    }
}

/// [`SubqueryRunner`] that executes inner queries through the executor,
/// substituting outer-row references before running.
///
/// Holds shared refs to `storage`, `txn`, and `bloom`, a mutable ref to `ctx`,
/// plus the current outer row for `substitute_outer`.
///
/// An optional `&mut SubqueryCache` enables materialization of uncorrelated
/// subqueries: when a `SelectStmt` contains no `OuterColumn` references, the
/// result is cached on first execution and reused for subsequent outer rows.
/// This turns O(n × cost(inner)) into O(n + cost(inner)).
struct ExecSubqueryRunner<'a> {
    storage: &'a dyn StorageEngine,
    txn: &'a TxnManager,
    bloom: &'a crate::bloom::BloomRegistry,
    ctx: &'a mut SessionContext,
    outer_row: &'a [Value],
    cache: Option<&'a mut SubqueryCache>,
    in_set_cache: Option<&'a mut InSetCache>,
    correlated_cache: Option<&'a mut CorrelatedCache>,
    /// Phase 11.12: materialized correlated subquery lookup tables.
    /// Keyed by AST pointer. Built once on first cache miss, reused for all outer rows.
    materialized: Option<&'a mut MaterializedCache>,
}

impl<'a> SubqueryRunner for ExecSubqueryRunner<'a> {
    fn eval_function(
        &mut self,
        name: &str,
        args: &[Expr],
        row: &[Value],
    ) -> Result<Option<Value>, DbError> {
        if let Some(v) = eval_sequence_function(name, args, row, self)? {
            return Ok(Some(v));
        }
        if let Some(v) = eval_cron_function(name, args, row, self)? {
            return Ok(Some(v));
        }
        if let Some(v) = eval_business_calendar_function(name, args, row, self)? {
            return Ok(Some(v));
        }
        eval_money_function(name, args, row, self)
    }

    fn run(&mut self, stmt: &SelectStmt) -> Result<QueryResult, DbError> {
        let cache_key = std::ptr::from_ref(stmt) as usize;
        let is_uncorrelated = !stmt_has_outer_ref(stmt);

        // Fast path 1: uncorrelated subquery — cache by AST pointer.
        if is_uncorrelated {
            if let Some(ref cache) = self.cache {
                if let Some(cached) = cache.get(&cache_key) {
                    return Ok(cached.clone());
                }
            }
        }

        // Phase 11.12: materialized correlated subquery — O(1) hash lookup.
        // For single-equijoin patterns (inner.col = OuterColumn(idx)):
        // execute the inner query ONCE with GROUP BY, build HashMap, then
        // lookup per outer row. Turns O(N × M) into O(N + M).
        if !is_uncorrelated {
            if let Some(ref mut mat_cache) = self.materialized {
                mat_cache.entry(cache_key).or_insert_with(|| {
                    detect_materializable_pattern(stmt).and_then(|info| {
                        let outer_idx = info.outer_col_idx;
                        materialize_correlated_subquery(
                            stmt,
                            &info,
                            self.storage,
                            self.txn,
                            self.bloom,
                            self.ctx,
                        )
                        .ok()
                        .map(|(map, cols)| (outer_idx, map, cols))
                    })
                });
                if let Some(Some((outer_idx, ref map, ref cols))) = mat_cache.get(&cache_key) {
                    let key = crate::eval::core::HashableValue(
                        self.outer_row
                            .get(*outer_idx)
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                    return if let Some(row) = map.get(&key) {
                        Ok(QueryResult::Rows {
                            columns: cols.clone(),
                            rows: vec![row.clone()],
                        })
                    } else {
                        // No match — return single row with NULLs (aggregate over empty set).
                        let null_row = cols.iter().map(|_| Value::Null).collect();
                        Ok(QueryResult::Rows {
                            columns: cols.clone(),
                            rows: vec![null_row],
                        })
                    };
                }
                // Pattern didn't match or materialization failed — fall through.
            }
        }

        // Fast path 2: correlated subquery — cache by (AST pointer, param hash).
        if !is_uncorrelated {
            if let Some(ref corr_cache) = self.correlated_cache {
                let outer_indices = extract_outer_col_indices(stmt);
                let param_hash = hash_outer_params(self.outer_row, &outer_indices);
                let corr_key = (cache_key, param_hash);
                if let Some(cached) = corr_cache.get(&corr_key) {
                    return Ok(cached.clone());
                }
            }
        }

        let bound = substitute_outer(stmt.clone(), self.outer_row);
        let exec_ctx = ExecutionContext::new(self.storage, self.txn, self.bloom, None);
        let conn = self.ctx.conn_txn.take();
        let r = execute_select_ctx(bound, &exec_ctx, conn.as_ref(), self.ctx);
        self.ctx.conn_txn = conn;

        let result = r?;

        // Store in cache for reuse by subsequent outer rows.
        if is_uncorrelated {
            if let Some(ref mut cache) = self.cache {
                cache.insert(cache_key, result.clone());
            }
        } else if let Some(ref mut corr_cache) = self.correlated_cache {
            let outer_indices = extract_outer_col_indices(stmt);
            let param_hash = hash_outer_params(self.outer_row, &outer_indices);
            corr_cache.insert((cache_key, param_hash), result.clone());
        }

        Ok(result)
    }

    fn run_in_check(&mut self, stmt: &SelectStmt, needle: &Value) -> Result<(bool, bool), DbError> {
        let cache_key = std::ptr::from_ref(stmt) as usize;
        let is_uncorrelated = !stmt_has_outer_ref(stmt);

        // Fast path: probe cached HashSet in O(1).
        if is_uncorrelated {
            if let Some(ref in_set_cache) = self.in_set_cache {
                if let Some(set) = in_set_cache.get(&cache_key) {
                    return Ok(set.contains(needle));
                }
            }
        }

        // Execute the subquery.
        let result = self.run(stmt)?;

        if is_uncorrelated {
            // Build a HashSet for O(1) lookups on subsequent outer rows.
            let set = InSubquerySet::from_query_result(result);
            let answer = set.contains(needle);
            if let Some(ref mut in_set_cache) = self.in_set_cache {
                in_set_cache.insert(cache_key, set);
            }
            Ok(answer)
        } else {
            // Correlated: linear scan (result changes per outer row).
            let rows = match result {
                QueryResult::Rows { rows, .. } => rows,
                _ => return Ok((false, false)),
            };
            let mut found = false;
            let mut has_null = false;
            for row in &rows {
                let v = row.first().cloned().unwrap_or(Value::Null);
                match v {
                    Value::Null => has_null = true,
                    ref iv if *iv == *needle => {
                        found = true;
                        break;
                    }
                    _ => {}
                }
            }
            Ok((found, has_null))
        }
    }
}

// ── EXISTS decorrelation to hash semi-join ────────────────────────────────────
//
// Inspired by PostgreSQL's InitPlan + hash semi-join (subselect.c) and
// DataFusion's `decorrelate_predicate_subquery.rs` which rewrites
// `WHERE EXISTS (SELECT ... FROM t WHERE t.col = outer.col AND ...)` into a
// LEFT SEMI JOIN.
//
// We implement a lightweight version at the executor level: before entering the
// per-row WHERE evaluation loop, we detect the EXISTS pattern, execute the inner
// query once, build a HashSet of the join key values, and then filter the outer
// rows with O(1) probes.
//
// This turns O(n × cost(inner)) into O(n + m) where m = inner table rows.

/// Result of analyzing a WHERE clause for EXISTS decorrelation.
struct ExistsDecorrelation {
    /// Column index in the outer row used as the equijoin key.
    outer_col_idx: usize,
    /// Column index in the inner table used as the equijoin key.
    inner_col_idx: usize,
    /// The inner `SelectStmt` with OuterColumn replaced by a plain Column
    /// so it can be executed standalone (the equijoin predicate is removed).
    inner_stmt: SelectStmt,
    /// Additional non-correlated filter predicates (e.g., `o.amount > 80`).
    /// Already embedded in `inner_stmt.where_clause`.
    negated: bool,
}

/// Tries to extract an EXISTS decorrelation from the WHERE clause.
///
/// Matches the pattern:
/// ```sql
/// WHERE [NOT] EXISTS (
///   SELECT ... FROM <table>
///   WHERE <inner_col> = OuterColumn(<idx>) [AND <extra_filters>]
/// )
/// ```
///
/// Returns `None` if the pattern doesn't match (falls back to per-row eval).
fn try_extract_exists_decorrelation(wc: &Expr) -> Option<ExistsDecorrelation> {
    let (query, negated) = match wc {
        Expr::Exists { query, negated } => (query, *negated),
        _ => return None,
    };

    // Must have a FROM clause (single table, no joins in the subquery itself).
    if query.from.is_none() || !query.joins.is_empty() {
        return None;
    }

    // Must have a WHERE clause with correlation.
    let inner_where = query.where_clause.as_ref()?;

    // Split WHERE into conjuncts (AND-separated predicates).
    let conjuncts = split_and_conjuncts(inner_where);

    // Find exactly one conjunct that is an equijoin with OuterColumn.
    let mut outer_col_idx = None;
    let mut inner_col_idx = None;
    let mut non_correlated: Vec<Expr> = Vec::new();

    for conj in &conjuncts {
        if let Some((oc, ic)) = extract_equijoin_outer_inner(conj) {
            if outer_col_idx.is_some() {
                // Multiple equijoin conditions — too complex, bail out.
                return None;
            }
            outer_col_idx = Some(oc);
            inner_col_idx = Some(ic);
        } else if expr_has_outer_ref(conj) {
            // Non-equijoin correlation — can't decorrelate with hash semi-join.
            return None;
        } else {
            non_correlated.push(conj.clone());
        }
    }

    let outer_col_idx = outer_col_idx?;
    let inner_col_idx = inner_col_idx?;

    // Build a standalone inner statement without the correlated predicate.
    let mut inner_stmt = query.as_ref().clone();
    inner_stmt.where_clause = if non_correlated.is_empty() {
        None
    } else {
        Some(rebuild_and_conjunction(non_correlated))
    };
    // Rewrite SELECT list to include the join key column.
    // We need the inner_col_idx value in the result to build the HashSet.
    inner_stmt.columns = vec![SelectItem::Wildcard];
    // Remove ORDER BY / LIMIT (irrelevant for semi-join).
    inner_stmt.order_by.clear();
    inner_stmt.limit = None;
    inner_stmt.offset = None;

    Some(ExistsDecorrelation {
        outer_col_idx,
        inner_col_idx,
        inner_stmt,
        negated,
    })
}

/// Extracts an equijoin predicate between an `OuterColumn` and an inner `Column`.
///
/// Returns `(outer_col_idx, inner_col_idx)` if the expression is:
/// - `Column(inner) = OuterColumn(outer)` or
/// - `OuterColumn(outer) = Column(inner)`
fn extract_equijoin_outer_inner(expr: &Expr) -> Option<(usize, usize)> {
    if let Expr::BinaryOp {
        op: BinaryOp::Eq,
        left,
        right,
    } = expr
    {
        match (left.as_ref(), right.as_ref()) {
            (
                Expr::OuterColumn {
                    col_idx: oc,
                    depth: 0,
                    ..
                },
                Expr::Column { col_idx: ic, .. },
            ) => Some((*oc, *ic)),
            (
                Expr::Column { col_idx: ic, .. },
                Expr::OuterColumn {
                    col_idx: oc,
                    depth: 0,
                    ..
                },
            ) => Some((*oc, *ic)),
            _ => None,
        }
    } else {
        None
    }
}

/// Splits an expression tree on AND boundaries into individual conjuncts.
fn split_and_conjuncts(expr: &Expr) -> Vec<Expr> {
    let mut out = Vec::new();
    split_and_conjuncts_inner(expr, &mut out);
    out
}

fn split_and_conjuncts_inner(expr: &Expr, out: &mut Vec<Expr>) {
    if let Expr::BinaryOp {
        op: BinaryOp::And,
        left,
        right,
    } = expr
    {
        split_and_conjuncts_inner(left, out);
        split_and_conjuncts_inner(right, out);
    } else {
        out.push(expr.clone());
    }
}

/// Rebuilds a conjunction (AND chain) from a list of expressions.
fn rebuild_and_conjunction(mut exprs: Vec<Expr>) -> Expr {
    assert!(!exprs.is_empty());
    let mut result = exprs.pop().unwrap();
    while let Some(e) = exprs.pop() {
        result = Expr::BinaryOp {
            op: BinaryOp::And,
            left: Box::new(e),
            right: Box::new(result),
        };
    }
    result
}

/// Executes the EXISTS decorrelation as a hash semi-join.
///
/// 1. Executes the inner query once (standalone, no outer refs).
/// 2. Builds a HashSet of the inner join key values.
/// 3. Filters outer rows by probing the HashSet.
///
/// O(n + m) instead of O(n × m).
fn apply_exists_semijoin(
    outer_rows: Vec<(RecordId, Row)>,
    decorr: &ExistsDecorrelation,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
) -> Result<Vec<Row>, DbError> {
    // Execute inner query once.
    let mut temp_ctx = SessionContext::new();
    let temp_bloom = crate::bloom::BloomRegistry::new();
    let exec_ctx = ExecutionContext::new(storage, txn, &temp_bloom, None);
    let result = execute_select_ctx(decorr.inner_stmt.clone(), &exec_ctx, None, &mut temp_ctx)?;

    // Build HashSet from the inner join key column.
    let inner_rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => vec![],
    };
    let mut key_set: std::collections::HashSet<crate::eval::core::HashableValue> =
        std::collections::HashSet::with_capacity(inner_rows.len());
    for row in &inner_rows {
        if let Some(val) = row.get(decorr.inner_col_idx) {
            if !matches!(val, Value::Null) {
                key_set.insert(crate::eval::core::HashableValue(val.clone()));
            }
        }
    }

    // Filter outer rows by probing the HashSet.
    let mut combined = Vec::new();
    for (_rid, values) in outer_rows {
        let outer_key = values
            .get(decorr.outer_col_idx)
            .cloned()
            .unwrap_or(Value::Null);
        let matches = if matches!(outer_key, Value::Null) {
            false
        } else {
            key_set.contains(&crate::eval::core::HashableValue(outer_key))
        };
        let keep = if decorr.negated { !matches } else { matches };
        if keep {
            combined.push(values);
        }
    }
    Ok(combined)
}
