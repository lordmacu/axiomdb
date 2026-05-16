use axiomdb_core::error::DbError;
use axiomdb_types::{
    ltree_index, ltree_lca, ltree_nlevel, ltree_subpath, validate_ltree_path, Value,
};

use crate::expr::Expr;

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        "nlevel" => eval_nlevel(args, row),
        "subpath" => eval_subpath(args, row),
        "subltree" => eval_subltree(args, row),
        "index" => eval_index(args, row),
        "lca" => eval_lca(args, row),
        "text2ltree" => eval_text2ltree(args, row),
        "ltree2text" => eval_ltree2text(args, row),
        _ => unreachable!("ltree::eval called with unknown function '{name}'"),
    }
}

fn require_args(name: &str, args: &[Expr], n: usize) -> Result<(), DbError> {
    if args.len() < n {
        return Err(DbError::TypeMismatch {
            expected: format!("{name}: {n} argument(s)"),
            got: format!("{}", args.len()),
        });
    }
    Ok(())
}

fn eval_nlevel(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    require_args("nlevel", args, 1)?;
    match crate::eval::eval(&args[0], row)? {
        Value::Null => Ok(Value::Null),
        Value::Ltree(s) => Ok(Value::Int(ltree_nlevel(&s) as i32)),
        Value::Text(s) => {
            validate_ltree_path(&s).map_err(|e| DbError::InvalidValue {
                reason: format!("nlevel: {e}"),
            })?;
            Ok(Value::Int(ltree_nlevel(&s) as i32))
        }
        other => Err(DbError::TypeMismatch {
            expected: "LTREE value for nlevel()".into(),
            got: other.variant_name().to_string(),
        }),
    }
}

fn eval_subpath(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    require_args("subpath", args, 2)?;
    let path_v = crate::eval::eval(&args[0], row)?;
    let offset_v = crate::eval::eval(&args[1], row)?;
    let len_v = if args.len() >= 3 {
        Some(crate::eval::eval(&args[2], row)?)
    } else {
        None
    };

    if matches!(path_v, Value::Null) || matches!(offset_v, Value::Null) {
        return Ok(Value::Null);
    }
    if matches!(len_v, Some(Value::Null)) {
        return Ok(Value::Null);
    }

    let s = ltree_path_str(&path_v, "subpath")?;
    let n = ltree_nlevel(s);
    let raw_offset = i64_arg(&offset_v, "subpath offset")?;
    // negative offset counts from the end (PostgreSQL semantics)
    let offset: usize = if raw_offset < 0 {
        let from_end = (-raw_offset) as usize;
        n.saturating_sub(from_end)
    } else {
        raw_offset as usize
    };

    let len: Option<usize> = match len_v {
        None => None,
        Some(v) => {
            let raw = i64_arg(&v, "subpath len")?;
            if raw < 0 {
                return Err(DbError::InvalidValue {
                    reason: "subpath: len must be non-negative".into(),
                });
            }
            Some(raw as usize)
        }
    };

    match ltree_subpath(s, offset, len) {
        Some(r) => Ok(Value::Ltree(r)),
        None => Ok(Value::Null),
    }
}

fn eval_subltree(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    require_args("subltree", args, 3)?;
    let path_v = crate::eval::eval(&args[0], row)?;
    let start_v = crate::eval::eval(&args[1], row)?;
    let end_v = crate::eval::eval(&args[2], row)?;

    if matches!(path_v, Value::Null)
        || matches!(start_v, Value::Null)
        || matches!(end_v, Value::Null)
    {
        return Ok(Value::Null);
    }

    let s = ltree_path_str(&path_v, "subltree")?;
    let start = i64_arg(&start_v, "subltree start")? as usize;
    let end = i64_arg(&end_v, "subltree end")? as usize;
    let len = end.saturating_sub(start);

    match ltree_subpath(s, start, Some(len)) {
        Some(r) => Ok(Value::Ltree(r)),
        None => Ok(Value::Null),
    }
}

fn eval_index(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    require_args("index", args, 2)?;
    let haystack_v = crate::eval::eval(&args[0], row)?;
    let needle_v = crate::eval::eval(&args[1], row)?;
    let offset_v = if args.len() >= 3 {
        Some(crate::eval::eval(&args[2], row)?)
    } else {
        None
    };

    if matches!(haystack_v, Value::Null) || matches!(needle_v, Value::Null) {
        return Ok(Value::Null);
    }
    if matches!(offset_v, Some(Value::Null)) {
        return Ok(Value::Null);
    }

    let hs = ltree_path_str(&haystack_v, "index")?;
    let nd = ltree_path_str(&needle_v, "index")?;
    let offset: usize = match offset_v {
        Some(v) => {
            let raw = i64_arg(&v, "index offset")?;
            if raw < 0 {
                0
            } else {
                raw as usize
            }
        }
        None => 0,
    };

    // -1 means not found (PostgreSQL semantics)
    let pos: i32 = match ltree_index(hs, nd, offset) {
        Some(p) => p as i32,
        None => -1,
    };
    Ok(Value::Int(pos))
}

fn eval_lca(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    if args.is_empty() {
        return Err(DbError::TypeMismatch {
            expected: "lca: at least 1 argument".into(),
            got: "0".into(),
        });
    }
    let mut paths: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        let v = crate::eval::eval(arg, row)?;
        if matches!(v, Value::Null) {
            return Ok(Value::Null);
        }
        paths.push(ltree_path_str(&v, "lca")?.to_string());
    }

    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let result = ltree_lca(&refs);
    Ok(Value::Ltree(result))
}

fn eval_text2ltree(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    require_args("text2ltree", args, 1)?;
    let v = crate::eval::eval(&args[0], row)?;
    match v {
        Value::Null => Ok(Value::Null),
        Value::Ltree(s) => Ok(Value::Ltree(s)),
        Value::Text(s) => {
            validate_ltree_path(&s).map_err(|e| DbError::InvalidCoercion {
                from: "Text".into(),
                to: "LTREE".into(),
                value: format!("'{s}'"),
                reason: e.to_string(),
            })?;
            Ok(Value::Ltree(s))
        }
        other => Err(DbError::TypeMismatch {
            expected: "text for text2ltree()".into(),
            got: other.variant_name().to_string(),
        }),
    }
}

fn eval_ltree2text(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    require_args("ltree2text", args, 1)?;
    let v = crate::eval::eval(&args[0], row)?;
    match v {
        Value::Null => Ok(Value::Null),
        Value::Ltree(s) | Value::Text(s) => Ok(Value::Text(s)),
        other => Err(DbError::TypeMismatch {
            expected: "LTREE value for ltree2text()".into(),
            got: other.variant_name().to_string(),
        }),
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn ltree_path_str<'a>(v: &'a Value, ctx: &str) -> Result<&'a str, DbError> {
    match v {
        Value::Ltree(s) | Value::Text(s) => Ok(s.as_str()),
        other => Err(DbError::TypeMismatch {
            expected: format!("LTREE value for {ctx}()"),
            got: other.variant_name().to_string(),
        }),
    }
}

fn i64_arg(v: &Value, ctx: &str) -> Result<i64, DbError> {
    match v {
        Value::Int(n) => Ok(*n as i64),
        Value::BigInt(n) => Ok(*n),
        Value::Decimal(m, 0) => Ok(*m as i64),
        other => Err(DbError::TypeMismatch {
            expected: format!("integer for {ctx}"),
            got: other.variant_name().to_string(),
        }),
    }
}
