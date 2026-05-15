//! Converts an [`Expr`] AST node back to a SQL string representation.
//!
//! Used for persisting expressions as text in the catalog (e.g. DEFAULT
//! expressions, CHECK constraints, partial index predicates, expression
//! indexes — Phase 21.8).
//!
//! **Important**: output must be re-parseable by `parse_expr_only`, otherwise
//! index/constraint compilation will fail when the catalog re-hydrates the
//! expression. That is why the fallback for unknown variants returns a string
//! that *clearly* indicates an unsupported expression instead of Debug output
//! (which contains `{`/`}` and confuses the lexer).

use crate::expr::{BinaryOp, Expr, UnaryOp};
use axiomdb_types::Value;

/// Formats an `Expr` as a SQL string suitable for storage in the catalog.
///
/// The output must be re-parseable by `parse_expr_only`.
///
/// Top-level binary/comparison/between/like/in expressions are emitted without
/// wrapping parentheses so the catalog form round-trips to the same string the
/// user wrote (tested by Phase 21.8 expression-index suites). Nested binary
/// expressions still receive parentheses to preserve precedence.
pub fn expr_to_sql_string(expr: &Expr) -> String {
    expr_to_sql_inner(expr, true)
}

fn expr_to_sql_inner(expr: &Expr, top_level: bool) -> String {
    match expr {
        Expr::Literal(v) => match v {
            Value::Int(n) => n.to_string(),
            Value::BigInt(n) => n.to_string(),
            Value::Bool(b) => {
                if *b {
                    "TRUE".to_string()
                } else {
                    "FALSE".to_string()
                }
            }
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Null => "NULL".to_string(),
            Value::Real(f) => f.to_string(),
            _ => format!("{v}"),
        },
        Expr::Column { name, .. } => name.clone(),
        Expr::Collate { expr, collation } => {
            format!("{} COLLATE {}", expr_to_sql_inner(expr, false), collation)
        }
        Expr::BinaryOp { left, op, right } => {
            let op_str = match op {
                BinaryOp::Eq => "=",
                BinaryOp::NotEq => "!=",
                BinaryOp::Lt => "<",
                BinaryOp::LtEq => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::GtEq => ">=",
                BinaryOp::And => "AND",
                BinaryOp::Or => "OR",
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::Concat => "||",
                BinaryOp::Xor => "XOR",
                BinaryOp::NullSafe => "<=>",
                BinaryOp::IntDiv => "DIV",
                BinaryOp::BitAnd => "&",
                BinaryOp::BitOr => "|",
                BinaryOp::BitXor => "^",
                BinaryOp::ShiftLeft => "<<",
                BinaryOp::ShiftRight => ">>",
                BinaryOp::Regexp => "REGEXP",
                BinaryOp::RegexpTilde => "~",
                BinaryOp::RegexpITilde => "~*",
                BinaryOp::RegexpNotTilde => "!~",
                BinaryOp::RegexpNotITilde => "!~*",
                BinaryOp::JsonSub => "->",
                BinaryOp::JsonContains => "@>",
                BinaryOp::JsonContainedBy => "<@",
                BinaryOp::JsonExists => "?",
                BinaryOp::JsonbPathExists => "@?",
                BinaryOp::JsonbPathMatch => "@@",
                BinaryOp::JsonExistsAny => "?|",
                BinaryOp::JsonExistsAll => "?&",
                BinaryOp::JsonPathExtract => "#>",
                BinaryOp::JsonPathExtractText => "#>>",
                BinaryOp::JsonPathDelete => "#-",
                // Phase 20.4, Step 5 — array overlap operator.
                BinaryOp::ArrayOverlap => "&&",
            };
            let body = format!(
                "{} {op_str} {}",
                expr_to_sql_inner(left, false),
                expr_to_sql_inner(right, false)
            );
            if top_level {
                body
            } else {
                format!("({body})")
            }
        }
        Expr::UnaryOp {
            op: UnaryOp::Not,
            operand,
        } => {
            format!("NOT {}", expr_to_sql_inner(operand, false))
        }
        Expr::UnaryOp {
            op: UnaryOp::Neg,
            operand,
        } => {
            format!("(-{})", expr_to_sql_inner(operand, false))
        }
        Expr::UnaryOp {
            op: UnaryOp::BitNot,
            operand,
        } => {
            format!("(~{})", expr_to_sql_inner(operand, false))
        }
        Expr::IsNull {
            expr: inner,
            negated: false,
        } => {
            format!("{} IS NULL", expr_to_sql_inner(inner, false))
        }
        Expr::IsNull {
            expr: inner,
            negated: true,
        } => {
            format!("{} IS NOT NULL", expr_to_sql_inner(inner, false))
        }
        Expr::IsBoolean {
            expr: inner,
            value,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            let v = if *value { "TRUE" } else { "FALSE" };
            format!("{} IS {not}{v}", expr_to_sql_inner(inner, false))
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            let body = format!(
                "{} {not}BETWEEN {} AND {}",
                expr_to_sql_inner(inner, false),
                expr_to_sql_inner(low, false),
                expr_to_sql_inner(high, false),
            );
            if top_level {
                body
            } else {
                format!("({body})")
            }
        }
        Expr::Like {
            expr: inner,
            pattern,
            negated,
            escape,
        } => {
            let not = if *negated { "NOT " } else { "" };
            let esc = escape
                .as_ref()
                .map(|e| format!(" ESCAPE {}", expr_to_sql_inner(e, false)))
                .unwrap_or_default();
            let body = format!(
                "{} {not}LIKE {}{esc}",
                expr_to_sql_inner(inner, false),
                expr_to_sql_inner(pattern, false),
            );
            if top_level {
                body
            } else {
                format!("({body})")
            }
        }
        Expr::In {
            expr: inner,
            list,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            let items: Vec<String> = list.iter().map(|e| expr_to_sql_inner(e, false)).collect();
            let body = format!(
                "{} {not}IN ({})",
                expr_to_sql_inner(inner, false),
                items.join(", ")
            );
            if top_level {
                body
            } else {
                format!("({body})")
            }
        }
        Expr::Function { name, args } => {
            let arg_str: Vec<String> = args.iter().map(|a| expr_to_sql_inner(a, true)).collect();
            // Function names in SQL are case-insensitive. Our parser normalises
            // them to lowercase; when we serialise back for catalog storage we
            // use upper-case because that's the canonical form users write in
            // DDL and several tests (Phase 21.8 expression indexes) round-trip
            // against the upper-cased representation.
            format!("{}({})", name.to_ascii_uppercase(), arg_str.join(", "))
        }
        Expr::Window { func, spec } => {
            let func_name = match func {
                crate::expr::WindowFunc::RowNumber => "ROW_NUMBER",
                crate::expr::WindowFunc::Rank => "RANK",
                crate::expr::WindowFunc::DenseRank => "DENSE_RANK",
            };
            let mut parts = Vec::new();
            if !spec.partition_by.is_empty() {
                parts.push(format!(
                    "PARTITION BY {}",
                    spec.partition_by
                        .iter()
                        .map(|expr| expr_to_sql_inner(expr, true))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !spec.order_by.is_empty() {
                parts.push(format!(
                    "ORDER BY {}",
                    spec.order_by
                        .iter()
                        .map(|item| {
                            let mut s = expr_to_sql_inner(&item.expr, true);
                            if item.order == crate::ast::SortOrder::Desc {
                                s.push_str(" DESC");
                            }
                            if let Some(nulls) = item.nulls {
                                s.push_str(" NULLS ");
                                s.push_str(match nulls {
                                    crate::ast::NullsOrder::First => "FIRST",
                                    crate::ast::NullsOrder::Last => "LAST",
                                });
                            }
                            s
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            format!("{func_name}() OVER ({})", parts.join(" "))
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            let mut s = String::from("CASE");
            if let Some(op) = operand {
                s.push(' ');
                s.push_str(&expr_to_sql_inner(op, false));
            }
            for (cond, res) in when_thens {
                s.push_str(&format!(
                    " WHEN {} THEN {}",
                    expr_to_sql_inner(cond, false),
                    expr_to_sql_inner(res, false)
                ));
            }
            if let Some(e) = else_result {
                s.push_str(&format!(" ELSE {}", expr_to_sql_inner(e, false)));
            }
            s.push_str(" END");
            s
        }
        Expr::Cast {
            expr: inner,
            target,
        } => {
            format!("CAST({} AS {})", expr_to_sql_inner(inner, true), target)
        }
        Expr::Default => "DEFAULT".to_string(),
        // Fallback: emit a clearly-invalid SQL comment-like placeholder rather
        // than Debug output. If this reaches the lexer, re-parse will fail with
        // a readable message instead of a confusing `{` character error.
        other => format!(
            "/* unsupported_expr: {} */",
            std::any::type_name_of_val(other)
        ),
    }
}
