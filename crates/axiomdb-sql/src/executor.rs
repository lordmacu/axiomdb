//! Basic SQL executor — interprets an analyzed [`Stmt`] and produces a [`QueryResult`].
//!
//! ## Entry point
//!
//! [`execute`] is the single public function. It accepts an analyzed statement
//! (all `col_idx` resolved by the semantic analyzer) and drives it to completion,
//! returning a [`QueryResult`].
//!
//! ## Autocommit
//!
//! If no transaction is active when `execute` is called, the statement is
//! automatically wrapped in an implicit `BEGIN / COMMIT` via
//! [`TxnManager::autocommit`]. Transaction control statements (`BEGIN`,
//! `COMMIT`, `ROLLBACK`) bypass autocommit and operate on the `TxnManager`
//! directly.
//!
//! ## Snapshot selection
//!
//! All reads inside a statement use [`TxnManager::active_snapshot`] so that
//! writes made earlier in the same transaction are visible (read-your-own-writes).
//! This is always valid because:
//! - In autocommit mode, `autocommit()` calls `begin()` before invoking the handler.
//! - In explicit transaction mode, `begin()` was already called by the user.
//!
//! ## Phase 4.5 scope
//!
//! Supported: SELECT (with optional WHERE + projection), SELECT without FROM,
//! INSERT VALUES, UPDATE, DELETE, CREATE TABLE, DROP TABLE, CREATE INDEX,
//! DROP INDEX, BEGIN / COMMIT / ROLLBACK, SET (stub).
//!
//! Not yet supported (returns [`DbError::NotImplemented`]):
//! JOIN, GROUP BY, ORDER BY, LIMIT, DISTINCT, subqueries in FROM, INSERT SELECT,
//! TRUNCATE, ALTER TABLE, SHOW TABLES / DESCRIBE.

use std::collections::HashMap;

use axiomdb_catalog::{
    schema::{ColumnDef as CatalogColumnDef, ColumnType, IndexDef, TableDef},
    CatalogReader, CatalogWriter, SchemaResolver,
};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
use axiomdb_storage::{Page, PageType, StorageEngine};
use axiomdb_types::{DataType, Value};
use axiomdb_wal::TxnManager;

use crate::{
    ast::{
        ColumnConstraint, CreateIndexStmt, CreateTableStmt, DeleteStmt, DropIndexStmt,
        DropTableStmt, FromClause, InsertSource, InsertStmt, JoinClause, JoinCondition, JoinType,
        SelectItem, SelectStmt, Stmt, UpdateStmt,
    },
    eval::{eval, is_truthy},
    expr::{BinaryOp, Expr},
    result::{ColumnMeta, QueryResult, Row},
    table::TableEngine,
};

// ── Public entry point ────────────────────────────────────────────────────────

/// Executes a single analyzed SQL statement.
///
/// If no transaction is currently active, the statement is automatically wrapped
/// in an implicit `BEGIN / COMMIT` (autocommit mode). On error in autocommit mode,
/// the transaction is automatically rolled back.
///
/// If a transaction is already active, the executor participates in it without
/// committing — the caller is responsible for `COMMIT` or `ROLLBACK`.
///
/// Transaction control statements (`BEGIN`, `COMMIT`, `ROLLBACK`) operate directly
/// on `txn` regardless of autocommit state.
///
/// # Errors
///
/// Any [`DbError`] from storage, WAL, catalog, type coercion, or expression
/// evaluation. [`DbError::NotImplemented`] is returned for SQL features deferred
/// to later phases (JOIN, GROUP BY, ORDER BY, LIMIT, DISTINCT, subqueries).
pub fn execute(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    if txn.active_txn_id().is_some() {
        // Inside an explicit transaction — execute without auto-committing.
        dispatch(stmt, storage, txn)
    } else {
        // Autocommit: BEGIN + execute + COMMIT, with automatic ROLLBACK on error.
        // We cannot use txn.autocommit() here because its closure signature
        // (FnOnce(&mut TxnManager)) does not provide storage, but dispatch() needs it.
        match stmt {
            Stmt::Begin => {
                txn.begin()?;
                Ok(QueryResult::Empty)
            }
            Stmt::Commit => Err(DbError::NoActiveTransaction),
            Stmt::Rollback => Err(DbError::NoActiveTransaction),
            other => {
                txn.begin()?;
                match dispatch(other, storage, txn) {
                    Ok(result) => {
                        txn.commit()?;
                        Ok(result)
                    }
                    Err(e) => {
                        // Best-effort rollback — ignore rollback error to surface the original.
                        let _ = txn.rollback(storage);
                        Err(e)
                    }
                }
            }
        }
    }
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

/// Routes a statement to its handler. Called both inside `autocommit` and
/// directly when an explicit transaction is already active.
fn dispatch(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    match stmt {
        // DML
        Stmt::Select(s) => execute_select(s, storage, txn),
        Stmt::Insert(s) => execute_insert(s, storage, txn),
        Stmt::Update(s) => execute_update(s, storage, txn),
        Stmt::Delete(s) => execute_delete(s, storage, txn),
        // DDL
        Stmt::CreateTable(s) => execute_create_table(s, storage, txn),
        Stmt::DropTable(s) => execute_drop_table(s, storage, txn),
        Stmt::CreateIndex(s) => execute_create_index(s, storage, txn),
        Stmt::DropIndex(s) => execute_drop_index(s, storage, txn),
        // Transaction control (when already inside a txn)
        Stmt::Begin => Err(DbError::TransactionAlreadyActive {
            txn_id: txn.active_txn_id().unwrap_or(0),
        }),
        Stmt::Commit => {
            txn.commit()?;
            Ok(QueryResult::Empty)
        }
        Stmt::Rollback => {
            txn.rollback(storage)?;
            Ok(QueryResult::Empty)
        }
        // Session variables — stub for Phase 5
        Stmt::Set(_) => Ok(QueryResult::Empty),
        // Deferred statements
        Stmt::TruncateTable(_) => Err(DbError::NotImplemented {
            feature: "TRUNCATE TABLE — Phase 4.21".into(),
        }),
        Stmt::AlterTable(_) => Err(DbError::NotImplemented {
            feature: "ALTER TABLE — Phase 4.22".into(),
        }),
        Stmt::ShowTables(_) | Stmt::ShowColumns(_) => Err(DbError::NotImplemented {
            feature: "SHOW / DESCRIBE — Phase 4.20".into(),
        }),
    }
}

// ── SELECT ───────────────────────────────────────────────────────────────────

fn execute_select(
    mut stmt: SelectStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    // Guard unsupported clauses (ORDER BY, LIMIT, DISTINCT remain unimplemented).
    if !stmt.order_by.is_empty() {
        return Err(DbError::NotImplemented {
            feature: "ORDER BY — Phase 4.10".into(),
        });
    }
    if stmt.limit.is_some() {
        return Err(DbError::NotImplemented {
            feature: "LIMIT — Phase 4.10".into(),
        });
    }
    if stmt.distinct {
        return Err(DbError::NotImplemented {
            feature: "DISTINCT — Phase 4.12".into(),
        });
    }

    // Dispatch based on FROM clause type and whether JOINs are present.
    if stmt.from.is_none() {
        // ── SELECT without FROM ───────────────────────────────────────────────
        let mut out_row: Row = Vec::new();
        let mut out_cols: Vec<ColumnMeta> = Vec::new();
        for item in &stmt.columns {
            match item {
                SelectItem::Expr { expr, alias } => {
                    let v = eval(expr, &[])?;
                    let name = alias
                        .clone()
                        .unwrap_or_else(|| expr_column_name(expr, None));
                    let dt = datatype_of_value(&v);
                    out_cols.push(ColumnMeta::computed(name, dt));
                    out_row.push(v);
                }
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    return Err(DbError::NotImplemented {
                        feature: "SELECT * without FROM".into(),
                    });
                }
            }
        }
        return Ok(QueryResult::Rows {
            columns: out_cols,
            rows: vec![out_row],
        });
    }

    // FROM is present — check for subquery vs table.
    if matches!(stmt.from, Some(FromClause::Subquery { .. })) {
        return Err(DbError::NotImplemented {
            feature: "subquery in FROM — Phase 4.11".into(),
        });
    }

    // Extract the FROM table reference.
    // `stmt` still owns everything; destructure only what we need.
    let from_table_ref = match stmt.from.take() {
        Some(FromClause::Table(tref)) => tref,
        _ => unreachable!("already handled None and Subquery above"),
    };

    if stmt.joins.is_empty() {
        // ── Single-table path (no JOIN) ───────────────────────────────────────
        let resolved = {
            let resolver = make_resolver(storage, txn)?;
            resolver.resolve_table(from_table_ref.schema.as_deref(), &from_table_ref.name)?
        };

        let snap = txn.active_snapshot()?;
        let raw_rows = TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap)?;

        // Collect post-WHERE combined rows (not yet projected).
        let mut combined_rows: Vec<Row> = Vec::new();
        for (_rid, values) in raw_rows {
            if let Some(ref wc) = stmt.where_clause {
                if !is_truthy(&eval(wc, &values)?) {
                    continue;
                }
            }
            combined_rows.push(values);
        }

        // Branch: aggregation or direct projection.
        if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
            return execute_select_grouped(stmt, combined_rows);
        }

        let out_cols = build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
        let rows = combined_rows
            .iter()
            .map(|v| project_row(&stmt.columns, v))
            .collect::<Result<_, _>>()?;
        Ok(QueryResult::Rows {
            columns: out_cols,
            rows,
        })
    } else {
        // ── Multi-table JOIN path ─────────────────────────────────────────────
        execute_select_with_joins(stmt, from_table_ref, storage, txn)
    }
}

