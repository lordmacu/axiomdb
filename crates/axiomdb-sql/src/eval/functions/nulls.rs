use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::{BinaryOp, Expr};

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        // ── Null handling ────────────────────────────────────────────────────
        "coalesce" | "ifnull" | "nvl" => {
            for arg in args {
                let v = crate::eval::eval(arg, row)?;
                if !matches!(v, Value::Null) {
                    return Ok(v);
                }
            }
            Ok(Value::Null)
        }
        "nullif" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 arguments".into(),
                    got: format!("{}", args.len()),
                });
            }
            let a = crate::eval::eval(&args[0], row)?;
            let b = crate::eval::eval(&args[1], row)?;
            let eq = crate::eval::eval(
                &Expr::BinaryOp {
                    op: BinaryOp::Eq,
                    left: Box::new(Expr::Literal(a.clone())),
                    right: Box::new(Expr::Literal(b)),
                },
                &[],
            )?;
            Ok(if crate::eval::is_truthy(&eq) {
                Value::Null
            } else {
                a
            })
        }
        "isnull" => {
            if args.is_empty() {
                return Ok(Value::Bool(true));
            }
            let v = crate::eval::eval(&args[0], row)?;
            Ok(Value::Bool(matches!(v, Value::Null)))
        }
        "if" | "iff" => {
            if args.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "3 arguments for IF(cond, true_val, false_val)".into(),
                    got: format!("{}", args.len()),
                });
            }
            let cond = crate::eval::eval(&args[0], row)?;
            if crate::eval::is_truthy(&cond) {
                crate::eval::eval(&args[1], row)
            } else {
                crate::eval::eval(&args[2], row)
            }
        }

        // ── Type inspection / conversion ─────────────────────────────────────
        "typeof" | "pg_typeof" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            Ok(Value::Text(v.variant_name().into()))
        }
        "to_char" | "str" | "tostring" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                other => Ok(Value::Text(other.to_string())),
            }
        }

        _ => unreachable!("dispatcher routed unsupported null/conversion function"),
    }
}
