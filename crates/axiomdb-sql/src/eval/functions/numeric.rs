use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::{BinaryOp, Expr};

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        // ── Numeric functions (4.19) ─────────────────────────────────────────
        "abs" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 argument".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Int(n.abs())),
                Value::BigInt(n) => Ok(Value::BigInt(n.abs())),
                Value::Real(f) => Ok(Value::Real(f.abs())),
                Value::Decimal(m, s) => Ok(Value::Decimal(m.abs(), s)),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "ceil" | "ceiling" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Real(n as f64)),
                Value::BigInt(n) => Ok(Value::Real(n as f64)),
                Value::Real(f) => Ok(Value::Real(f.ceil())),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "floor" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Real(n as f64)),
                Value::BigInt(n) => Ok(Value::Real(n as f64)),
                Value::Real(f) => Ok(Value::Real(f.floor())),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "round" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1+ args".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            let decimals = if args.len() > 1 {
                match crate::eval::eval(&args[1], row)? {
                    Value::Int(d) => d.max(0) as u32,
                    Value::BigInt(d) => d.max(0) as u32,
                    _ => 0,
                }
            } else {
                0
            };
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Int(n)),
                Value::BigInt(n) => Ok(Value::BigInt(n)),
                Value::Real(f) => {
                    let factor = 10f64.powi(decimals as i32);
                    Ok(Value::Real((f * factor).round() / factor))
                }
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "pow" | "power" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let base = crate::eval::eval(&args[0], row)?;
            let exp = crate::eval::eval(&args[1], row)?;
            fn to_f64(v: Value) -> Option<f64> {
                match v {
                    Value::Null => None,
                    Value::Int(n) => Some(n as f64),
                    Value::BigInt(n) => Some(n as f64),
                    Value::Real(f) => Some(f),
                    _ => None,
                }
            }
            match (to_f64(base), to_f64(exp)) {
                (None, _) | (_, None) => Ok(Value::Null),
                (Some(b), Some(e)) => Ok(Value::Real(b.powf(e))),
            }
        }
        "sqrt" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Real((n as f64).sqrt())),
                Value::BigInt(n) => Ok(Value::Real((n as f64).sqrt())),
                Value::Real(f) => Ok(Value::Real(f.sqrt())),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "mod" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let a = crate::eval::eval(&args[0], row)?;
            let b = crate::eval::eval(&args[1], row)?;
            crate::eval::eval(
                &Expr::BinaryOp {
                    op: BinaryOp::Mod,
                    left: Box::new(Expr::Literal(a)),
                    right: Box::new(Expr::Literal(b)),
                },
                &[],
            )
        }
        "sign" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Int(n.signum())),
                Value::BigInt(n) => Ok(Value::BigInt(n.signum())),
                Value::Real(f) => Ok(Value::Real(f.signum())),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        _ => unreachable!("dispatcher routed unsupported numeric function"),
    }
}