// ── JOIN execution ───────────────────────────────────────────────────────────

/// Executes a SELECT with one or more JOINs using nested-loop strategy.
///
/// All tables are pre-scanned once. The combined row is built progressively:
/// - Stage 0: rows from the FROM table
/// - Stage i: `apply_join(stage_{i-1}, scan(JOIN[i].table), ...)`
///
/// WHERE is applied to the fully combined row after all joins.
fn execute_select_with_joins(
    stmt: SelectStmt,
    from_ref: crate::ast::TableRef,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    // Resolve all tables (FROM + each JOIN table) and compute col_offsets.
    let mut all_resolved: Vec<axiomdb_catalog::ResolvedTable> = Vec::new();
    let mut col_offsets: Vec<usize> = Vec::new(); // col_offset[i] = start of table i in combined row
    let mut running_offset = 0usize;

    {
        let resolver = make_resolver(storage, txn)?;
        let from_t = resolver.resolve_table(from_ref.schema.as_deref(), &from_ref.name)?;
        col_offsets.push(running_offset);
        running_offset += from_t.columns.len();
        all_resolved.push(from_t);

        for join in &stmt.joins {
            match &join.table {
                FromClause::Table(tref) => {
                    let jt = resolver.resolve_table(tref.schema.as_deref(), &tref.name)?;
                    col_offsets.push(running_offset);
                    running_offset += jt.columns.len();
                    all_resolved.push(jt);
                }
                FromClause::Subquery { .. } => {
                    return Err(DbError::NotImplemented {
                        feature: "subquery in JOIN — Phase 4.11".into(),
                    })
                }
            }
        }
    } // resolver dropped — storage immutable borrow released

    // Pre-scan all tables once (consistent snapshot for all).
    let snap = txn.active_snapshot()?;
    let mut scanned: Vec<Vec<Row>> = Vec::with_capacity(all_resolved.len());
    for t in &all_resolved {
        let rows = TableEngine::scan_table(storage, &t.def, &t.columns, snap)?;
        scanned.push(rows.into_iter().map(|(_, r)| r).collect());
    }

    // Progressive nested-loop join.
    let mut combined_rows: Vec<Row> = scanned[0].clone();
    let mut left_col_count = all_resolved[0].columns.len();

    // left_schema tracks (col_name, global_col_idx) for all accumulated left columns.
    // Used by USING conditions to locate column positions by name.
    let mut left_schema: Vec<(String, usize)> = all_resolved[0]
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.clone(), i))
        .collect();

    for (i, join) in stmt.joins.iter().enumerate() {
        if join.join_type == JoinType::Full {
            return Err(DbError::NotImplemented {
                feature: "FULL OUTER JOIN — Phase 4.8+".into(),
            });
        }

        let right_idx = i + 1;
        let right_col_count = all_resolved[right_idx].columns.len();
        let right_col_offset = col_offsets[right_idx];

        combined_rows = apply_join(
            combined_rows,
            &scanned[right_idx],
            left_col_count,
            right_col_count,
            join.join_type,
            &join.condition,
            &left_schema,
            right_col_offset,
            &all_resolved[right_idx].columns,
        )?;

        // Extend left_schema with the right table's columns at their global positions.
        for (j, col) in all_resolved[right_idx].columns.iter().enumerate() {
            left_schema.push((col.name.clone(), right_col_offset + j));
        }
        left_col_count += right_col_count;
    }

    // Apply WHERE against the full combined row.
    if let Some(ref wc) = stmt.where_clause {
        let mut filtered = Vec::with_capacity(combined_rows.len());
        for row in combined_rows {
            if is_truthy(&eval(wc, &row)?) {
                filtered.push(row);
            }
        }
        combined_rows = filtered;
    }

    // Branch: aggregation (GROUP BY / aggregate functions) or direct projection.
    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        return execute_select_grouped(stmt, combined_rows);
    }

    // Build output ColumnMeta.
    let out_cols = build_join_column_meta(&stmt.columns, &all_resolved, &stmt.joins)?;

    // Project SELECT list.
    let rows = combined_rows
        .iter()
        .map(|r| project_row(&stmt.columns, r))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Nested-loop join of `left_rows` against `right_rows`.
