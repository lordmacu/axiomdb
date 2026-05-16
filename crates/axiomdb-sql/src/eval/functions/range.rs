use axiomdb_core::error::DbError;
use axiomdb_types::{range_value::RangeValue, DataType, Value};

use crate::expr::Expr;

pub(crate) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        "lower" => eval_lower(args, row),
        "upper" => eval_upper(args, row),
        "isempty" => eval_isempty(args, row),
        "lowerinc" => eval_lowerinc(args, row),
        "upperinc" => eval_upperinc(args, row),
        "lowerbnd" => eval_lower(args, row),
        "upperbnd" => eval_upper(args, row),
        "int4range" => eval_constructor(args, row, DataType::Int, true),
        "int8range" => eval_constructor(args, row, DataType::BigInt, true),
        "daterange" => eval_constructor(args, row, DataType::Date, true),
        "numrange" => eval_constructor(args, row, DataType::Decimal, false),
        "tsrange" => eval_constructor(args, row, DataType::Timestamp, false),
        _ => Err(DbError::NotImplemented {
            feature: format!("range function '{name}'"),
        }),
    }
}

/// Returns the lower bound of a range, or NULL if unbounded.
fn eval_lower(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    let rv = require_range_arg("lower", args, row)?;
    if rv.is_empty {
        return Ok(Value::Null);
    }
    Ok(rv.lower.clone().unwrap_or(Value::Null))
}

/// Returns the upper bound of a range, or NULL if unbounded.
fn eval_upper(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    let rv = require_range_arg("upper", args, row)?;
    if rv.is_empty {
        return Ok(Value::Null);
    }
    Ok(rv.upper.clone().unwrap_or(Value::Null))
}

/// Returns true if the range is empty.
fn eval_isempty(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    let rv = require_range_arg("isempty", args, row)?;
    Ok(Value::Bool(rv.is_empty))
}

/// Returns true if the lower bound is inclusive.
fn eval_lowerinc(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    let rv = require_range_arg("lowerinc", args, row)?;
    if rv.is_empty || rv.lower.is_none() {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(rv.lower_inc))
}

/// Returns true if the upper bound is inclusive.
fn eval_upperinc(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    let rv = require_range_arg("upperinc", args, row)?;
    if rv.is_empty || rv.upper.is_none() {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(rv.upper_inc))
}

/// Constructor for range types.
///
/// Signature: `type_range(lower, upper[, bounds])`
/// - `lower` and `upper` may be NULL (representing unbounded ends).
/// - `bounds` is a 2-character string like `'[)'`, `'()'`, `'[]'`, `'(]'`.
///   Defaults to `'[)'` (lower inclusive, upper exclusive — PG default).
/// - `is_discrete`: if true, canonicalizes upper inclusive → exclusive.
fn eval_constructor(
    args: &[Expr],
    row: &[Value],
    elem_type: DataType,
    is_discrete: bool,
) -> Result<Value, DbError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(DbError::ParseError {
            message: format!(
                "range constructor expects 2 or 3 arguments, got {}",
                args.len()
            ),
            position: None,
        });
    }

    let lo_val = crate::eval::eval(&args[0], row)?;
    let hi_val = crate::eval::eval(&args[1], row)?;

    // Parse bounds string, default "[)"
    let (lower_inc, upper_inc) = if args.len() == 3 {
        let bounds_val = crate::eval::eval(&args[2], row)?;
        match &bounds_val {
            Value::Text(s) => parse_bounds_str(s)?,
            Value::Null => (true, false),
            _ => {
                return Err(DbError::ParseError {
                    message: "range bounds argument must be a TEXT string like '[)' or '[]'"
                        .to_string(),
                    position: None,
                })
            }
        }
    } else {
        (true, false) // default: [)
    };

    let lower = if lo_val == Value::Null {
        None
    } else {
        Some(lo_val)
    };
    let upper = if hi_val == Value::Null {
        None
    } else {
        Some(hi_val)
    };

    let rv = if is_discrete {
        RangeValue::new_discrete(lower, upper, lower_inc, upper_inc, |v| {
            increment_discrete(v, &elem_type)
        })?
    } else {
        RangeValue::new(lower, upper, lower_inc, upper_inc)?
    };

    Ok(Value::Range(Box::new(rv)))
}

/// Parse a 2-character bounds string: first char = lower bound style, second = upper.
fn parse_bounds_str(s: &str) -> Result<(bool, bool), DbError> {
    let bytes = s.as_bytes();
    if bytes.len() != 2 {
        return Err(DbError::ParseError {
            message: format!("invalid range bounds string '{s}': must be 2 characters like '[)'"),
            position: None,
        });
    }
    let lower_inc = match bytes[0] {
        b'[' => true,
        b'(' => false,
        _ => {
            return Err(DbError::ParseError {
                message: format!("invalid lower bound char '{}' in '{s}'", bytes[0] as char),
                position: None,
            })
        }
    };
    let upper_inc = match bytes[1] {
        b']' => true,
        b')' => false,
        _ => {
            return Err(DbError::ParseError {
                message: format!("invalid upper bound char '{}' in '{s}'", bytes[1] as char),
                position: None,
            })
        }
    };
    Ok((lower_inc, upper_inc))
}

/// Increment a discrete value by one unit for canonicalization.
fn increment_discrete(v: &Value, elem_type: &DataType) -> Result<Value, DbError> {
    match (v, elem_type) {
        (Value::Int(n), DataType::Int) => {
            n.checked_add(1)
                .map(Value::Int)
                .ok_or_else(|| DbError::InvalidValue {
                    reason: "int4range upper bound overflow".to_string(),
                })
        }
        (Value::BigInt(n), DataType::BigInt) => {
            n.checked_add(1)
                .map(Value::BigInt)
                .ok_or_else(|| DbError::InvalidValue {
                    reason: "int8range upper bound overflow".to_string(),
                })
        }
        (Value::Date(d), DataType::Date) => {
            d.checked_add(1)
                .map(Value::Date)
                .ok_or_else(|| DbError::InvalidValue {
                    reason: "daterange upper bound overflow".to_string(),
                })
        }
        _ => Err(DbError::TypeMismatch {
            expected: "discrete range element type (INT, BIGINT, DATE)".to_string(),
            got: v.variant_name().to_string(),
        }),
    }
}

// ── Helper ────────────────────────────────────────────────────────────────────

fn require_range_arg(
    fn_name: &str,
    args: &[Expr],
    row: &[Value],
) -> Result<Box<RangeValue>, DbError> {
    if args.len() != 1 {
        return Err(DbError::ParseError {
            message: format!("{fn_name}() expects 1 argument, got {}", args.len()),
            position: None,
        });
    }
    let val = crate::eval::eval(&args[0], row)?;
    match val {
        Value::Range(rv) => Ok(rv),
        Value::Null => Err(DbError::InvalidValue {
            reason: format!("{fn_name}() called with NULL"),
        }),
        other => Err(DbError::TypeMismatch {
            expected: "RANGE".to_string(),
            got: other.variant_name().to_string(),
        }),
    }
}
