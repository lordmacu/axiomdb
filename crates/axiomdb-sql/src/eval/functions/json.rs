use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        // JSON_EXTRACT(json, path) — extract value at simple dot-path.
        // MySQL-compatible: JSON_EXTRACT(data, '$.name') → 'Alice'
        "json_extract" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let (Value::Json(json_str) | Value::Text(json_str)) = json_val else {
                return if matches!(json_val, Value::Null) {
                    Ok(Value::Null)
                } else {
                    Err(DbError::TypeMismatch {
                        expected: "JSON value".into(),
                        got: json_val.variant_name().into(),
                    })
                };
            };
            let path = eval_path_arg(args, 1, row, name)?;
            let parsed: serde_json::Value = parse_json(&json_str)?;
            let result = extract_path(&parsed, &path)?;
            Ok(json_value_to_sql(result))
        }

        // JSON_SET(json, path, value) — set value at path.
        "json_set" => {
            expect_arg_count(name, args, 3)?;
            let json_str = eval_arg(args, 0, row, name)?;
            if matches!(json_str, Value::Null) {
                return Ok(Value::Null);
            }
            let path = eval_path_arg(args, 1, row, name)?;
            let new_val = eval_arg(args, 2, row, name)?;

            let mut parsed: serde_json::Value = parse_json_from_value(&json_str)?;
            set_path(&mut parsed, &path, sql_to_json_value(&new_val));
            Ok(Value::Json(parsed.to_string()))
        }

        // JSON_REMOVE(json, path) — remove key at path.
        "json_remove" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let path = eval_path_arg(args, 1, row, name)?;
            let (Value::Json(json_str) | Value::Text(json_str)) = json_val else {
                return Err(DbError::TypeMismatch {
                    expected: "JSON value".into(),
                    got: json_val.variant_name().into(),
                });
            };
            let mut parsed: serde_json::Value = parse_json(&json_str)?;
            remove_path(&mut parsed, &path);
            Ok(Value::Json(parsed.to_string()))
        }

        // JSON_KEYS(json) — return array of top-level keys.
        "json_keys" => {
            expect_arg_count(name, args, 1)?;
            let json_val = eval_arg(args, 0, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let (Value::Json(json_str) | Value::Text(json_str)) = json_val else {
                return Err(DbError::TypeMismatch {
                    expected: "JSON value".into(),
                    got: json_val.variant_name().into(),
                });
            };
            let parsed: serde_json::Value = parse_json(&json_str)?;
            match parsed {
                serde_json::Value::Object(map) => {
                    let keys: Vec<serde_json::Value> = map
                        .keys()
                        .map(|k| serde_json::Value::String(k.clone()))
                        .collect();
                    Ok(Value::Json(serde_json::Value::Array(keys).to_string()))
                }
                _ => Ok(Value::Null),
            }
        }

        // JSON_VALID(text) — returns 1 if valid JSON, 0 otherwise.
        "json_valid" => {
            expect_arg_count(name, args, 1)?;
            let arg = eval_arg(args, 0, row, name)?;
            let s = match arg {
                Value::Null => return Ok(Value::Int(0)),
                Value::Json(s) | Value::Text(s) => s,
                other => other.to_string(),
            };
            let valid = serde_json::from_str::<serde_json::Value>(&s).is_ok();
            Ok(Value::Int(if valid { 1 } else { 0 }))
        }

        // JSON_TYPE(json) — returns type name.
        "json_type" => {
            expect_arg_count(name, args, 1)?;
            let arg = eval_arg(args, 0, row, name)?;
            if matches!(arg, Value::Null) {
                return Ok(Value::Null);
            }
            let (Value::Json(s) | Value::Text(s)) = arg else {
                return Err(DbError::TypeMismatch {
                    expected: "JSON value".into(),
                    got: arg.variant_name().into(),
                });
            };
            let parsed: serde_json::Value = parse_json(&s)?;
            let type_name = match parsed {
                serde_json::Value::Object(_) => "OBJECT",
                serde_json::Value::Array(_) => "ARRAY",
                serde_json::Value::String(_) => "STRING",
                serde_json::Value::Number(_) => "NUMBER",
                serde_json::Value::Bool(_) => "BOOLEAN",
                serde_json::Value::Null => "NULL",
            };
            Ok(Value::Text(type_name.to_string()))
        }

        _ => unreachable!("dispatcher routed unsupported JSON function"),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn expect_arg_count(name: &str, args: &[Expr], expected: usize) -> Result<(), DbError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(DbError::TypeMismatch {
            expected: format!("{name}: {expected} args"),
            got: args.len().to_string(),
        })
    }
}

fn eval_arg(args: &[Expr], idx: usize, row: &[Value], name: &str) -> Result<Value, DbError> {
    let arg = args.get(idx).ok_or_else(|| DbError::TypeMismatch {
        expected: format!("{name}: arg {idx}"),
        got: "missing".into(),
    })?;
    crate::eval::eval(arg, row)
}