///
/// Each output row is `left_row ++ right_row`. For OUTER joins, unmatched
/// rows are padded with `Value::Null` on the non-driving side.
///
/// `right_col_offset` is the position of the right table's first column in
/// the combined row. Used only for `JoinCondition::Using`.
#[allow(clippy::too_many_arguments)] // 9 params needed: join context has inherent complexity
fn apply_join(
    left_rows: Vec<Row>,
    right_rows: &[Row],
    left_col_count: usize,
    right_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    left_schema: &[(String, usize)], // for USING: (col_name, global_col_idx) for left side
    right_col_offset: usize,
    right_columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<Vec<Row>, DbError> {
    match join_type {
        JoinType::Inner | JoinType::Cross => {
            let mut result = Vec::new();
            for left in &left_rows {
                for right in right_rows {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                    }
                }
            }
            Ok(result)
        }

        JoinType::Left => {
            let null_right: Row = vec![Value::Null; right_col_count];
            let mut result = Vec::new();
            for left in &left_rows {
                let mut matched = false;
                for right in right_rows {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched = true;
                    }
                }
                if !matched {
                    result.push(concat_rows(left, &null_right));
                }
            }
            Ok(result)
        }

        JoinType::Right => {
            let null_left: Row = vec![Value::Null; left_col_count];
            let mut matched_right = vec![false; right_rows.len()];
            let mut result = Vec::new();

            for left in &left_rows {
                for (i, right) in right_rows.iter().enumerate() {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched_right[i] = true;
                    }
                }
            }
            // Emit unmatched right rows with NULLs on the left side.
            for (i, right) in right_rows.iter().enumerate() {
                if !matched_right[i] {
                    result.push(concat_rows(&null_left, right));
                }
            }
            Ok(result)
        }

        JoinType::Full => Err(DbError::NotImplemented {
            feature: "FULL OUTER JOIN — Phase 4.8+".into(),
        }),
    }
}

/// Evaluates a join condition against a combined row.
///
/// - `On(expr)`: evaluates the expression directly (`col_idx` already resolved by analyzer).
/// - `Using(names)`: for each name, finds its index in the left schema and in the right
///   table, then checks equality. NULL = NULL is UNKNOWN (returns false per SQL semantics).
///
/// `left_schema` is a `(column_name, global_col_idx)` list for every column in the
/// accumulated left side of this join stage.
fn eval_join_cond(
    cond: &JoinCondition,
    combined: &[Value],
    left_schema: &[(String, usize)],
    right_col_offset: usize,
    right_columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<bool, DbError> {
    match cond {
        JoinCondition::On(expr) => Ok(is_truthy(&eval(expr, combined)?)),

        JoinCondition::Using(names) => {
            for col_name in names {
                // Find col_idx in the accumulated left schema.
                let left_idx = left_schema
                    .iter()
                    .find(|(name, _)| name == col_name)
                    .map(|(_, idx)| *idx)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: "left side (USING)".into(),
                    })?;

                // Find col_idx in the right table.
                let right_pos = right_columns
                    .iter()
                    .position(|c| &c.name == col_name)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: "right table (USING)".into(),
                    })?;
                let right_idx = right_col_offset + right_pos;

                let left_val = combined
                    .get(left_idx)
                    .ok_or(DbError::ColumnIndexOutOfBounds {
                        idx: left_idx,
                        len: combined.len(),
                    })?;
                let right_val = combined
                    .get(right_idx)
                    .ok_or(DbError::ColumnIndexOutOfBounds {
                        idx: right_idx,
                        len: combined.len(),
                    })?;

                // NULL = NULL is UNKNOWN in SQL 3-valued logic — no match.
                if matches!(left_val, Value::Null) || matches!(right_val, Value::Null) {
                    return Ok(false);
                }
                if left_val != right_val {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}

/// Concatenates two row slices into a new combined row.
#[inline]
fn concat_rows(left: &[Value], right: &[Value]) -> Row {
    let mut combined = Vec::with_capacity(left.len() + right.len());
    combined.extend_from_slice(left);
    combined.extend_from_slice(right);
    combined
}

/// Builds `ColumnMeta` for the output of a JOIN query.
fn build_join_column_meta(
    items: &[SelectItem],
    all_tables: &[axiomdb_catalog::ResolvedTable],
    joins: &[JoinClause],
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();

    for item in items {
        match item {
            SelectItem::Wildcard => {
                // Expand all columns from all tables in order.
                for (t_idx, table) in all_tables.iter().enumerate() {
                    let outer_nullable = is_outer_nullable(t_idx, joins);
                    for col in &table.columns {
                        out.push(ColumnMeta {
                            name: col.name.clone(),
                            data_type: column_type_to_datatype(col.col_type),
                            nullable: col.nullable || outer_nullable,
                            table_name: Some(table.def.table_name.clone()),
                        });
                    }
                }
            }

            SelectItem::QualifiedWildcard(qualifier) => {
                // Expand only the columns from the matching table.
                let t_idx = all_tables
                    .iter()
                    .position(|t| t.def.table_name == *qualifier || t.def.schema_name == *qualifier)
                    .ok_or_else(|| DbError::TableNotFound {
                        name: qualifier.clone(),
                    })?;
                let table = &all_tables[t_idx];
                let outer_nullable = is_outer_nullable(t_idx, joins);
                for col in &table.columns {
                    out.push(ColumnMeta {
                        name: col.name.clone(),
                        data_type: column_type_to_datatype(col.col_type),
                        nullable: col.nullable || outer_nullable,
                        table_name: Some(table.def.table_name.clone()),
                    });
                }
            }

            SelectItem::Expr { expr, alias } => {
                let name = expr_column_name(expr, alias.as_deref());
                // Infer type: plain column reference uses catalog type; others use Text fallback.
                let (dt, nullable) = infer_expr_type_join(expr, all_tables, joins);
                out.push(ColumnMeta {
                    name,
                    data_type: dt,
                    nullable,
                    table_name: None,
                });
            }
        }
    }
    Ok(out)
}

/// Returns true if the table at `t_idx` can have NULLs due to its position
/// in the join chain.
///
/// - Table 0 (FROM table): nullable if the first join is RIGHT.
/// - Table `i` (i > 0, the i-th JOIN table): nullable if join[i-1] is LEFT.
fn is_outer_nullable(t_idx: usize, joins: &[JoinClause]) -> bool {
    if t_idx == 0 {
        // FROM table is nullable if the first join is RIGHT.
        joins
            .first()
            .is_some_and(|j| j.join_type == JoinType::Right)
    } else {
        // JOIN[t_idx-1] table is nullable if that join is LEFT.
        joins
            .get(t_idx - 1)
            .is_some_and(|j| j.join_type == JoinType::Left)
    }
}

/// Infers (DataType, nullable) for an expression in a JOIN context.
fn infer_expr_type_join(
    expr: &Expr,
    all_tables: &[axiomdb_catalog::ResolvedTable],
    joins: &[JoinClause],
) -> (DataType, bool) {
    if let Expr::Column { col_idx, .. } = expr {
        // Find which table owns this col_idx and what the column type is.
        let mut offset = 0;
        for (t_idx, table) in all_tables.iter().enumerate() {
            let end = offset + table.columns.len();
            if *col_idx < end {
                let local_pos = col_idx - offset;
                if let Some(col) = table.columns.get(local_pos) {
                    let nullable = col.nullable || is_outer_nullable(t_idx, joins);
                    return (column_type_to_datatype(col.col_type), nullable);
                }
            }
            offset = end;
        }
    }
    (DataType::Text, true) // safe fallback for computed expressions
}

// ── GROUP BY / AGGREGATE execution ───────────────────────────────────────────

// ── Aggregate detection ───────────────────────────────────────────────────────

/// Returns `true` if `name` is a known aggregate function.
fn is_aggregate(name: &str) -> bool {
    matches!(name, "count" | "sum" | "min" | "max" | "avg")
}

/// Returns `true` if `expr` or any sub-expression is an aggregate call.
fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function { name, .. } if is_aggregate(name.as_str()) => true,
        Expr::BinaryOp { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::UnaryOp { operand, .. } => contains_aggregate(operand),
        Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::In { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Function { args, .. } => args.iter().any(contains_aggregate),
        Expr::Literal(_) | Expr::Column { .. } => false,
    }
}

/// Returns `true` if the SELECT list or HAVING clause contain any aggregate call.
fn has_aggregates(items: &[SelectItem], having: &Option<Expr>) -> bool {
    let in_select = items.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => contains_aggregate(expr),
        _ => false,
    });
    let in_having = having.as_ref().is_some_and(contains_aggregate);
    in_select || in_having
}

