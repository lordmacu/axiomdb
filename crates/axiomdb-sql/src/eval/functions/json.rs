use std::sync::Arc;

use axiomdb_core::error::DbError;
use axiomdb_types::{
    jsonb_contains, jsonb_merge_patch, jsonb_overlaps, JsonbDecoder, JsonbEncoder, JsonbRef, Value,
};

use crate::expr::Expr;

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        // ── JSON_EXTRACT(json, path) ─────────────────────────────────────────
        "json_extract" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let path = eval_path_arg(args, 1, row, name)?;
            match json_val {
                Value::Null => Ok(Value::Null),
                Value::Jsonb(ref blob) => {
                    let sj = jsonb_to_serde(blob)?;
                    let result = extract_path(&sj, &path)?;
                    Ok(serde_json_to_sql_value(result))
                }
                Value::Json(ref s) | Value::Text(ref s) => {
                    let parsed = parse_json(s)?;
                    Ok(serde_json_to_sql_value(extract_path(&parsed, &path)?))
                }
                other => Err(type_err("JSON value", other.variant_name())),
            }
        }

        // ── JSON_SET(json, path, value) ──────────────────────────────────────
        "json_set" => {
            expect_arg_count(name, args, 3)?;
            let json_val = eval_arg(args, 0, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let path = eval_path_arg(args, 1, row, name)?;
            let new_val = eval_arg(args, 2, row, name)?;
            let mut parsed = value_to_serde_json(&json_val)?;
            set_path(&mut parsed, &path, sql_to_serde_json(&new_val));
            Ok(Value::Json(parsed.to_string()))
        }

        // ── JSON_REMOVE(json, path) ──────────────────────────────────────────
        "json_remove" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let path = eval_path_arg(args, 1, row, name)?;
            let mut parsed = value_to_serde_json(&json_val)?;
            remove_path(&mut parsed, &path);
            Ok(Value::Json(parsed.to_string()))
        }

        // ── JSON_KEYS(json) — return array of top-level keys ─────────────────
        "json_keys" => {
            expect_arg_count(name, args, 1)?;
            let json_val = eval_arg(args, 0, row, name)?;
            match json_val {
                Value::Null => Ok(Value::Null),
                Value::Jsonb(ref blob) => {
                    let r = JsonbRef::new(blob);
                    if r.is_array() || r.is_scalar() {
                        return Ok(Value::Null);
                    }
                    let keys: Vec<serde_json::Value> = r
                        .object_keys()?
                        .into_iter()
                        .map(|k| serde_json::Value::String(k.as_ref().to_string()))
                        .collect();
                    Ok(Value::Json(serde_json::Value::Array(keys).to_string()))
                }
                Value::Json(ref s) | Value::Text(ref s) => {
                    let parsed = parse_json(s)?;
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
                other => Err(type_err("JSON value", other.variant_name())),
            }
        }

        // ── JSON_VALID(text) — returns 1 if valid JSON, 0 otherwise ──────────
        "json_valid" => {
            expect_arg_count(name, args, 1)?;
            let arg = eval_arg(args, 0, row, name)?;
            let s = match arg {
                Value::Null => return Ok(Value::Int(0)),
                Value::Jsonb(_) => return Ok(Value::Int(1)),
                Value::Json(s) | Value::Text(s) => s,
                other => other.to_string(),
            };
            Ok(Value::Int(
                if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
                    1
                } else {
                    0
                },
            ))
        }

        // ── JSON_TYPE(json) — returns type name ──────────────────────────────
        "json_type" => {
            expect_arg_count(name, args, 1)?;
            let arg = eval_arg(args, 0, row, name)?;
            match arg {
                Value::Null => Ok(Value::Null),
                ref v @ Value::Jsonb(ref blob) => {
                    let _ = v;
                    let sj = jsonb_to_serde(blob)?;
                    Ok(Value::Text(serde_json_type_name(&sj).to_string()))
                }
                Value::Json(ref s) | Value::Text(ref s) => {
                    let parsed = parse_json(s)?;
                    Ok(Value::Text(serde_json_type_name(&parsed).to_string()))
                }
                other => Err(type_err("JSON value", other.variant_name())),
            }
        }

        // ── JSON_MERGE_PATCH(target, patch) — RFC 7396 ───────────────────────
        "json_merge_patch" => {
            expect_arg_count(name, args, 2)?;
            let target = eval_arg(args, 0, row, name)?;
            let patch = eval_arg(args, 1, row, name)?;
            if matches!(target, Value::Null) || matches!(patch, Value::Null) {
                return Ok(Value::Null);
            }
            let target_sj = value_to_serde_json(&target)?;
            let patch_sj = value_to_serde_json(&patch)?;
            // jsonb_merge_patch: takes &serde_json::Value, returns serde_json::Value
            let result_sj = jsonb_merge_patch(&target_sj, &patch_sj);
            let blob = JsonbEncoder::encode(&result_sj)?;
            Ok(Value::Jsonb(Arc::new(blob)))
        }

        // ── JSON_CONTAINS(target, candidate [, path]) ────────────────────────
        "json_contains" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(DbError::TypeMismatch {
                    expected: "json_contains: 2 or 3 args".into(),
                    got: args.len().to_string(),
                });
            }
            let target = eval_arg(args, 0, row, name)?;
            let candidate = eval_arg(args, 1, row, name)?;
            if matches!(target, Value::Null) || matches!(candidate, Value::Null) {
                return Ok(Value::Null);
            }
            let mut target_blob = value_to_jsonb_blob(&target)?;
            let cand_blob = value_to_jsonb_blob(&candidate)?;
            if args.len() == 3 {
                let path = eval_path_arg(args, 2, row, name)?;
                let sj = jsonb_to_serde(&target_blob)?;
                let sub = extract_path(&sj, &path)?;
                match sub {
                    None => return Ok(Value::Null),
                    Some(v) => {
                        target_blob = JsonbEncoder::encode(v)?;
                    }
                }
            }
            let result = jsonb_contains(&target_blob, &cand_blob)?;
            Ok(Value::Int(if result { 1 } else { 0 }))
        }

        // ── JSON_OVERLAPS(a, b) ───────────────────────────────────────────────
        "json_overlaps" => {
            expect_arg_count(name, args, 2)?;
            let a = eval_arg(args, 0, row, name)?;
            let b = eval_arg(args, 1, row, name)?;
            if matches!(a, Value::Null) || matches!(b, Value::Null) {
                return Ok(Value::Null);
            }
            let a_blob = value_to_jsonb_blob(&a)?;
            let b_blob = value_to_jsonb_blob(&b)?;
            let result = jsonb_overlaps(&a_blob, &b_blob)?;
            Ok(Value::Int(if result { 1 } else { 0 }))
        }

        // ── JSON_ARRAY_LENGTH(json [, path]) ─────────────────────────────────
        "json_array_length" => {
            if args.is_empty() || args.len() > 2 {
                return Err(DbError::TypeMismatch {
                    expected: "json_array_length: 1 or 2 args".into(),
                    got: args.len().to_string(),
                });
            }
            let json_val = eval_arg(args, 0, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let mut sj = value_to_serde_json(&json_val)?;
            if args.len() == 2 {
                let path = eval_path_arg(args, 1, row, name)?;
                sj = match extract_path(&sj, &path)? {
                    None => return Ok(Value::Null),
                    Some(v) => v.clone(),
                };
            }
            match sj {
                serde_json::Value::Array(arr) => Ok(Value::Int(arr.len() as i32)),
                _ => Ok(Value::Null),
            }
        }

        // ── JSON_DEPTH(json) ─────────────────────────────────────────────────
        "json_depth" => {
            expect_arg_count(name, args, 1)?;
            let json_val = eval_arg(args, 0, row, name)?;
            match json_val {
                Value::Null => Ok(Value::Null),
                Value::Jsonb(ref blob) => {
                    let d = JsonbRef::new(blob).max_depth()?;
                    Ok(Value::Int(d as i32))
                }
                ref v @ (Value::Json(_) | Value::Text(_)) => {
                    let sj = value_to_serde_json(v)?;
                    Ok(Value::Int(serde_json_depth(&sj) as i32))
                }
                other => Err(type_err("JSON value", other.variant_name())),
            }
        }

        // ── JSON_PRETTY(json) ────────────────────────────────────────────────
        "json_pretty" => {
            expect_arg_count(name, args, 1)?;
            let json_val = eval_arg(args, 0, row, name)?;
            match json_val {
                Value::Null => Ok(Value::Null),
                Value::Jsonb(ref blob) => {
                    let sj = jsonb_to_serde(blob)?;
                    Ok(Value::Text(serde_json::to_string_pretty(&sj).map_err(
                        |e| DbError::InvalidValue {
                            reason: format!("json_pretty: {e}"),
                        },
                    )?))
                }
                ref v @ (Value::Json(_) | Value::Text(_)) => {
                    let sj = value_to_serde_json(v)?;
                    Ok(Value::Text(serde_json::to_string_pretty(&sj).map_err(
                        |e| DbError::InvalidValue {
                            reason: format!("json_pretty: {e}"),
                        },
                    )?))
                }
                other => Err(type_err("JSON value", other.variant_name())),
            }
        }

        // ── TO_JSONB(val) / JSONB(val) ───────────────────────────────────────
        "to_jsonb" | "jsonb" => {
            expect_arg_count(name, args, 1)?;
            let val = eval_arg(args, 0, row, name)?;
            match val {
                Value::Null => Ok(Value::Null),
                Value::Jsonb(_) => Ok(val),
                ref v => {
                    let sj = value_to_serde_json(v)?;
                    let blob = JsonbEncoder::encode(&sj)?;
                    Ok(Value::Jsonb(Arc::new(blob)))
                }
            }
        }

        // ── JSON_PATH_EXISTS(json, path) ─────────────────────────────────────
        "json_path_exists" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let path_str = eval_path_arg_raw(args, 1, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&json_val)?;
            let steps = parse_jsonpath(&path_str)?;
            let results = execute_jsonpath(&sj, &steps);
            Ok(Value::Bool(!results.is_empty()))
        }

        // ── JSON_PATH_QUERY(json, path) ───────────────────────────────────────
        "json_path_query" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let path_str = eval_path_arg_raw(args, 1, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&json_val)?;
            let steps = parse_jsonpath(&path_str)?;
            let results = execute_jsonpath(&sj, &steps);
            let arr: Vec<serde_json::Value> = results.into_iter().cloned().collect();
            Ok(Value::Json(serde_json::Value::Array(arr).to_string()))
        }

        // ── JSON_PATH_QUERY_FIRST(json, path) ─────────────────────────────────
        "json_path_query_first" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let path_str = eval_path_arg_raw(args, 1, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&json_val)?;
            let steps = parse_jsonpath(&path_str)?;
            let results = execute_jsonpath(&sj, &steps);
            match results.into_iter().next() {
                None => Ok(Value::Null),
                Some(v) => Ok(serde_json_to_sql_value(Some(v))),
            }
        }

        // ── Phase 11.18a: JSONB operator aliases for cross-engine SQL ───────
        //
        // `JSONB_EXISTS(doc, key)` ≡ `doc ? key`
        // `JSONB_CONTAINED(a, b)` ≡ `a <@ b`
        // `JSONB_CONCAT(a, b)` ≡ `a || b`
        // `JSONB_DELETE_KEY(doc, key)` ≡ `doc - 'key'`
        // `JSONB_DELETE_INDEX(doc, idx)` ≡ `doc - idx`
        "jsonb_exists" => {
            expect_arg_count(name, args, 2)?;
            let left = eval_arg(args, 0, row, name)?;
            let right = eval_arg(args, 1, row, name)?;
            crate::eval::eval_binary(crate::expr::BinaryOp::JsonExists, left, right)
        }
        "jsonb_contained" => {
            expect_arg_count(name, args, 2)?;
            let left = eval_arg(args, 0, row, name)?;
            let right = eval_arg(args, 1, row, name)?;
            crate::eval::eval_binary(crate::expr::BinaryOp::JsonContainedBy, left, right)
        }
        "jsonb_concat" => {
            expect_arg_count(name, args, 2)?;
            let left = eval_arg(args, 0, row, name)?;
            let right = eval_arg(args, 1, row, name)?;
            crate::eval::eval_binary(crate::expr::BinaryOp::Concat, left, right)
        }
        "jsonb_delete_key" | "jsonb_delete_index" => {
            expect_arg_count(name, args, 2)?;
            let left = eval_arg(args, 0, row, name)?;
            let right = eval_arg(args, 1, row, name)?;
            crate::eval::eval_binary(crate::expr::BinaryOp::Sub, left, right)
        }

        _ => unreachable!("dispatcher routed unsupported JSON function"),
    }
}

