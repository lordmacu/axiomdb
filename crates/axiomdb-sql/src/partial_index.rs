//! Helpers for partial index predicate compilation and evaluation (Phase 6.7).
//!
//! A partial index stores a WHERE predicate as a SQL string. Before row-level
//! operations (INSERT, UPDATE, DELETE) and before query planning, the SQL string
//! must be parsed back to an `Expr` with correctly resolved `col_idx` values.
//!
//! ## Parse-once pattern
//!
//! Callers compile all index predicates **once per statement** before iterating
//! over rows. `compile_index_predicates` returns a `Vec<Option<Expr>>` aligned
//! with the `indexes` slice: `None` for full indexes, `Some(Expr)` for partial.
//!
//! ## Column resolution
//!
//! The parser sets `col_idx = 0` for all column references. `resolve_predicate_columns`
//! walks the Expr tree and replaces each column reference's `col_idx` with the
//! actual index from the table's column definitions.

use axiomdb_catalog::schema::ColumnDef;
use axiomdb_catalog::IndexDef;
use axiomdb_core::error::DbError;

use crate::expr::Expr;

// ── Public API ─────────────────────────────────────────────────────────────────

/// Compiles partial index predicates from SQL strings to resolved `Expr` trees.
///
/// Returns a `Vec` parallel to `indexes`:
/// - `None`  → full index (no predicate); row is always indexed.
/// - `Some(expr)` → partial index; row is indexed only if `eval(expr, row)` is truthy.
///
/// Predicates are compiled once here; callers pass the result to
/// [`insert_into_indexes`] and [`delete_from_indexes`] to avoid re-parsing per row.
pub fn compile_index_predicates(
    indexes: &[IndexDef],
    col_defs: &[ColumnDef],
) -> Result<Vec<Option<Expr>>, DbError> {
    indexes
        .iter()
        .map(|idx| match &idx.predicate {
            None => Ok(None),
            Some(sql) => compile_predicate_sql(sql, col_defs).map(Some),
        })
        .collect()
}

/// Parses and resolves a single predicate SQL expression.
///
/// Wraps `sql` in a dummy `SELECT 1 WHERE <sql>` statement, parses it,
/// extracts the WHERE clause, then resolves column references against `col_defs`.
pub fn compile_predicate_sql(sql: &str, col_defs: &[ColumnDef]) -> Result<Expr, DbError> {
    // Wrap in dummy SELECT to use the existing parser infrastructure.
    let wrapped = format!("SELECT 1 WHERE {sql}");
    let stmt = crate::parser::parse(&wrapped, None)?;

    let where_expr = match stmt {
        crate::ast::Stmt::Select(s) => s.where_clause.ok_or_else(|| DbError::ParseError {
            message: format!("partial index predicate produced no WHERE clause: {sql}"),
        })?,
        _ => {
            return Err(DbError::ParseError {
                message: format!("partial index predicate must be an expression: {sql}"),
            })
        }
    };

    resolve_predicate_columns(where_expr, col_defs)
}

// ── Column reference resolution ───────────────────────────────────────────────

/// Walks an `Expr` tree and replaces each `Column` reference's `col_idx` with
/// the correct value from `col_defs`.
///
/// The parser sets `col_idx = 0` for all column references; this function
/// resolves them to the actual position in the table's column list.
///
/// Returns `DbError::ColumnNotFound` if a column name is not in `col_defs`.
/// Returns `DbError::NotImplemented` for Expr variants that cannot appear in
/// simple index predicates (subqueries, aggregate functions, etc.).
pub fn resolve_predicate_columns(expr: Expr, col_defs: &[ColumnDef]) -> Result<Expr, DbError> {
    match expr {
        Expr::Column { name, .. } => {
            let col = col_defs.iter().find(|c| c.name == name).ok_or_else(|| {
                DbError::ColumnNotFound {
                    name: name.clone(),
                    table: "<partial index predicate>".to_string(),
                }
            })?;
            Ok(Expr::Column {
                name,
                col_idx: col.col_idx as usize,
            })
        }
        Expr::Literal(_) => Ok(expr),
        Expr::IsNull {
            expr: inner,
            negated,
        } => Ok(Expr::IsNull {
            expr: Box::new(resolve_predicate_columns(*inner, col_defs)?),
            negated,
        }),
        Expr::UnaryOp { op, operand } => Ok(Expr::UnaryOp {
            op,
            operand: Box::new(resolve_predicate_columns(*operand, col_defs)?),
        }),
        Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
            left: Box::new(resolve_predicate_columns(*left, col_defs)?),
            op,
            right: Box::new(resolve_predicate_columns(*right, col_defs)?),
        }),
        Expr::Between {
            expr: inner,
            negated,
            low,
            high,
        } => Ok(Expr::Between {
            expr: Box::new(resolve_predicate_columns(*inner, col_defs)?),
            negated,
            low: Box::new(resolve_predicate_columns(*low, col_defs)?),
            high: Box::new(resolve_predicate_columns(*high, col_defs)?),
        }),
        Expr::In {
            expr: inner,
            list,
            negated,
        } => Ok(Expr::In {
            expr: Box::new(resolve_predicate_columns(*inner, col_defs)?),
            list: list
                .into_iter()
                .map(|e| resolve_predicate_columns(e, col_defs))
                .collect::<Result<_, _>>()?,
            negated,
        }),
        Expr::Like {
            expr: inner,
            pattern,
            negated,
        } => Ok(Expr::Like {
            expr: Box::new(resolve_predicate_columns(*inner, col_defs)?),
            pattern: Box::new(resolve_predicate_columns(*pattern, col_defs)?),
            negated,
        }),
        Expr::Function { name, args } => Ok(Expr::Function {
            name,
            args: args
                .into_iter()
                .map(|e| resolve_predicate_columns(e, col_defs))
                .collect::<Result<_, _>>()?,
        }),
        // Subqueries, correlated references, CASE, etc. cannot appear in a
        // partial index predicate — reject them clearly at CREATE INDEX time.
        Expr::Subquery(_)
        | Expr::InSubquery { .. }
        | Expr::Exists { .. }
        | Expr::OuterColumn { .. }
        | Expr::Param { .. }
        | Expr::Case { .. }
        | Expr::Cast { .. } => Err(DbError::NotImplemented {
            feature: "partial index predicate: unsupported expression (subquery/CASE/param/cast)"
                .into(),
        }),
    }
}