// ── Aggregate descriptor ──────────────────────────────────────────────────────

/// Descriptor for one aggregate expression in the query.
///
/// Collected from the SELECT list and HAVING clause before the scan loop.
/// Deduplicated: if `COUNT(*)` appears in both SELECT and HAVING, only one
/// `AggExpr` is created and both share the same accumulator index.
#[derive(Debug, Clone)]
struct AggExpr {
    /// Lowercase function name: "count", "sum", "min", "max", "avg".
    name: String,
    /// The argument expression. `None` for `COUNT(*)`.
    arg: Option<Expr>,
    /// Position in `GroupState::accumulators`. Preserved for diagnostics.
    #[allow(dead_code)]
    agg_idx: usize,
}

impl AggExpr {
    /// Returns `true` if this descriptor matches the given function call.
    fn matches(&self, name: &str, args: &[Expr]) -> bool {
        if self.name != name {
            return false;
        }
        match (&self.arg, args.first()) {
            // Both COUNT(*): arg = None, args is empty
            (None, None) => args.is_empty(),
            // Both have an argument — compare by col_idx if both are Column refs
            (Some(Expr::Column { col_idx: a, .. }), Some(Expr::Column { col_idx: b, .. })) => {
                a == b
            }
            // One has an arg, the other doesn't
            _ => false,
        }
    }
}

/// Walks `expr` and registers any aggregate function calls into `result`.
fn collect_agg_exprs_from(expr: &Expr, result: &mut Vec<AggExpr>) {
    match expr {
        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let arg = args.first().cloned();
            // Deduplicate: only add if not already registered.
            let already = result.iter().any(|ae| ae.matches(name.as_str(), args));
            if !already {
                let idx = result.len();
                result.push(AggExpr {
                    name: name.clone(),
                    arg,
                    agg_idx: idx,
                });
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_agg_exprs_from(left, result);
            collect_agg_exprs_from(right, result);
        }
        Expr::UnaryOp { operand, .. } => collect_agg_exprs_from(operand, result),
        Expr::IsNull { expr, .. } => collect_agg_exprs_from(expr, result),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_agg_exprs_from(expr, result);
            collect_agg_exprs_from(low, result);
            collect_agg_exprs_from(high, result);
        }
        Expr::In { expr, list, .. } => {
            collect_agg_exprs_from(expr, result);
            for e in list {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_agg_exprs_from(a, result);
            }
        }
        Expr::Literal(_) | Expr::Column { .. } | Expr::Like { .. } => {}
    }
}

/// Builds the deduplicated list of aggregate expressions from SELECT + HAVING.
fn collect_agg_exprs(items: &[SelectItem], having: &Option<Expr>) -> Vec<AggExpr> {
    let mut result = Vec::new();
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_agg_exprs_from(expr, &mut result);
        }
    }
    if let Some(h) = having {
        collect_agg_exprs_from(h, &mut result);
    }
    result
}

// ── Accumulator ───────────────────────────────────────────────────────────────

/// Per-group state for a single aggregate expression.
#[derive(Debug)]
enum AggAccumulator {
    /// `COUNT(*)` — increments for every row.
    CountStar { n: u64 },
    /// `COUNT(col)` — increments only for non-NULL values.
    CountCol { n: u64 },
    /// `SUM(col)` — sum of non-NULL values. `None` = all values were NULL.
    Sum { acc: Option<Value> },
    /// `MIN(col)` — minimum non-NULL value.
    Min { acc: Option<Value> },
    /// `MAX(col)` — maximum non-NULL value.
    Max { acc: Option<Value> },
    /// `AVG(col)` — running sum + count; final = sum / count as Real.
    Avg { sum: Value, count: u64 },
}

impl AggAccumulator {
    fn new(agg: &AggExpr) -> Self {
        match agg.name.as_str() {
            "count" if agg.arg.is_none() => Self::CountStar { n: 0 },
            "count" => Self::CountCol { n: 0 },
            "sum" => Self::Sum { acc: None },
            "min" => Self::Min { acc: None },
            "max" => Self::Max { acc: None },
            "avg" => Self::Avg {
                sum: Value::Int(0),
                count: 0,
            },
            _ => unreachable!("AggAccumulator::new called with non-aggregate"),
        }
    }

    fn update(&mut self, row: &[Value], agg: &AggExpr) -> Result<(), DbError> {
        match self {
            Self::CountStar { n } => *n += 1,

            Self::CountCol { n } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *n += 1;
                }
            }