// ── Helpers — argument evaluation ────────────────────────────────────────────

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

fn type_err(expected: &str, got: &str) -> DbError {
    DbError::TypeMismatch {
        expected: expected.into(),
        got: got.into(),
    }
}

fn eval_arg(args: &[Expr], idx: usize, row: &[Value], name: &str) -> Result<Value, DbError> {
    let arg = args.get(idx).ok_or_else(|| DbError::TypeMismatch {
        expected: format!("{name}: arg {idx}"),
        got: "missing".into(),
    })?;
    crate::eval::eval(arg, row)
}

/// Evaluate and normalize a MySQL-style path (adds `$.` prefix if missing).
fn eval_path_arg(args: &[Expr], idx: usize, row: &[Value], name: &str) -> Result<String, DbError> {
    match eval_arg(args, idx, row, name)? {
        Value::Null => Err(DbError::InvalidValue {
            reason: format!("{name}: JSON path cannot be NULL"),
        }),
        Value::Text(s) | Value::Json(s) => normalize_path(&s),
        other => normalize_path(&other.to_string()),
    }
}

/// Evaluate a raw path for JSONPath expressions (no normalization).
fn eval_path_arg_raw(
    args: &[Expr],
    idx: usize,
    row: &[Value],
    name: &str,
) -> Result<String, DbError> {
    match eval_arg(args, idx, row, name)? {
        Value::Null => Err(DbError::InvalidValue {
            reason: format!("{name}: JSONPath cannot be NULL"),
        }),
        Value::Text(s) | Value::Json(s) => Ok(s),
        other => Ok(other.to_string()),
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

// ── Helpers — value conversions ───────────────────────────────────────────────

fn parse_json(s: &str) -> Result<serde_json::Value, DbError> {
    serde_json::from_str(s).map_err(|e| DbError::InvalidValue {
        reason: format!("invalid JSON: {e}"),
    })
}

fn jsonb_to_serde(blob: &[u8]) -> Result<serde_json::Value, DbError> {
    JsonbDecoder::decode(blob)
}

fn value_to_serde_json(v: &Value) -> Result<serde_json::Value, DbError> {
    match v {
        Value::Jsonb(blob) => jsonb_to_serde(blob),
        Value::Json(s) | Value::Text(s) => parse_json(s),
        Value::Null => Ok(serde_json::Value::Null),
        Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Value::Int(n) => Ok(serde_json::json!(n)),
        Value::BigInt(n) => Ok(serde_json::json!(n)),
        Value::Real(f) => Ok(serde_json::json!(f)),
        other => Ok(serde_json::Value::String(other.to_string())),
    }
}

/// Convert any JSON/JSONB/Text value to a raw JSONB blob.
fn value_to_jsonb_blob(v: &Value) -> Result<Vec<u8>, DbError> {
    match v {
        Value::Jsonb(blob) => Ok(blob.as_ref().clone()),
        other => {
            let sj = value_to_serde_json(other)?;
            JsonbEncoder::encode(&sj)
        }
    }
}

fn serde_json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Object(_) => "OBJECT",
        serde_json::Value::Array(_) => "ARRAY",
        serde_json::Value::String(_) => "STRING",
        serde_json::Value::Number(_) => "NUMBER",
        serde_json::Value::Bool(_) => "BOOLEAN",
        serde_json::Value::Null => "NULL",
    }
}

