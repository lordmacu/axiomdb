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

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::ast::{InsertSource, SelectItem, Stmt};
use crate::expr::Expr;

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
            expr: e,
            low,
            high,
            ..
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