            Self::Sum { acc } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v,
                        Some(a) => agg_add(a, v)?,
                    });
                }
            }

            Self::Min { acc } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v.clone(),
                        Some(a) => {
                            if agg_compare(&v, &a)? == std::cmp::Ordering::Less {
                                v
                            } else {
                                a
                            }
                        }
                    });
                }
            }

            Self::Max { acc } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v.clone(),
                        Some(a) => {
                            if agg_compare(&v, &a)? == std::cmp::Ordering::Greater {
                                v
                            } else {
                                a
                            }
                        }
                    });
                }
            }

            Self::Avg { sum, count } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *sum = agg_add(sum.clone(), v)?;
                    *count += 1;
                }
            }
        }
        Ok(())
    }

    fn finalize(self) -> Result<Value, DbError> {
        match self {
            Self::CountStar { n } => Ok(Value::BigInt(n as i64)),
            Self::CountCol { n } => Ok(Value::BigInt(n as i64)),
            Self::Sum { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Min { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Max { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Avg { sum, count } => finalize_avg(sum, count),
        }
    }
}

/// Add two values for aggregation (reuses `eval` for type handling and coercion).
fn agg_add(a: Value, b: Value) -> Result<Value, DbError> {
    eval(
        &Expr::BinaryOp {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(a)),
            right: Box::new(Expr::Literal(b)),
        },
        &[],
    )
}

/// Compare two values for MIN/MAX (returns Ordering).
fn agg_compare(a: &Value, b: &Value) -> Result<std::cmp::Ordering, DbError> {
    // Delegate to eval: if a < b → Less, if a = b → Equal, else Greater.
    let lt = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    )?;
    if is_truthy(&lt) {
        return Ok(std::cmp::Ordering::Less);
    }
    let eq = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    )?;
    if is_truthy(&eq) {
        Ok(std::cmp::Ordering::Equal)
    } else {
        Ok(std::cmp::Ordering::Greater)
    }
}

/// Finalize AVG: always produces `Real`. Returns `Null` if count == 0.
fn finalize_avg(sum: Value, count: u64) -> Result<Value, DbError> {
    if count == 0 {
        return Ok(Value::Null);
    }
    // Convert sum to Real.
    let sum_real = match sum {
        Value::Int(n) => Value::Real(n as f64),
        Value::BigInt(n) => Value::Real(n as f64),
        Value::Real(f) => Value::Real(f),
        Value::Decimal(m, s) => Value::Real(m as f64 * 10f64.powi(-(s as i32))),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "numeric".into(),
                got: other.variant_name().into(),
            })
        }
    };
    eval(
        &Expr::BinaryOp {
            op: BinaryOp::Div,
            left: Box::new(Expr::Literal(sum_real)),
            right: Box::new(Expr::Literal(Value::Real(count as f64))),
        },
        &[],
    )
}

// ── GroupState ────────────────────────────────────────────────────────────────

/// State for one GROUP BY group.
struct GroupState {
    /// Evaluated GROUP BY expression values (for future sort-based output — 4.9b).
    #[allow(dead_code)]
    key_values: Vec<Value>,
    /// One source row from this group — used by HAVING/SELECT to resolve column refs.
    representative_row: Row,
    /// One accumulator per aggregate in the query (SELECT + HAVING).
    accumulators: Vec<AggAccumulator>,
}

// ── GROUP BY key hashing ──────────────────────────────────────────────────────

/// Serializes a `Value` to a self-describing byte sequence for use as a
/// GROUP BY hash key.
///
/// Properties:
/// - Two `NULL` values produce identical bytes `[0x00]` → they form one group
///   (SQL grouping semantics: NULLs are considered equal for GROUP BY).
/// - `Real(f64)` uses `to_bits()` for bit-exact representation. `NaN` would
///   produce a fixed bit pattern, but NaN is forbidden in stored values.
/// - The tag byte guarantees values of different types never collide.
fn value_to_key_bytes(v: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    match v {
        Value::Null => buf.push(0x00),
        Value::Bool(b) => {
            buf.push(0x01);
            buf.push(*b as u8);
        }
        Value::Int(n) => {
            buf.push(0x02);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::BigInt(n) => {
            buf.push(0x03);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Real(f) => {
            buf.push(0x04);
            buf.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::Decimal(m, s) => {
            buf.push(0x05);
            buf.extend_from_slice(&m.to_le_bytes());
            buf.push(*s);
        }
        Value::Text(s) => {
            buf.push(0x06);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            buf.push(0x07);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Date(d) => {
            buf.push(0x08);
            buf.extend_from_slice(&d.to_le_bytes());
        }
        Value::Timestamp(t) => {
            buf.push(0x09);
            buf.extend_from_slice(&t.to_le_bytes());
        }
        Value::Uuid(u) => {
            buf.push(0x0A);
            buf.extend_from_slice(u.as_slice());
        }
    }
    buf
}

/// Serializes a GROUP BY key (multiple values) to a single byte sequence.
fn group_key_bytes(key_values: &[Value]) -> Vec<u8> {
    key_values.iter().flat_map(value_to_key_bytes).collect()
}

// ── HAVING evaluator ──────────────────────────────────────────────────────────

/// Evaluates a HAVING expression against a finalized group.
///
/// `Expr::Column` references are evaluated against `representative_row`
/// (the original source row, so `col_idx` values from the analyzer are valid).
///
/// `Expr::Function` aggregate calls are looked up in `agg_values` by name + arg.
///
/// All other expressions are evaluated by delegating sub-expression results to
/// the standard `eval()` via synthetic `Expr::Literal` nodes.
fn eval_with_aggs(
    expr: &Expr,
    representative_row: &[Value],
    agg_values: &[Value],
    agg_exprs: &[AggExpr],
) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),

        Expr::Column { col_idx, .. } => {
            representative_row
                .get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: representative_row.len(),
                })
        }

        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let idx = agg_exprs
                .iter()
                .position(|ae| ae.matches(name.as_str(), args))
                .ok_or_else(|| {
                    DbError::Other(format!(
                        "aggregate '{name}' not pre-registered — internal error"
                    ))
                })?;
            Ok(agg_values[idx].clone())
        }

        // AND: short-circuit
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            match l {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true) => {
                    eval_with_aggs(right, representative_row, agg_values, agg_exprs)
                }
                Value::Null => {
                    let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
                    Ok(if matches!(r, Value::Bool(false)) {
                        Value::Bool(false)
                    } else {
                        Value::Null
                    })
                }
                other => Err(DbError::TypeMismatch {
                    expected: "Bool".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // OR: short-circuit
        Expr::BinaryOp {
            op: BinaryOp::Or,
            left,
            right,
        } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            match l {
                Value::Bool(true) => Ok(Value::Bool(true)),
                Value::Bool(false) => {
                    eval_with_aggs(right, representative_row, agg_values, agg_exprs)
                }
                Value::Null => {
                    let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
                    Ok(if matches!(r, Value::Bool(true)) {
                        Value::Bool(true)
                    } else {
                        Value::Null
                    })
                }
                other => Err(DbError::TypeMismatch {
                    expected: "Bool".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // Other binary ops: evaluate both sides, delegate to eval() via Literal.
        Expr::BinaryOp { op, left, right } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::BinaryOp {
                    op: *op,
                    left: Box::new(Expr::Literal(l)),
                    right: Box::new(Expr::Literal(r)),
                },
                &[],
            )
        }

        Expr::UnaryOp { op, operand } => {
            let v = eval_with_aggs(operand, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::UnaryOp {
                    op: *op,
                    operand: Box::new(Expr::Literal(v)),
                },
                &[],
            )
        }

        Expr::IsNull { expr, negated } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::IsNull {
                    expr: Box::new(Expr::Literal(v)),
                    negated: *negated,
                },
                &[],
            )
        }

        // For remaining variants: fall back to standard eval against representative_row.
        other => eval(other, representative_row),
    }
}