fn serde_json_to_sql_value(v: Option<&serde_json::Value>) -> Value {
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

fn sql_to_serde_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::json!(n),
        Value::BigInt(n) => serde_json::json!(n),
        Value::Real(f) => serde_json::json!(f),
        Value::Text(s) | Value::Json(s) => serde_json::Value::String(s.clone()),
        Value::Jsonb(blob) => JsonbDecoder::decode(blob).unwrap_or(serde_json::Value::Null),
        _ => serde_json::Value::String(v.to_string()),
    }
}

// ── Helpers — path navigation ─────────────────────────────────────────────────

/// Extract value at a simple dot-path ($.key1.key2[0]).
fn extract_path<'a>(
    val: &'a serde_json::Value,
    path: &str,
) -> Result<Option<&'a serde_json::Value>, DbError> {
    let components = normalized_path_components(path)?;
    if components.is_empty() {
        return Ok(Some(val));
    }
    let mut current = val;
    for segment in components.split('.') {
        let (key, indices) = split_key_indices(segment);
        if !key.is_empty() {
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
        for idx_str in &indices {
            let Ok(idx) = idx_str.parse::<usize>() else {
                return Ok(None);
            };
            match current {
                serde_json::Value::Array(arr) => {
                    let Some(next) = arr.get(idx) else {
                        return Ok(None);
                    };
                    current = next;
                }
                _ => return Ok(None),
            }
        }
    }
    Ok(Some(current))
}

