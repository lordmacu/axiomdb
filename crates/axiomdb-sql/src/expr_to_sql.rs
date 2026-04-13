//! Converts an [`Expr`] AST node back to a SQL string representation.
//!
//! Used for persisting expressions as text in the catalog (e.g. DEFAULT
//! expressions, CHECK constraints).

use crate::expr::{BinaryOp, Expr, UnaryOp};
use axiomdb_types::Value;

/// Formats an `Expr` as a SQL string suitable for storage in the catalog.
///
/// The output must be re-parseable by `parse_expr_only`.
pub fn expr_to_sql_string(expr: &Expr) -> String {
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
                BinaryOp::JsonSub => "->",
                BinaryOp::JsonContains => "@>",
                BinaryOp::JsonContainedBy => "<@",
                BinaryOp::JsonExists => "?",
                BinaryOp::JsonbPathExists => "@?",
                BinaryOp::JsonbPathMatch => "@@",
                BinaryOp::JsonExistsAny => "?|",
                BinaryOp::JsonExistsAll => "?&",
            };
            format!(
                "({} {op_str} {})",
                expr_to_sql_string(left),
                expr_to_sql_string(right)
            )
        }
        Expr::UnaryOp {
            op: UnaryOp::Not,
            operand,
        } => {
            format!("NOT {}", expr_to_sql_string(operand))
        }
        Expr::UnaryOp {
            op: UnaryOp::Neg,
            operand,
        } => {
            format!("(-{})", expr_to_sql_string(operand))
        }
        Expr::UnaryOp {
            op: UnaryOp::BitNot,
            operand,
        } => {
            format!("(~{})", expr_to_sql_string(operand))
        }
        Expr::IsNull {
            expr: inner,
            negated: false,
        } => {
            format!("{} IS NULL", expr_to_sql_string(inner))
        }
        Expr::IsNull {
            expr: inner,
            negated: true,
        } => {
            format!("{} IS NOT NULL", expr_to_sql_string(inner))
        }
        Expr::Function { name, args } => {
            let arg_str: Vec<String> = args.iter().map(expr_to_sql_string).collect();
            format!("{}({})", name, arg_str.join(", "))
        }
        Expr::Default => "DEFAULT".to_string(),
        // Fallback for complex expressions not yet handled.
        other => format!("{other:?}"),
    }
}