// ── execute_select_grouped ────────────────────────────────────────────────────

/// Executes the GROUP BY + aggregation path.
///
/// `combined_rows` are the post-scan, post-WHERE rows (not yet projected).
/// They are the "source" rows for the GROUP BY grouping.
fn execute_select_grouped(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
) -> Result<QueryResult, DbError> {
    // Build aggregate registry.
    let agg_exprs = collect_agg_exprs(&stmt.columns, &stmt.having);

    // One-pass hash aggregation.
    let mut groups: HashMap<Vec<u8>, GroupState> = HashMap::new();

    for row in &combined_rows {
        // Evaluate GROUP BY expressions → key values.
        let key_values: Vec<Value> = stmt
            .group_by
            .iter()
            .map(|e| eval(e, row))
            .collect::<Result<_, _>>()?;

        let key_bytes = group_key_bytes(&key_values);

        let state = groups.entry(key_bytes).or_insert_with(|| GroupState {
            key_values: key_values.clone(),
            representative_row: row.clone(),
            accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
        });

        // Update each accumulator.
        for (acc, agg) in state.accumulators.iter_mut().zip(&agg_exprs) {
            acc.update(row, agg)?;
        }
    }

    // Ungrouped aggregate: if no GROUP BY and no rows, still emit one output group.
    // (e.g., SELECT COUNT(*) FROM empty_table → returns (0), not 0 rows)
    if stmt.group_by.is_empty() && groups.is_empty() {
        groups.insert(
            vec![],
            GroupState {
                key_values: vec![],
                representative_row: vec![],
                accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
            },
        );
    }

    // Build output column metadata.
    let out_cols = build_grouped_column_meta(&stmt.columns, &agg_exprs)?;

    // Finalize, HAVING filter, project.
    let mut rows: Vec<Row> = Vec::new();
    for (_, state) in groups {
        // Finalize all accumulators.
        let agg_values: Vec<Value> = state
            .accumulators
            .into_iter()
            .map(|acc| acc.finalize())
            .collect::<Result<_, _>>()?;

        // HAVING filter.
        if let Some(ref having) = stmt.having {
            let v = eval_with_aggs(having, &state.representative_row, &agg_values, &agg_exprs)?;
            if !is_truthy(&v) {
                continue;
            }
        }

        // Project SELECT list.
        let out_row = project_grouped_row(
            &stmt.columns,
            &state.representative_row,
            &agg_values,
            &agg_exprs,
        )?;
        rows.push(out_row);
    }

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Projects one output row for the grouped path.
///
/// For each `SelectItem::Expr`:
/// - If the expression contains an aggregate → `eval_with_aggs`
/// - Otherwise → standard `eval` against `representative_row`
fn project_grouped_row(
    items: &[SelectItem],
    representative_row: &[Value],
    agg_values: &[Value],
    agg_exprs: &[AggExpr],
) -> Result<Row, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return Err(DbError::TypeMismatch {
                    expected: "column in GROUP BY or aggregate function".into(),
                    got: "SELECT * (wildcard) with GROUP BY".into(),
                });
            }
            SelectItem::Expr { expr, .. } => {
                let v = if contains_aggregate(expr) {
                    eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?
                } else {
                    eval(expr, representative_row)?
                };
                out.push(v);
            }
        }
    }
    Ok(out)
}

/// Builds `ColumnMeta` for the output of a grouped SELECT.
fn build_grouped_column_meta(
    items: &[SelectItem],
    agg_exprs: &[AggExpr],
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return Err(DbError::TypeMismatch {
                    expected: "column in GROUP BY or aggregate function".into(),
                    got: "SELECT * (wildcard) with GROUP BY".into(),
                });
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| grouped_expr_name(expr, agg_exprs));
                let (dt, nullable) = grouped_expr_type(expr, agg_exprs);
                out.push(ColumnMeta {
                    name,
                    data_type: dt,
                    nullable,
                    table_name: None,
                });
            }
        }
    }
    Ok(out)
}

/// Returns a display name for a grouped SELECT expression.
fn grouped_expr_name(expr: &Expr, _agg_exprs: &[AggExpr]) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            if args.is_empty() {
                format!("{name}(*)")
            } else {
                format!("{name}(...)")
            }
        }
        _ => "?column?".into(),
    }
}

/// Infers `(DataType, nullable)` for a grouped SELECT expression.
/// Aggregate results: COUNT → BigInt non-null; SUM/MIN/MAX/AVG → nullable.
fn grouped_expr_type(expr: &Expr, _agg_exprs: &[AggExpr]) -> (DataType, bool) {
    match expr {
        Expr::Function { name, .. } if is_aggregate(name.as_str()) => match name.as_str() {
            "count" => (DataType::BigInt, false),
            "avg" => (DataType::Real, true),
            _ => (DataType::Text, true), // SUM/MIN/MAX: type depends on column — Text fallback
        },
        Expr::Column { .. } => (DataType::Text, true), // Column refs: safe fallback
        _ => (DataType::Text, true),
    }
}

// ── INSERT ────────────────────────────────────────────────────────────────────