/// Split `key[0][1]` into `("key", ["0", "1"])`.
fn split_key_indices(segment: &str) -> (&str, Vec<&str>) {
    if let Some(bracket_start) = segment.find('[') {
        let key = &segment[..bracket_start];
        let rest = &segment[bracket_start..];
        let indices: Vec<&str> = rest
            .split('[')
            .skip(1)
            .filter_map(|s| s.strip_suffix(']'))
            .collect();
        (key, indices)
    } else {
        (segment, vec![])
    }
}

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

fn serde_json_depth(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Array(arr) => 1 + arr.iter().map(serde_json_depth).max().unwrap_or(0),
        serde_json::Value::Object(map) => 1 + map.values().map(serde_json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

// ── SQL:2016 JSONPath compiler ────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum PathStep {
    Root,
    Key(String),
    Index(usize),
    WildcardKey,
    WildcardIndex,
    Recursive,
    Filter(FilterExpr),
}

#[derive(Debug, Clone)]
enum FilterExpr {
    Exists(Vec<String>),
    Compare {
        path: Vec<String>,
        op: CmpOp,
        value: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn parse_jsonpath(path: &str) -> Result<Vec<PathStep>, DbError> {
    let path = path.trim();
    if !path.starts_with('$') {
        return Err(DbError::InvalidValue {
            reason: format!("JSONPath must start with '$': {path}"),
        });
    }
    let mut steps = vec![PathStep::Root];
    let mut chars = path[1..].chars().peekable();

    while let Some(&ch) = chars.peek() {
        match ch {
            '.' => {
                chars.next();
                if chars.peek() == Some(&'.') {
                    chars.next();
                    steps.push(PathStep::Recursive);
                    let key = consume_identifier(&mut chars);
                    if key == "*" {
                        steps.push(PathStep::WildcardKey);
                    } else if !key.is_empty() {
                        steps.push(PathStep::Key(key));
                    }
                } else if chars.peek() == Some(&'*') {
                    chars.next();
                    steps.push(PathStep::WildcardKey);
                } else {
                    let key = consume_identifier(&mut chars);
                    if !key.is_empty() {
                        steps.push(PathStep::Key(key));
                    }
                }
            }
            '[' => {
                chars.next();
                let inner = consume_bracket(&mut chars);
                let inner = inner.trim();
                if inner == "*" {
                    steps.push(PathStep::WildcardIndex);
                } else if inner.starts_with('?') {
                    steps.push(PathStep::Filter(parse_filter(inner)?));
                } else if let Ok(idx) = inner.parse::<usize>() {
                    steps.push(PathStep::Index(idx));
                } else {
                    let key = inner.trim_matches(|c| c == '"' || c == '\'').to_string();
                    steps.push(PathStep::Key(key));
                }
            }
            _ => break,
        }
    }
    Ok(steps)
}

fn consume_identifier(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while let Some(&c) = chars.peek() {
        if c == '.' || c == '[' || c == ']' {
            break;
        }
        s.push(c);
        chars.next();
    }
    s
}

fn consume_bracket(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut content = String::new();
    let mut depth = 1i32;
    for c in chars.by_ref() {
        if c == '[' {
            depth += 1;
        } else if c == ']' {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        content.push(c);
    }
    content
}

fn parse_filter(s: &str) -> Result<FilterExpr, DbError> {
    let inner = s
        .strip_prefix('?')
        .and_then(|s| s.trim().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| DbError::InvalidValue {
            reason: format!("invalid JSONPath filter: {s}"),
        })?
        .trim();

    let inner = inner
        .strip_prefix('@')
        .ok_or_else(|| DbError::InvalidValue {
            reason: format!("JSONPath filter must start with '@': {inner}"),
        })?;

    // Parse dot-path: .key.key2...
    let mut path = Vec::new();
    let mut rest = inner;
    while let Some(r) = rest.strip_prefix('.') {
        let end = r
            .find(|c: char| c.is_whitespace() || c == '=' || c == '!' || c == '<' || c == '>')
            .unwrap_or(r.len());
        let key = &r[..end];
        if !key.is_empty() {
            path.push(key.to_string());
        }
        rest = &r[end..];
        if rest.is_empty()
            || rest.starts_with(char::is_whitespace)
            || rest.starts_with(['=', '!', '<', '>'])
        {
            break;
        }
    }

    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(FilterExpr::Exists(path));
    }

    let (op, value_str) = if let Some(r) = rest.strip_prefix("==") {
        (CmpOp::Eq, r.trim())
    } else if let Some(r) = rest.strip_prefix("!=") {
        (CmpOp::Ne, r.trim())
    } else if let Some(r) = rest.strip_prefix("<=") {
        (CmpOp::Le, r.trim())
    } else if let Some(r) = rest.strip_prefix(">=") {
        (CmpOp::Ge, r.trim())
    } else if let Some(r) = rest.strip_prefix('<') {
        (CmpOp::Lt, r.trim())
    } else if let Some(r) = rest.strip_prefix('>') {
        (CmpOp::Gt, r.trim())
    } else if let Some(r) = rest.strip_prefix('=') {
        (CmpOp::Eq, r.trim())
    } else {
        return Ok(FilterExpr::Exists(path));
    };

    let value = parse_jsonpath_literal(value_str)?;
    Ok(FilterExpr::Compare { path, op, value })
}

fn parse_jsonpath_literal(s: &str) -> Result<serde_json::Value, DbError> {
    match s {
        "null" => return Ok(serde_json::Value::Null),
        "true" => return Ok(serde_json::Value::Bool(true)),
        "false" => return Ok(serde_json::Value::Bool(false)),
        _ => {}
    }
    if let Ok(n) = s.parse::<i64>() {
        return Ok(serde_json::json!(n));
    }
    if let Ok(f) = s.parse::<f64>() {
        return Ok(serde_json::json!(f));
    }
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        return Ok(serde_json::Value::String(s[1..s.len() - 1].to_string()));
    }
    Err(DbError::InvalidValue {
        reason: format!("invalid JSONPath literal: {s}"),
    })
}

// ── JSONPath executor (lax mode) ──────────────────────────────────────────────

fn execute_jsonpath<'a>(
    root: &'a serde_json::Value,
    steps: &[PathStep],
) -> Vec<&'a serde_json::Value> {
    let start = if matches!(steps.first(), Some(PathStep::Root)) {
        &steps[1..]
    } else {
        steps
    };
    apply_steps(root, start)
}