fn eval_path_arg(args: &[Expr], idx: usize, row: &[Value], name: &str) -> Result<String, DbError> {
    match eval_arg(args, idx, row, name)? {
        Value::Null => Err(DbError::InvalidValue {
            reason: format!("{name}: JSON path cannot be NULL"),
        }),
        Value::Text(s) | Value::Json(s) => normalize_path(&s),
        other => normalize_path(&other.to_string()),
    }
}

fn normalize_path(path: &str) -> Result<String, DbError> {
    if path == "$" {
        return Ok(path.to_string());
    }

    let normalized = path.strip_prefix("$.").unwrap_or(path);
    if normalized.is_empty()
        || normalized
            .split('.')
            .any(|part| part.is_empty() || part.contains('$'))
    {
        return Err(DbError::InvalidValue {
            reason: format!("invalid JSON path: {path}"),
        });
    }

    Ok(format!("$.{normalized}"))
}

fn parse_json(s: &str) -> Result<serde_json::Value, DbError> {
    serde_json::from_str(s).map_err(|e| DbError::InvalidValue {
        reason: format!("invalid JSON: {e}"),
    })
}

fn parse_json_from_value(v: &Value) -> Result<serde_json::Value, DbError> {
    match v {
        Value::Json(s) | Value::Text(s) => parse_json(s),
        Value::Null => Ok(serde_json::Value::Null),
        _ => Err(DbError::TypeMismatch {
            expected: "JSON value".into(),
            got: v.variant_name().into(),
        }),
    }
}

/// Extract value at a simple dot-path ($.key1.key2).
fn extract_path<'a>(
    val: &'a serde_json::Value,
    path: &str,
) -> Result<Option<&'a serde_json::Value>, DbError> {
    let path = normalized_path_components(path)?;
    if path.is_empty() {
        return Ok(Some(val));
    }
    let mut current = val;
    for key in path.split('.') {
        match current {
            serde_json::Value::Object(map) => {
                let Some(next) = map.get(key) else {
                    return Ok(None);
                };
                current = next;
            }
            serde_json::Value::Array(arr) => {
                let Ok(idx) = key.parse::<usize>() else {
                    return Ok(None);
                };
                let Some(next) = arr.get(idx) else {
                    return Ok(None);
                };
                current = next;
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// Set value at a simple dot-path.
fn set_path(root: &mut serde_json::Value, path: &str, new_val: serde_json::Value) {
    let path = normalized_path_components_lossy(path);
    if path.is_empty() {
        *root = new_val;
        return;
    }
    let keys: Vec<&str> = path.split('.').collect();
    let mut current = root;
    for &key in &keys[..keys.len() - 1] {
        if !current.is_object() {
            *current = serde_json::json!({});
        }
        let Some(obj) = current.as_object_mut() else {
            return;
        };
        current = obj.entry(key).or_insert(serde_json::json!({}));
    }
    if let Some(obj) = current.as_object_mut() {
        obj.insert(keys[keys.len() - 1].to_string(), new_val);
    }
}

/// Remove value at a simple dot-path.
fn remove_path(root: &mut serde_json::Value, path: &str) {
    let path = normalized_path_components_lossy(path);
    if path.is_empty() {
        *root = serde_json::Value::Null;
        return;
    }
    let keys: Vec<&str> = path.split('.').collect();
    let mut current = root;
    for &key in &keys[..keys.len() - 1] {
        match current {
            serde_json::Value::Object(map) => {
                if let Some(next) = map.get_mut(key) {
                    current = next;
                } else {
                    return;
                }
            }
            _ => return,
        }
    }
    if let Some(obj) = current.as_object_mut() {
        obj.remove(keys[keys.len() - 1]);
    }
}

fn normalized_path_components(path: &str) -> Result<&str, DbError> {
    if path == "$" {
        return Ok("");
    }
    let Some(path) = path.strip_prefix("$.") else {
        return Err(DbError::InvalidValue {
            reason: format!("invalid JSON path: {path}"),
        });
    };
    if path.is_empty() || path.split('.').any(|part| part.is_empty()) {
        return Err(DbError::InvalidValue {
            reason: format!("invalid JSON path: $.{path}"),
        });
    }
    Ok(path)
}

fn normalized_path_components_lossy(path: &str) -> &str {
    if path == "$" {
        ""
    } else {
        path.strip_prefix("$.").unwrap_or(path)
    }
}

fn json_value_to_sql(v: Option<&serde_json::Value>) -> Value {
    match v {
        None => Value::Null,
        Some(serde_json::Value::Null) => Value::Null,
        Some(serde_json::Value::String(s)) => Value::Text(s.clone()),
        Some(serde_json::Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    Value::Int(i as i32)
                } else {
                    Value::BigInt(i)
                }
            } else if let Some(f) = n.as_f64() {
                Value::Real(f)
            } else {
                Value::Text(n.to_string())
            }
        }
        Some(serde_json::Value::Bool(b)) => Value::Bool(*b),
        Some(other) => Value::Json(other.to_string()),
    }
}

fn sql_to_json_value(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::json!(n),
        Value::BigInt(n) => serde_json::json!(n),
        Value::Real(f) => serde_json::json!(f),
        Value::Text(s) | Value::Json(s) => serde_json::Value::String(s.clone()),
        _ => serde_json::Value::String(v.to_string()),
    }
}
