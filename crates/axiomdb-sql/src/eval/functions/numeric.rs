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
                    // MySQL uses round-half-away-from-zero, not IEEE 754 banker's rounding.
                    let shifted = f * factor;
                    let rounded = if shifted >= 0.0 {
                        shifted.floor() + if shifted.fract() >= 0.5 { 1.0 } else { 0.0 }
                    } else {
                        shifted.ceil() - if (-shifted).fract() >= 0.5 { 1.0 } else { 0.0 }
                    };
                    Ok(Value::Real(rounded / factor))
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

        // ── Trigonometric / logarithmic (4.19h) ─────────────────────────────────
        "pi" => Ok(Value::Real(std::f64::consts::PI)),

        "exp" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match to_f64_val(v) {
                None => Ok(Value::Null),
                Some(f) => Ok(Value::Real(f.exp())),
            }
        }

        "log" => match args.len() {
            0 => Err(DbError::TypeMismatch {
                expected: "1 or 2 args".into(),
                got: "0".into(),
            }),
            1 => {
                let v = crate::eval::eval(&args[0], row)?;
                match to_f64_val(v) {
                    None => Ok(Value::Null),
                    Some(f) => Ok(Value::Real(f.ln())),
                }
            }
            _ => {
                let base = to_f64_val(crate::eval::eval(&args[0], row)?);
                let x = to_f64_val(crate::eval::eval(&args[1], row)?);
                match (base, x) {
                    (None, _) | (_, None) => Ok(Value::Null),
                    (Some(b), Some(v)) => Ok(Value::Real(v.log(b))),
                }
            }
        },

        "log2" => one_trig(args, row, f64::log2),
        "log10" => one_trig(args, row, f64::log10),
        "sin" => one_trig(args, row, f64::sin),
        "cos" => one_trig(args, row, f64::cos),
        "tan" => one_trig(args, row, f64::tan),
        "asin" => one_trig(args, row, f64::asin),
        "acos" => one_trig(args, row, f64::acos),
        "cot" => one_trig(args, row, |f| 1.0 / f.tan()),
        "radians" => one_trig(args, row, f64::to_radians),
        "degrees" => one_trig(args, row, f64::to_degrees),

        "atan" | "atan2" => {
            if args.len() == 2 {
                let y = to_f64_val(crate::eval::eval(&args[0], row)?);
                let x = to_f64_val(crate::eval::eval(&args[1], row)?);
                match (y, x) {
                    (None, _) | (_, None) => Ok(Value::Null),
                    (Some(y), Some(x)) => Ok(Value::Real(y.atan2(x))),
                }
            } else {
                one_trig(args, row, f64::atan)
            }
        }

        "truncate" | "trunc" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1+ args".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            let d: i32 = if args.len() > 1 {
                match crate::eval::eval(&args[1], row)? {
                    Value::Int(n) => n,
                    Value::BigInt(n) => n as i32,
                    _ => 0,
                }
            } else {
                0
            };
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Int(if d >= 0 {
                    n
                } else {
                    let f = 10i32.pow((-d) as u32);
                    (n / f) * f
                })),
                Value::BigInt(n) => Ok(Value::BigInt(if d >= 0 {
                    n
                } else {
                    let f = 10i64.pow((-d) as u32);
                    (n / f) * f
                })),
                Value::Real(f) => {
                    let factor = 10f64.powi(d);
                    Ok(Value::Real((f * factor).trunc() / factor))
                }
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        "rand" | "random" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            let r = (seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) as f64)
                / (u32::MAX as f64 + 1.0);
            Ok(Value::Real(r))
        }

        "greatest" => {
            if args.is_empty() {
                return Err(DbError::TypeMismatch {
                    expected: "1+ args".into(),
                    got: "0".into(),
                });
            }
            let mut best: Option<Value> = None;
            for a in args {
                let v = crate::eval::eval(a, row)?;
                if matches!(v, Value::Null) {
                    return Ok(Value::Null);
                }
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        if crate::eval::ops::compare_values(&v, &b)
                            .map(|o| o.is_gt())
                            .unwrap_or(false)
                        {
                            v
                        } else {
                            b
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }

        "least" => {
            if args.is_empty() {
                return Err(DbError::TypeMismatch {
                    expected: "1+ args".into(),
                    got: "0".into(),
                });
            }
            let mut best: Option<Value> = None;
            for a in args {
                let v = crate::eval::eval(a, row)?;
                if matches!(v, Value::Null) {
                    return Ok(Value::Null);
                }
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        if crate::eval::ops::compare_values(&v, &b)
                            .map(|o| o.is_lt())
                            .unwrap_or(false)
                        {
                            v
                        } else {
                            b
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }

        _ => unreachable!("dispatcher routed unsupported numeric function"),
    }
}

fn to_f64_val(v: Value) -> Option<f64> {
    match v {
        Value::Null => None,
        Value::Int(n) => Some(n as f64),
        Value::BigInt(n) => Some(n as f64),
        Value::Real(f) => Some(f),
        _ => None,
    }
}

fn one_trig(
    args: &[crate::expr::Expr],
    row: &[Value],
    f: fn(f64) -> f64,
) -> Result<Value, DbError> {
    let v = crate::eval::eval(
        args.first().ok_or_else(|| DbError::TypeMismatch {
            expected: "1 arg".into(),
            got: "0".into(),
        })?,
        row,
    )?;
    match to_f64_val(v) {
        None => Ok(Value::Null),
        Some(x) => Ok(Value::Real(f(x))),
    }
}