fn apply_steps<'a>(
    current: &'a serde_json::Value,
    steps: &[PathStep],
) -> Vec<&'a serde_json::Value> {
    if steps.is_empty() {
        return vec![current];
    }
    let (step, rest) = (&steps[0], &steps[1..]);
    match step {
        PathStep::Root => apply_steps(current, rest),

        PathStep::Key(key) => match current {
            serde_json::Value::Object(map) => match map.get(key.as_str()) {
                Some(v) => apply_steps(v, rest),
                None => vec![],
            },
            serde_json::Value::Array(arr) => arr
                .iter()
                .flat_map(|elem| apply_steps(elem, steps))
                .collect(),
            _ => vec![],
        },

        PathStep::Index(idx) => match current {
            serde_json::Value::Array(arr) => match arr.get(*idx) {
                Some(v) => apply_steps(v, rest),
                None => vec![],
            },
            _ => vec![],
        },

        PathStep::WildcardKey => match current {
            serde_json::Value::Object(map) => {
                map.values().flat_map(|v| apply_steps(v, rest)).collect()
            }
            serde_json::Value::Array(arr) => arr
                .iter()
                .flat_map(|elem| apply_steps(elem, steps))
                .collect(),
            _ => vec![],
        },

        PathStep::WildcardIndex => match current {
            serde_json::Value::Array(arr) => {
                arr.iter().flat_map(|v| apply_steps(v, rest)).collect()
            }
            _ => vec![],
        },

        PathStep::Recursive => {
            let mut all_nodes = vec![current];
            collect_recursive(current, &mut all_nodes);
            all_nodes
                .into_iter()
                .flat_map(|node| apply_steps(node, rest))
                .collect()
        }

        PathStep::Filter(filter_expr) => match current {
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter(|elem| eval_filter(elem, filter_expr))
                .flat_map(|elem| apply_steps(elem, rest))
                .collect(),
            serde_json::Value::Object(_) => {
                if eval_filter(current, filter_expr) {
                    apply_steps(current, rest)
                } else {
                    vec![]
                }
            }
            _ => vec![],
        },
    }
}