fn execute_insert(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let schema_cols = &resolved.columns;

    // Determine the mapping: schema_column_index → values_row_index (or MAX = Null).
    let col_positions: Vec<usize> = match &stmt.columns {
        None => (0..schema_cols.len()).collect(),
        Some(named_cols) => {
            let mut map = vec![usize::MAX; schema_cols.len()];
            for (val_pos, col_name) in named_cols.iter().enumerate() {
                let schema_pos = schema_cols
                    .iter()
                    .position(|c| &c.name == col_name)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: resolved.def.table_name.clone(),
                    })?;
                map[schema_pos] = val_pos;
            }
            map
        }
    };

    let source_rows = match stmt.source {
        InsertSource::Values(rows) => rows,
        InsertSource::Select(_) => {
            return Err(DbError::NotImplemented {
                feature: "INSERT SELECT — Phase 4.6".into(),
            })
        }
        InsertSource::DefaultValues => {
            return Err(DbError::NotImplemented {
                feature: "DEFAULT VALUES — Phase 4.3c".into(),
            })
        }
    };

    let mut count = 0u64;
    for value_exprs in source_rows {
        // Evaluate each expression against an empty row.
        let provided: Vec<Value> = value_exprs
            .iter()
            .map(|e| eval(e, &[]))
            .collect::<Result<_, _>>()?;

        // Build full_values aligned to the schema column order.
        let full_values: Vec<Value> = col_positions
            .iter()
            .map(|&idx| {
                if idx == usize::MAX {
                    Value::Null
                } else {
                    provided.get(idx).cloned().unwrap_or(Value::Null)
                }
            })
            .collect();

        TableEngine::insert_row(storage, txn, &resolved.def, schema_cols, full_values)?;
        count += 1;
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn execute_update(
    stmt: UpdateStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let schema_cols = resolved.columns.clone();

    // Resolve assignment column positions once, before the scan.
    let assignments: Vec<(usize, Expr)> = stmt
        .assignments
        .into_iter()
        .map(|a| {
            let pos = schema_cols
                .iter()
                .position(|c| c.name == a.column)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: a.column.clone(),
                    table: resolved.def.table_name.clone(),
                })?;
            Ok((pos, a.value))
        })
        .collect::<Result<_, DbError>>()?;

    let snap = txn.active_snapshot()?;
    let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap)?;

    let mut count = 0u64;
    for (rid, current_values) in rows {
        // WHERE filter.
        if let Some(ref wc) = stmt.where_clause {
            if !is_truthy(&eval(wc, &current_values)?) {
                continue;
            }
        }
        // Apply SET assignments.
        let mut new_values = current_values.clone();
        for (col_pos, val_expr) in &assignments {
            new_values[*col_pos] = eval(val_expr, &current_values)?;
        }
        TableEngine::update_row(storage, txn, &resolved.def, &schema_cols, rid, new_values)?;
        count += 1;
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── DELETE ────────────────────────────────────────────────────────────────────

fn execute_delete(
    stmt: DeleteStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let schema_cols = resolved.columns.clone();
    let snap = txn.active_snapshot()?;
    let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap)?;

    // Collect all matching RecordIds BEFORE deleting any row.
    // Deleting a slot during iteration can cause scan to see stale slot states.
    let to_delete: Vec<RecordId> = rows
        .into_iter()
        .filter_map(|(rid, values)| match &stmt.where_clause {
            None => Some(Ok(rid)),
            Some(wc) => match eval(wc, &values) {
                Ok(v) if is_truthy(&v) => Some(Ok(rid)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            },
        })
        .collect::<Result<_, DbError>>()?;

    let count = to_delete.len() as u64;
    for rid in to_delete {
        TableEngine::delete_row(storage, txn, &resolved.def, rid)?;
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── CREATE TABLE ─────────────────────────────────────────────────────────────

fn execute_create_table(
    stmt: CreateTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");

    // Check existence before constructing CatalogWriter (avoids double mutable borrow).
    {
        let resolver = make_resolver(storage, txn)?;
        if resolver.table_exists(Some(schema), &stmt.table.name)? {
            if stmt.if_not_exists {
                return Ok(QueryResult::Empty);
            }
            return Err(DbError::TableAlreadyExists {
                schema: schema.to_string(),
                name: stmt.table.name.clone(),
            });
        }
    } // resolver dropped here — releases immutable borrow on storage

    let mut writer = CatalogWriter::new(storage, txn)?;
    let table_id = writer.create_table(schema, &stmt.table.name)?;

    for (i, col_def) in stmt.columns.iter().enumerate() {
        let col_type = datatype_to_column_type(&col_def.data_type)?;
        let nullable = !col_def
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::NotNull));
        writer.create_column(CatalogColumnDef {
            table_id,
            col_idx: i as u16,
            name: col_def.name.clone(),
            col_type,
            nullable,
        })?;
    }

    Ok(QueryResult::Empty)
}

// ── DROP TABLE ────────────────────────────────────────────────────────────────

fn execute_drop_table(
    stmt: DropTableStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    for table_ref in stmt.tables {
        let schema = table_ref.schema.as_deref().unwrap_or("public");
        let snap = txn.active_snapshot()?;

        let table_id = {
            let reader = CatalogReader::new(storage, snap)?;
            match reader.get_table(schema, &table_ref.name)? {
                Some(def) => def.id,
                None if stmt.if_exists => continue,
                None => {
                    return Err(DbError::TableNotFound {
                        name: table_ref.name.clone(),
                    })
                }
            }
        }; // reader dropped — immutable borrow released

        CatalogWriter::new(storage, txn)?.delete_table(table_id)?;
    }

    Ok(QueryResult::Empty)
}

// ── CREATE INDEX ──────────────────────────────────────────────────────────────

fn execute_create_index(
    stmt: CreateIndexStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");

    let table_id = {
        let resolver = make_resolver(storage, txn)?;
        let resolved = resolver.resolve_table(Some(schema), &stmt.table.name)?;
        resolved.def.id
    }; // resolver dropped

    // Allocate an empty B-Tree root page for this index.
    let root_page_id = storage.alloc_page(PageType::Index)?;
    let root_page = Page::new(PageType::Index, root_page_id);
    storage.write_page(root_page_id, &root_page)?;

    let mut writer = CatalogWriter::new(storage, txn)?;
    writer.create_index(IndexDef {
        index_id: 0, // allocated by create_index
        table_id,
        name: stmt.name.clone(),
        root_page_id,
        is_unique: stmt.unique,
        is_primary: false,
    })?;

    Ok(QueryResult::Empty)
}

// ── DROP INDEX ────────────────────────────────────────────────────────────────

fn execute_drop_index(
    stmt: DropIndexStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot()?;

    // MySQL requires `DROP INDEX name ON table`. If no table is provided, we cannot
    // efficiently search all indexes for Phase 4.5.
    let table_ref = stmt.table.as_ref().ok_or_else(|| DbError::NotImplemented {
        feature: "DROP INDEX without ON table — Phase 4.20".into(),
    })?;

    let schema = table_ref.schema.as_deref().unwrap_or("public");

    let index_id = {
        let reader = CatalogReader::new(storage, snap)?;
        let table_def = match reader.get_table(schema, &table_ref.name)? {
            Some(d) => d,
            None if stmt.if_exists => return Ok(QueryResult::Empty),
            None => {
                return Err(DbError::TableNotFound {
                    name: table_ref.name.clone(),
                })
            }
        };
        let indexes = reader.list_indexes(table_def.id)?;
        indexes
            .into_iter()
            .find(|i| i.name == stmt.name)
            .map(|i| i.index_id)
    }; // reader dropped

    match index_id {
        None if stmt.if_exists => Ok(QueryResult::Empty),
        None => Err(DbError::NotImplemented {
            feature: format!("DROP INDEX — index '{}' not found", stmt.name),
        }),
        Some(id) => {
            CatalogWriter::new(storage, txn)?.delete_index(id)?;
            Ok(QueryResult::Empty)
        }
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Creates a [`SchemaResolver`] using the current snapshot.
///
/// Uses `active_snapshot()` when a transaction is active, falling back to
/// `snapshot()` for read-only access outside a transaction.
fn make_resolver<'a>(
    storage: &'a dyn StorageEngine,
    txn: &TxnManager,
) -> Result<SchemaResolver<'a>, DbError> {
    let snap: TransactionSnapshot = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    SchemaResolver::new(storage, snap, "public")
}

/// Converts a SQL [`DataType`] (from the AST) to the compact [`ColumnType`] stored
/// in the catalog. Returns [`DbError::NotImplemented`] for types not yet in the catalog.
fn datatype_to_column_type(dt: &DataType) -> Result<ColumnType, DbError> {
    match dt {
        DataType::Bool => Ok(ColumnType::Bool),
        DataType::Int => Ok(ColumnType::Int),
        DataType::BigInt => Ok(ColumnType::BigInt),
        DataType::Real => Ok(ColumnType::Float),
        DataType::Text => Ok(ColumnType::Text),
        DataType::Bytes => Ok(ColumnType::Bytes),
        DataType::Timestamp => Ok(ColumnType::Timestamp),
        DataType::Uuid => Ok(ColumnType::Uuid),
        DataType::Decimal => Err(DbError::NotImplemented {
            feature: "DECIMAL column type — Phase 4.3".into(),
        }),
        DataType::Date => Err(DbError::NotImplemented {
            feature: "DATE column type — Phase 4.19".into(),
        }),
    }
}

/// Converts a compact catalog [`ColumnType`] back to the full [`DataType`].
fn column_type_to_datatype(ct: ColumnType) -> DataType {
    match ct {
        ColumnType::Bool => DataType::Bool,
        ColumnType::Int => DataType::Int,
        ColumnType::BigInt => DataType::BigInt,
        ColumnType::Float => DataType::Real,
        ColumnType::Text => DataType::Text,
        ColumnType::Bytes => DataType::Bytes,
        ColumnType::Timestamp => DataType::Timestamp,
        ColumnType::Uuid => DataType::Uuid,
    }
}

/// Returns the [`DataType`] that best describes a runtime [`Value`].
/// Used for computing `ColumnMeta.data_type` for computed SELECT expressions.
fn datatype_of_value(v: &Value) -> DataType {
    match v {
        Value::Null => DataType::Text, // unknown type — use Text as fallback
        Value::Bool(_) => DataType::Bool,
        Value::Int(_) => DataType::Int,
        Value::BigInt(_) => DataType::BigInt,
        Value::Real(_) => DataType::Real,
        Value::Decimal(..) => DataType::Decimal,
        Value::Text(_) => DataType::Text,
        Value::Bytes(_) => DataType::Bytes,
        Value::Date(_) => DataType::Date,
        Value::Timestamp(_) => DataType::Timestamp,
        Value::Uuid(_) => DataType::Uuid,
    }
}

/// Infers the `(DataType, nullable)` pair for a SELECT expression.
///
/// For plain column references, uses the catalog type. For all other expressions,
/// returns `(DataType::Text, true)` as a safe fallback (proper type inference is Phase 6).
fn infer_expr_type(expr: &Expr, columns: &[CatalogColumnDef]) -> (DataType, bool) {
    match expr {
        Expr::Column { col_idx, .. } => {
            if let Some(col) = columns.get(*col_idx) {
                (column_type_to_datatype(col.col_type), col.nullable)
            } else {
                (DataType::Text, true)
            }
        }
        _ => (DataType::Text, true),
    }
}

/// Returns the output name for a SELECT expression item.
fn expr_column_name(expr: &Expr, alias: Option<&str>) -> String {
    if let Some(a) = alias {
        return a.to_string();
    }
    match expr {
        Expr::Column { name, .. } => name.clone(),
        _ => "?column?".to_string(),
    }
}

/// Builds the [`ColumnMeta`] vector for the output of a SELECT statement.
fn build_select_column_meta(
    items: &[SelectItem],
    columns: &[CatalogColumnDef],
    table_def: &TableDef,
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                for col in columns {
                    out.push(ColumnMeta {
                        name: col.name.clone(),
                        data_type: column_type_to_datatype(col.col_type),
                        nullable: col.nullable,
                        table_name: Some(table_def.table_name.clone()),
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = expr_column_name(expr, alias.as_deref());
                let (dt, nullable) = infer_expr_type(expr, columns);
                out.push(ColumnMeta {
                    name,
                    data_type: dt,
                    nullable,
                    table_name: None,
                });
            }
        }
    }
    Ok(out)
}

/// Projects a raw heap row through a SELECT item list to produce the output row.
fn project_row(items: &[SelectItem], values: &[Value]) -> Result<Row, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                out.extend_from_slice(values);
            }
            SelectItem::Expr { expr, .. } => {
                out.push(eval(expr, values)?);
            }
        }
    }
    Ok(out)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_datatype_to_column_type_supported() {
        assert_eq!(
            datatype_to_column_type(&DataType::Bool).unwrap(),
            ColumnType::Bool
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Int).unwrap(),
            ColumnType::Int
        );
        assert_eq!(
            datatype_to_column_type(&DataType::BigInt).unwrap(),
            ColumnType::BigInt
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Real).unwrap(),
            ColumnType::Float
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Text).unwrap(),
            ColumnType::Text
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Bytes).unwrap(),
            ColumnType::Bytes
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Timestamp).unwrap(),
            ColumnType::Timestamp
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Uuid).unwrap(),
            ColumnType::Uuid
        );
    }

    #[test]
    fn test_datatype_to_column_type_unsupported() {
        assert!(matches!(
            datatype_to_column_type(&DataType::Decimal),
            Err(DbError::NotImplemented { .. })
        ));
        assert!(matches!(
            datatype_to_column_type(&DataType::Date),
            Err(DbError::NotImplemented { .. })
        ));
    }

    #[test]
    fn test_column_type_to_datatype_roundtrip() {
        for &dt in &[
            DataType::Bool,
            DataType::Int,
            DataType::BigInt,
            DataType::Real,
            DataType::Text,
            DataType::Bytes,
            DataType::Timestamp,
            DataType::Uuid,
        ] {
            let ct = datatype_to_column_type(&dt).unwrap();
            assert_eq!(column_type_to_datatype(ct), dt);
        }
    }

    #[test]
    fn test_expr_column_name_alias_wins() {
        let expr = Expr::Literal(Value::Int(1));
        assert_eq!(expr_column_name(&expr, Some("total")), "total");
    }

    #[test]
    fn test_expr_column_name_column_expr() {
        let expr = Expr::Column {
            name: "age".into(),
            col_idx: 0,
        };
        assert_eq!(expr_column_name(&expr, None), "age");
    }

    #[test]
    fn test_expr_column_name_other_expr_fallback() {
        let expr = Expr::Literal(Value::Int(1));
        assert_eq!(expr_column_name(&expr, None), "?column?");
    }
}