// ── Planner: predicate implication ────────────────────────────────────────────

/// Conservative predicate implication check for the query planner.
///
/// Returns `true` if the query's WHERE clause **implies** the index predicate —
/// i.e., every row returned by the query is guaranteed to satisfy the predicate,
/// making the partial index safe to use.
///
/// Phase 6.7 scope: checks only simple atomic predicates:
/// - `col IS NULL`
/// - `col = literal`
/// - `col = TRUE` / `col = FALSE`
///
/// Returns `false` (conservative) for compound predicates or any case where
/// implication cannot be verified. This guarantees correctness at the cost of
/// missing some optimizations (which can be added in Phase 6.9).
pub fn predicate_implied_by_query(
    pred_sql: &str,
    query_where: Option<&Expr>,
    col_defs: &[ColumnDef],
) -> bool {
    let query_expr = match query_where {
        None => return false, // no WHERE → nothing implied
        Some(e) => e,
    };

    // Compile the index predicate.
    let pred_expr = match compile_predicate_sql(pred_sql, col_defs) {
        Ok(e) => e,
        Err(_) => return false, // can't parse → conservative
    };

    // Decompose the query WHERE into AND-clauses and check if any matches.
    let query_clauses = collect_and_clauses(query_expr);
    query_clauses
        .iter()
        .any(|clause| exprs_imply(&pred_expr, clause))
}

/// Collects the leaves of an AND-tree: `a AND b AND c` → `[a, b, c]`.
fn collect_and_clauses(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryOp {
            op: crate::expr::BinaryOp::And,
            left,
            right,
        } => {
            let mut v = collect_and_clauses(left);
            v.extend(collect_and_clauses(right));
            v
        }
        other => vec![other],
    }
}

/// Returns `true` if `clause` from the query WHERE implies `pred` — i.e.,
/// the clause is semantically equivalent to the predicate or stronger.
///
/// Phase 6.7: checks structural equality of simple atomic forms only.
fn exprs_imply(pred: &Expr, clause: &Expr) -> bool {
    match (pred, clause) {
        // col IS NULL matches col IS NULL
        (
            Expr::IsNull {
                expr: p_col,
                negated: false,
            },
            Expr::IsNull {
                expr: c_col,
                negated: false,
            },
        ) => exprs_same_col(p_col, c_col),

        // col IS NOT NULL matches col IS NOT NULL
        (
            Expr::IsNull {
                expr: p_col,
                negated: true,
            },
            Expr::IsNull {
                expr: c_col,
                negated: true,
            },
        ) => exprs_same_col(p_col, c_col),

        // col = literal matches col = same_literal
        (
            Expr::BinaryOp {
                op: crate::expr::BinaryOp::Eq,
                left: p_l,
                right: p_r,
            },
            Expr::BinaryOp {
                op: crate::expr::BinaryOp::Eq,
                left: c_l,
                right: c_r,
            },
        ) => {
            // col = lit
            if exprs_same_col(p_l, c_l) && exprs_same_literal(p_r, c_r) {
                return true;
            }
            // lit = col (reversed)
            if exprs_same_col(p_r, c_r) && exprs_same_literal(p_l, c_l) {
                return true;
            }
            // Mixed reversal: pred is col=lit, clause is lit=col
            if exprs_same_col(p_l, c_r) && exprs_same_literal(p_r, c_l) {
                return true;
            }
            if exprs_same_col(p_r, c_l) && exprs_same_literal(p_l, c_r) {
                return true;
            }
            false
        }

        _ => false, // conservative: can't verify
    }
}

/// Returns `true` if both expressions reference the same column (by col_idx).
fn exprs_same_col(a: &Expr, b: &Expr) -> bool {
    matches!((a, b), (
        Expr::Column { col_idx: ia, .. },
        Expr::Column { col_idx: ib, .. },
    ) if ia == ib)
}

/// Returns `true` if both expressions are equal literals.
fn exprs_same_literal(a: &Expr, b: &Expr) -> bool {
    matches!((a, b), (Expr::Literal(va), Expr::Literal(vb)) if va == vb)
}