fn collect_recursive<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
    match v {
        serde_json::Value::Object(map) => {
            for val in map.values() {
                out.push(val);
                collect_recursive(val, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for elem in arr {
                out.push(elem);
                collect_recursive(elem, out);
            }
        }
        _ => {}
    }
}

fn eval_filter(node: &serde_json::Value, filter: &FilterExpr) -> bool {
    match filter {
        FilterExpr::Exists(path) => {
            let mut current = node;
            for key in path {
                match current.get(key.as_str()) {
                    Some(v) => current = v,
                    None => return false,
                }
            }
            true
        }
        FilterExpr::Compare { path, op, value } => {
            let mut current = node;
            for key in path {
                match current.get(key.as_str()) {
                    Some(v) => current = v,
                    None => return false,
                }
            }
            compare_json(current, *op, value)
        }
    }
}

fn compare_json(left: &serde_json::Value, op: CmpOp, right: &serde_json::Value) -> bool {
    use serde_json::Value as J;
    match (left, right) {
        (J::Number(a), J::Number(b)) => {
            let af = a.as_f64().unwrap_or(f64::NAN);
            let bf = b.as_f64().unwrap_or(f64::NAN);
            match op {
                CmpOp::Eq => (af - bf).abs() < f64::EPSILON,
                CmpOp::Ne => (af - bf).abs() >= f64::EPSILON,
                CmpOp::Lt => af < bf,
                CmpOp::Le => af <= bf,
                CmpOp::Gt => af > bf,
                CmpOp::Ge => af >= bf,
            }
        }
        (J::String(a), J::String(b)) => match op {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        },
        (J::Bool(a), J::Bool(b)) => match op {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            _ => false,
        },
        (J::Null, J::Null) => matches!(op, CmpOp::Eq),
        _ => false,
    }
}
