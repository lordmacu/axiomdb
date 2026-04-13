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

        // ── Phase 11.21a: PG jsonb_path_* family ─────────────────────────────
        "jsonb_path_exists" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let path_str = eval_path_arg_raw(args, 1, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&json_val)?;
            let steps = parse_jsonpath(&path_str)?;
            Ok(Value::Bool(!execute_jsonpath_owned(&sj, &steps).is_empty()))
        }
        "jsonb_path_query" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let path_str = eval_path_arg_raw(args, 1, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&json_val)?;
            let steps = parse_jsonpath(&path_str)?;
            let arr = execute_jsonpath_owned(&sj, &steps);
            Ok(Value::Json(serde_json::Value::Array(arr).to_string()))
        }
        "jsonb_path_query_first" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let path_str = eval_path_arg_raw(args, 1, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&json_val)?;
            let steps = parse_jsonpath(&path_str)?;
            match execute_jsonpath_owned(&sj, &steps).into_iter().next() {
                None => Ok(Value::Null),
                Some(v) => Ok(serde_json_to_sql_value(Some(&v))),
            }
        }
        "jsonb_path_query_array" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let path_str = eval_path_arg_raw(args, 1, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&json_val)?;
            let steps = parse_jsonpath(&path_str)?;
            let arr = serde_json::Value::Array(execute_jsonpath_owned(&sj, &steps));
            jsonb_blob_from_serde(&arr)
        }
        "jsonb_path_match" => {
            expect_arg_count(name, args, 2)?;
            let json_val = eval_arg(args, 0, row, name)?;
            let path_str = eval_path_arg_raw(args, 1, row, name)?;
            if matches!(json_val, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&json_val)?;
            let steps = parse_jsonpath(&path_str)?;
            let results = execute_jsonpath_owned(&sj, &steps);
            if results.len() != 1 {
                return Ok(Value::Null);
            }
            match &results[0] {
                serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
                _ => Ok(Value::Null),
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

        // ── Phase 11.22a: JSONB mutation parity ──────────────────────────────
        //
        // jsonb_set(target, path, new_value [, create_if_missing=true])
        //   — PG upsert. Returns Jsonb.
        "jsonb_set" => {
            if args.len() < 3 || args.len() > 4 {
                return Err(DbError::TypeMismatch {
                    expected: "jsonb_set: 3 or 4 args".into(),
                    got: args.len().to_string(),
                });
            }
            let target = eval_arg(args, 0, row, name)?;
            if matches!(target, Value::Null) {
                return Ok(Value::Null);
            }
            let path_arg = eval_arg(args, 1, row, name)?;
            let new_val = eval_arg(args, 2, row, name)?;
            let create_if_missing = if args.len() == 4 {
                is_truthy_arg(&eval_arg(args, 3, row, name)?)
            } else {
                true
            };
            let mut sj = value_to_serde_json(&target)?;
            let parts = parse_mutation_path(&path_arg)?;
            set_path_ext(
                &mut sj,
                &parts,
                sql_to_serde_json(&new_val),
                MutationFlags {
                    create_if_missing,
                    insert_after: false,
                    raise_on_existing_key: false,
                    allow_insert: false,
                },
            )?;
            jsonb_blob_from_serde(&sj)
        }

        // jsonb_insert(target, path, new_value [, insert_after=false])
        //   — PG insert. Raises on existing object key (matches PG).
        "jsonb_insert" => {
            if args.len() < 3 || args.len() > 4 {
                return Err(DbError::TypeMismatch {
                    expected: "jsonb_insert: 3 or 4 args".into(),
                    got: args.len().to_string(),
                });
            }
            let target = eval_arg(args, 0, row, name)?;
            if matches!(target, Value::Null) {
                return Ok(Value::Null);
            }
            let path_arg = eval_arg(args, 1, row, name)?;
            let new_val = eval_arg(args, 2, row, name)?;
            let insert_after = if args.len() == 4 {
                is_truthy_arg(&eval_arg(args, 3, row, name)?)
            } else {
                false
            };
            let mut sj = value_to_serde_json(&target)?;
            let parts = parse_mutation_path(&path_arg)?;
            set_path_ext(
                &mut sj,
                &parts,
                sql_to_serde_json(&new_val),
                MutationFlags {
                    create_if_missing: false,
                    insert_after,
                    raise_on_existing_key: true,
                    allow_insert: true,
                },
            )?;
            jsonb_blob_from_serde(&sj)
        }

        // jsonb_delete_path(target, path)
        //   — PG #-. Empty path returns target unchanged. Scalar root errors.
        "jsonb_delete_path" => {
            expect_arg_count(name, args, 2)?;
            let target = eval_arg(args, 0, row, name)?;
            if matches!(target, Value::Null) {
                return Ok(Value::Null);
            }
            let path_arg = eval_arg(args, 1, row, name)?;
            let parts = parse_mutation_path(&path_arg)?;
            let mut sj = value_to_serde_json(&target)?;
            if !parts.is_empty() {
                if sj.is_object() || sj.is_array() {
                    remove_path_parts(&mut sj, &parts);
                } else {
                    return Err(DbError::InvalidValue {
                        reason: "cannot delete path in scalar JSONB".into(),
                    });
                }
            }
            jsonb_blob_from_serde(&sj)
        }

        // json_insert(doc, p1, v1, p2, v2, ...) — MySQL variadic.
        // Adds key only if path missing; silent no-op on existing key
        // (diverges from PG jsonb_insert).
        "json_insert" => {
            if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
                return Err(DbError::TypeMismatch {
                    expected: "json_insert: odd arg count (doc + path/value pairs)".into(),
                    got: args.len().to_string(),
                });
            }
            let doc = eval_arg(args, 0, row, name)?;
            if matches!(doc, Value::Null) {
                return Ok(Value::Null);
            }
            let mut sj = value_to_serde_json(&doc)?;
            let pair_count = (args.len() - 1) / 2;
            for i in 0..pair_count {
                let p = eval_arg(args, 1 + 2 * i, row, name)?;
                let v = eval_arg(args, 2 + 2 * i, row, name)?;
                let parts = parse_mutation_path(&p)?;
                if path_exists(&sj, &parts) {
                    continue; // MySQL: silent no-op on existing
                }
                set_path_ext(
                    &mut sj,
                    &parts,
                    sql_to_serde_json(&v),
                    MutationFlags {
                        create_if_missing: true,
                        insert_after: false,
                        raise_on_existing_key: false,
                        allow_insert: false,
                    },
                )?;
            }
            Ok(Value::Json(sj.to_string()))
        }

        // json_replace(doc, p1, v1, p2, v2, ...) — MySQL variadic.
        // Updates only if path exists; silent no-op on missing path.
        "json_replace" => {
            if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
                return Err(DbError::TypeMismatch {
                    expected: "json_replace: odd arg count (doc + path/value pairs)".into(),
                    got: args.len().to_string(),
                });
            }
            let doc = eval_arg(args, 0, row, name)?;
            if matches!(doc, Value::Null) {
                return Ok(Value::Null);
            }
            let mut sj = value_to_serde_json(&doc)?;
            let pair_count = (args.len() - 1) / 2;
            for i in 0..pair_count {
                let p = eval_arg(args, 1 + 2 * i, row, name)?;
                let v = eval_arg(args, 2 + 2 * i, row, name)?;
                let parts = parse_mutation_path(&p)?;
                if !path_exists(&sj, &parts) {
                    continue; // MySQL: silent no-op on missing
                }
                set_path_ext(
                    &mut sj,
                    &parts,
                    sql_to_serde_json(&v),
                    MutationFlags {
                        create_if_missing: false,
                        insert_after: false,
                        raise_on_existing_key: false,
                        allow_insert: false,
                    },
                )?;
            }
            Ok(Value::Json(sj.to_string()))
        }

        // ── Phase 11.22b: jsonb_set_lax ─────────────────────────────────────
        // jsonb_set_lax(target, path, new_value [, create_if_missing=true
        //               [, null_value_treatment='use_json_null']])
        // SQL-NULL new_value dispatches on null_value_treatment enum.
        "jsonb_set_lax" => {
            if args.len() < 3 || args.len() > 5 {
                return Err(DbError::TypeMismatch {
                    expected: "jsonb_set_lax: 3, 4 or 5 args".into(),
                    got: args.len().to_string(),
                });
            }
            let target = eval_arg(args, 0, row, name)?;
            let path_arg = eval_arg(args, 1, row, name)?;
            if matches!(target, Value::Null) || matches!(path_arg, Value::Null) {
                return Ok(Value::Null);
            }
            let new_val = eval_arg(args, 2, row, name)?;
            let create_if_missing = if args.len() >= 4 {
                let v = eval_arg(args, 3, row, name)?;
                if matches!(v, Value::Null) {
                    return Ok(Value::Null);
                }
                is_truthy_arg(&v)
            } else {
                true
            };
            let treatment: String = if args.len() == 5 {
                let v = eval_arg(args, 4, row, name)?;
                match v {
                    Value::Null => {
                        return Err(DbError::InvalidValue {
                            reason: "null_value_treatment must be \"delete_key\", \
                                \"return_target\", \"use_json_null\", or \
                                \"raise_exception\""
                                .into(),
                        });
                    }
                    Value::Text(s) | Value::Json(s) => s,
                    other => other.to_string(),
                }
            } else {
                "use_json_null".into()
            };

            let parts = parse_mutation_path(&path_arg)?;

            // non-null new_value → plain jsonb_set
            if !matches!(new_val, Value::Null) {
                let mut sj = value_to_serde_json(&target)?;
                set_path_ext(
                    &mut sj,
                    &parts,
                    sql_to_serde_json(&new_val),
                    MutationFlags {
                        create_if_missing,
                        insert_after: false,
                        raise_on_existing_key: false,
                        allow_insert: false,
                    },
                )?;
                return jsonb_blob_from_serde(&sj);
            }

            match treatment.as_str() {
                "use_json_null" => {
                    let mut sj = value_to_serde_json(&target)?;
                    set_path_ext(
                        &mut sj,
                        &parts,
                        serde_json::Value::Null,
                        MutationFlags {
                            create_if_missing,
                            insert_after: false,
                            raise_on_existing_key: false,
                            allow_insert: false,
                        },
                    )?;
                    jsonb_blob_from_serde(&sj)
                }
                "raise_exception" => Err(DbError::InvalidValue {
                    reason: "JSON value must not be null (null_value_treatment = \
                        raise_exception)"
                        .into(),
                }),
                "delete_key" => {
                    let mut sj = value_to_serde_json(&target)?;
                    if !parts.is_empty() {
                        if sj.is_object() || sj.is_array() {
                            remove_path_parts(&mut sj, &parts);
                        } else {
                            return Err(DbError::InvalidValue {
                                reason: "cannot delete path in scalar JSONB".into(),
                            });
                        }
                    }
                    jsonb_blob_from_serde(&sj)
                }
                "return_target" => {
                    let sj = value_to_serde_json(&target)?;
                    jsonb_blob_from_serde(&sj)
                }
                _ => Err(DbError::InvalidValue {
                    reason: "null_value_treatment must be \"delete_key\", \
                        \"return_target\", \"use_json_null\", or \"raise_exception\""
                        .into(),
                }),
            }
        }

        // ── Phase 11.24d: Oracle JSON_DATAGUIDE ──────────────────────────────
        // JSON_DATAGUIDE(doc) → JSON array of {path, type} entries describing
        // every reachable subpath and its JSON type. Compact Oracle-Data-Guide
        // analog. NULL doc → NULL. Format-spec args (ORDERED, FORMAT) accepted
        // but ignored in MVP.
        "json_dataguide" => {
            if args.is_empty() || args.len() > 3 {
                return Err(DbError::TypeMismatch {
                    expected: "json_dataguide: 1 to 3 args".into(),
                    got: args.len().to_string(),
                });
            }
            let doc = eval_arg(args, 0, row, name)?;
            if matches!(doc, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&doc)?;
            let mut entries = Vec::<serde_json::Value>::new();
            json_dataguide_walk(&sj, "$", &mut entries);
            Ok(Value::Json(serde_json::Value::Array(entries).to_string()))
        }

        // ── Phase 11.24b: JSON_TRANSFORM (function-form variadic) ───────────
        // Syntax (function-form, not Oracle special-form):
        //   JSON_TRANSFORM(doc, 'SET', path, val, 'REMOVE', path,
        //                       'RENAME', path, new_name,
        //                       'APPEND', path, val,
        //                       'INSERT', path, val,
        //                       'REPLACE', path, val, ...)
        // Ops consume their own arg counts (SET=3, REMOVE=1, RENAME=2,
        // APPEND=2, INSERT=2, REPLACE=2). Applied sequentially.
        "json_transform" => {
            if args.len() < 2 {
                return Err(DbError::TypeMismatch {
                    expected: "json_transform: doc + at least one op".into(),
                    got: args.len().to_string(),
                });
            }
            let doc = eval_arg(args, 0, row, name)?;
            if matches!(doc, Value::Null) {
                return Ok(Value::Null);
            }
            let mut sj = value_to_serde_json(&doc)?;
            let mut i = 1usize;
            while i < args.len() {
                let op_val = eval_arg(args, i, row, name)?;
                let op = match op_val {
                    Value::Text(s) | Value::Json(s) => s.to_ascii_uppercase(),
                    other => {
                        return Err(DbError::TypeMismatch {
                            expected: "json_transform op keyword (TEXT)".into(),
                            got: other.variant_name().into(),
                        });
                    }
                };
                match op.as_str() {
                    "SET" | "REPLACE" => {
                        if i + 2 >= args.len() {
                            return Err(DbError::InvalidValue {
                                reason: format!("json_transform {op} needs path + value"),
                            });
                        }
                        let p = eval_arg(args, i + 1, row, name)?;
                        let v = eval_arg(args, i + 2, row, name)?;
                        let parts = parse_mutation_path(&p)?;
                        set_path_ext(
                            &mut sj,
                            &parts,
                            sql_to_serde_json(&v),
                            MutationFlags {
                                create_if_missing: op == "SET",
                                insert_after: false,
                                raise_on_existing_key: false,
                                allow_insert: false,
                            },
                        )?;
                        i += 3;
                    }
                    "REMOVE" => {
                        if i + 1 >= args.len() {
                            return Err(DbError::InvalidValue {
                                reason: "json_transform REMOVE needs path".into(),
                            });
                        }
                        let p = eval_arg(args, i + 1, row, name)?;
                        let parts = parse_mutation_path(&p)?;
                        if !parts.is_empty() && (sj.is_object() || sj.is_array()) {
                            remove_path_parts(&mut sj, &parts);
                        }
                        i += 2;
                    }
                    "RENAME" => {
                        if i + 2 >= args.len() {
                            return Err(DbError::InvalidValue {
                                reason: "json_transform RENAME needs path + new_name".into(),
                            });
                        }
                        let p = eval_arg(args, i + 1, row, name)?;
                        let newn = eval_arg(args, i + 2, row, name)?;
                        let parts = parse_mutation_path(&p)?;
                        let new_name = match newn {
                            Value::Text(s) | Value::Json(s) => s,
                            other => other.to_string(),
                        };
                        json_rename_at(&mut sj, &parts, &new_name);
                        i += 3;
                    }
                    "APPEND" => {
                        if i + 2 >= args.len() {
                            return Err(DbError::InvalidValue {
                                reason: "json_transform APPEND needs path + value".into(),
                            });
                        }
                        let p = eval_arg(args, i + 1, row, name)?;
                        let v = eval_arg(args, i + 2, row, name)?;
                        let parts = parse_mutation_path(&p)?;
                        json_array_append_at(&mut sj, &parts, sql_to_serde_json(&v));
                        i += 3;
                    }
                    "INSERT" => {
                        if i + 2 >= args.len() {
                            return Err(DbError::InvalidValue {
                                reason: "json_transform INSERT needs path + value".into(),
                            });
                        }
                        let p = eval_arg(args, i + 1, row, name)?;
                        let v = eval_arg(args, i + 2, row, name)?;
                        let parts = parse_mutation_path(&p)?;
                        if !path_exists(&sj, &parts) {
                            set_path_ext(
                                &mut sj,
                                &parts,
                                sql_to_serde_json(&v),
                                MutationFlags {
                                    create_if_missing: true,
                                    insert_after: false,
                                    raise_on_existing_key: false,
                                    allow_insert: false,
                                },
                            )?;
                        }
                        i += 3;
                    }
                    other => {
                        return Err(DbError::InvalidValue {
                            reason: format!(
                                "json_transform: unknown op '{other}' (expected SET/REMOVE/RENAME/APPEND/INSERT/REPLACE)"
                            ),
                        });
                    }
                }
            }
            Ok(Value::Json(sj.to_string()))
        }

        // ── Phase 11.25c: MySQL JSON_SEARCH ──────────────────────────────────
        // JSON_SEARCH(doc, one|all, search_str [, escape_char [, path ...]])
        // Returns matching JSON paths ('$.a.b') whose string values match the
        // LIKE pattern. MVP: escape_char and path filters accepted but ignored
        // (full-doc walk). NULL args propagate. No match → NULL.
        "json_search" => {
            if args.len() < 3 {
                return Err(DbError::TypeMismatch {
                    expected: "json_search: doc + mode + pattern [...]".into(),
                    got: args.len().to_string(),
                });
            }
            let doc = eval_arg(args, 0, row, name)?;
            let mode_v = eval_arg(args, 1, row, name)?;
            let pat_v = eval_arg(args, 2, row, name)?;
            if matches!(doc, Value::Null)
                || matches!(mode_v, Value::Null)
                || matches!(pat_v, Value::Null)
            {
                return Ok(Value::Null);
            }
            let mode = match mode_v {
                Value::Text(s) | Value::Json(s) => s.to_ascii_lowercase(),
                other => other.to_string().to_ascii_lowercase(),
            };
            if mode != "one" && mode != "all" {
                return Err(DbError::InvalidValue {
                    reason: format!("json_search mode must be 'one' or 'all', got '{mode}'"),
                });
            }
            let pat = match pat_v {
                Value::Text(s) | Value::Json(s) => s,
                other => other.to_string(),
            };
            let sj = value_to_serde_json(&doc)?;
            let want_all = mode == "all";
            let mut hits: Vec<String> = Vec::new();
            json_search_walk(&sj, "$", &pat, &mut hits, !want_all);
            if hits.is_empty() {
                return Ok(Value::Null);
            }
            if want_all {
                Ok(Value::Json(
                    serde_json::Value::Array(
                        hits.into_iter().map(serde_json::Value::String).collect(),
                    )
                    .to_string(),
                ))
            } else {
                Ok(Value::Text(hits.into_iter().next().unwrap()))
            }
        }

        // ── Phase 11.25b: JSON constructors + merge_preserve + contains_path
        // JSON_ARRAY(v1, v2, ...) → JSON array.
        "json_array" => {
            let mut items = Vec::with_capacity(args.len());
            for i in 0..args.len() {
                let v = eval_arg(args, i, row, name)?;
                items.push(sql_to_serde_json(&v));
            }
            Ok(Value::Json(serde_json::Value::Array(items).to_string()))
        }
        // JSON_OBJECT(k1, v1, k2, v2, ...) → JSON object. Even arg count.
        "json_object" => {
            if !args.len().is_multiple_of(2) {
                return Err(DbError::TypeMismatch {
                    expected: "json_object: even arg count (key/value pairs)".into(),
                    got: args.len().to_string(),
                });
            }
            let mut map = serde_json::Map::with_capacity(args.len() / 2);
            let pair_count = args.len() / 2;
            for i in 0..pair_count {
                let k = eval_arg(args, 2 * i, row, name)?;
                let v = eval_arg(args, 2 * i + 1, row, name)?;
                let key = match k {
                    Value::Text(s) | Value::Json(s) => s,
                    Value::Null => {
                        return Err(DbError::InvalidValue {
                            reason: "json_object: NULL key".into(),
                        });
                    }
                    other => other.to_string(),
                };
                map.insert(key, sql_to_serde_json(&v));
            }
            Ok(Value::Json(serde_json::Value::Object(map).to_string()))
        }
        // JSON_MERGE_PRESERVE(d1, d2, ...) — array-concat on conflict,
        // object-key overwrite from right. Mirrors MySQL deprecated-alias
        // JSON_MERGE semantics.
        "json_merge_preserve" | "json_merge" => {
            if args.is_empty() {
                return Err(DbError::TypeMismatch {
                    expected: "json_merge_preserve: at least 1 arg".into(),
                    got: "0".into(),
                });
            }
            let mut acc: Option<serde_json::Value> = None;
            for i in 0..args.len() {
                let v = eval_arg(args, i, row, name)?;
                if matches!(v, Value::Null) {
                    return Ok(Value::Null);
                }
                let sj = value_to_serde_json(&v)?;
                acc = Some(match acc {
                    None => sj,
                    Some(a) => merge_preserve(a, sj),
                });
            }
            let merged = acc.unwrap();
            Ok(Value::Json(merged.to_string()))
        }
        // JSON_CONTAINS_PATH(doc, 'one'|'all', p1, p2, ...) — existence check.
        "json_contains_path" => {
            if args.len() < 3 {
                return Err(DbError::TypeMismatch {
                    expected: "json_contains_path: doc + mode + path [, path ...]".into(),
                    got: args.len().to_string(),
                });
            }
            let doc = eval_arg(args, 0, row, name)?;
            if matches!(doc, Value::Null) {
                return Ok(Value::Null);
            }
            let mode_val = eval_arg(args, 1, row, name)?;
            let mode = match mode_val {
                Value::Text(s) | Value::Json(s) => s.to_ascii_lowercase(),
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "'one' or 'all'".into(),
                        got: other.variant_name().into(),
                    });
                }
            };
            if mode != "one" && mode != "all" {
                return Err(DbError::InvalidValue {
                    reason: format!("json_contains_path mode must be 'one' or 'all', got '{mode}'"),
                });
            }
            let sj = value_to_serde_json(&doc)?;
            let want_all = mode == "all";
            let mut any_hit = false;
            let mut all_hit = true;
            for i in 2..args.len() {
                let p = eval_arg(args, i, row, name)?;
                let path_str = match p {
                    Value::Text(s) | Value::Json(s) => s,
                    Value::Null => return Ok(Value::Null),
                    other => other.to_string(),
                };
                let steps = parse_jsonpath(&path_str)?;
                if execute_jsonpath(&sj, &steps).is_empty() {
                    all_hit = false;
                    if want_all {
                        return Ok(Value::Bool(false));
                    }
                } else {
                    any_hit = true;
                    if !want_all {
                        return Ok(Value::Bool(true));
                    }
                }
            }
            Ok(Value::Bool(if want_all { all_hit } else { any_hit }))
        }

        // ── Phase 11.25a: MySQL JSON completion bundle ───────────────────────
        // JSON_QUOTE(text) — serialize a TEXT string as a JSON string literal.
        "json_quote" => {
            expect_arg_count(name, args, 1)?;
            let v = eval_arg(args, 0, row, name)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            let s = match v {
                Value::Text(s) | Value::Json(s) => s,
                other => other.to_string(),
            };
            Ok(Value::Json(serde_json::Value::String(s).to_string()))
        }
        // JSON_UNQUOTE(json) — strip outer double-quotes, decode JSON escapes.
        // Non-string input returned unchanged (as its text form).
        "json_unquote" => {
            expect_arg_count(name, args, 1)?;
            let v = eval_arg(args, 0, row, name)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&v)?;
            match sj {
                serde_json::Value::String(s) => Ok(Value::Text(s)),
                other => Ok(Value::Text(other.to_string())),
            }
        }
        // JSON_LENGTH(json [, path]) — number of top-level elements of the doc
        // (or at path). Object → key count, array → length, scalar → 1.
        "json_length" => {
            if args.len() != 1 && args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "json_length: 1 or 2 args".into(),
                    got: args.len().to_string(),
                });
            }
            let v = eval_arg(args, 0, row, name)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            let mut sj = value_to_serde_json(&v)?;
            if args.len() == 2 {
                let path = eval_path_arg(args, 1, row, name)?;
                match extract_path(&sj, &path)? {
                    Some(t) => sj = t.clone(),
                    None => return Ok(Value::Null),
                }
            }
            let len: i64 = match &sj {
                serde_json::Value::Array(a) => a.len() as i64,
                serde_json::Value::Object(o) => o.len() as i64,
                _ => 1,
            };
            Ok(Value::BigInt(len))
        }
        // JSON_STORAGE_SIZE(json) — byte length of the JSONB encoding.
        "json_storage_size" => {
            expect_arg_count(name, args, 1)?;
            let v = eval_arg(args, 0, row, name)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            match v {
                Value::Jsonb(b) => Ok(Value::BigInt(b.len() as i64)),
                other => {
                    let sj = value_to_serde_json(&other)?;
                    let blob = JsonbEncoder::encode(&sj)?;
                    Ok(Value::BigInt(blob.len() as i64))
                }
            }
        }
        // JSON_ARRAY_APPEND(doc, path, val, [path, val]*) — append val to array
        // at path; if target is non-array, it is wrapped in an array first.
        "json_array_append" => {
            if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
                return Err(DbError::TypeMismatch {
                    expected: "json_array_append: doc + path/value pairs".into(),
                    got: args.len().to_string(),
                });
            }
            let doc = eval_arg(args, 0, row, name)?;
            if matches!(doc, Value::Null) {
                return Ok(Value::Null);
            }
            let mut sj = value_to_serde_json(&doc)?;
            let pair_count = (args.len() - 1) / 2;
            for i in 0..pair_count {
                let p = eval_arg(args, 1 + 2 * i, row, name)?;
                let val = eval_arg(args, 2 + 2 * i, row, name)?;
                let parts = parse_mutation_path(&p)?;
                json_array_append_at(&mut sj, &parts, sql_to_serde_json(&val));
            }
            Ok(Value::Json(sj.to_string()))
        }
        // JSON_ARRAY_INSERT(doc, path, val, [path, val]*) — path must end with
        // [idx]; insert val at that position, shifting the rest right.
        "json_array_insert" => {
            if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
                return Err(DbError::TypeMismatch {
                    expected: "json_array_insert: doc + path/value pairs".into(),
                    got: args.len().to_string(),
                });
            }
            let doc = eval_arg(args, 0, row, name)?;
            if matches!(doc, Value::Null) {
                return Ok(Value::Null);
            }
            let mut sj = value_to_serde_json(&doc)?;
            let pair_count = (args.len() - 1) / 2;
            for i in 0..pair_count {
                let p = eval_arg(args, 1 + 2 * i, row, name)?;
                let val = eval_arg(args, 2 + 2 * i, row, name)?;
                let parts = parse_mutation_path(&p)?;
                if parts.is_empty() {
                    return Err(DbError::InvalidValue {
                        reason: "json_array_insert: path must end with [idx]".into(),
                    });
                }
                let (last, parents) = parts.split_last().unwrap();
                let idx: usize = last.parse().map_err(|_| DbError::InvalidValue {
                    reason: format!(
                        "json_array_insert: final path segment must be array index, got `{last}`"
                    ),
                })?;
                json_array_insert_at(&mut sj, parents, idx, sql_to_serde_json(&val));
            }
            Ok(Value::Json(sj.to_string()))
        }

        // ── Phase 11.23a: JSON Schema Draft-07 subset validator ──────────────
        // JSON_SCHEMA_VALID(schema, doc) → bool. NULL on either side → NULL.
        "json_schema_valid" => {
            expect_arg_count(name, args, 2)?;
            let schema = eval_arg(args, 0, row, name)?;
            let doc = eval_arg(args, 1, row, name)?;
            if matches!(schema, Value::Null) || matches!(doc, Value::Null) {
                return Ok(Value::Null);
            }
            let schema_sj = value_to_serde_json(&schema)?;
            let doc_sj = value_to_serde_json(&doc)?;
            Ok(Value::Bool(json_schema_validate(&schema_sj, &doc_sj)))
        }

        // JSON_SCHEMA_VALIDATION_REPORT(schema, doc) — Phase 11.23b.
        // Returns a JSON array of {path, keyword, message} error objects.
        // Empty array means the document is valid.
        "json_schema_validation_report" => {
            expect_arg_count(name, args, 2)?;
            let schema = eval_arg(args, 0, row, name)?;
            let doc = eval_arg(args, 1, row, name)?;
            if matches!(schema, Value::Null) || matches!(doc, Value::Null) {
                return Ok(Value::Null);
            }
            let schema_sj = value_to_serde_json(&schema)?;
            let doc_sj = value_to_serde_json(&doc)?;
            let mut errors = Vec::<serde_json::Value>::new();
            collect_schema_errors(
                &schema_sj,
                &doc_sj,
                &schema_sj,
                0,
                "#".to_string(),
                &mut errors,
            );
            Ok(Value::Json(serde_json::Value::Array(errors).to_string()))
        }

        // ── Phase 11.24a: Oracle JSON surface ────────────────────────────────
        // JSON_EQUAL(a, b) — deep structural equality. NULL if either is NULL.
        "json_equal" => {
            expect_arg_count(name, args, 2)?;
            let a = eval_arg(args, 0, row, name)?;
            let b = eval_arg(args, 1, row, name)?;
            if matches!(a, Value::Null) || matches!(b, Value::Null) {
                return Ok(Value::Null);
            }
            let sa = value_to_serde_json(&a)?;
            let sb = value_to_serde_json(&b)?;
            Ok(Value::Bool(sa == sb))
        }
        // JSON_SCALAR(v) — wrap a SQL scalar in a JSONB scalar value.
        "json_scalar" => {
            expect_arg_count(name, args, 1)?;
            let v = eval_arg(args, 0, row, name)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = sql_to_serde_json(&v);
            jsonb_blob_from_serde(&sj)
        }
        // JSON_SERIALIZE(jsonb) — render JSONB/JSON as canonical TEXT.
        "json_serialize" => {
            expect_arg_count(name, args, 1)?;
            let v = eval_arg(args, 0, row, name)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            let sj = value_to_serde_json(&v)?;
            Ok(Value::Text(sj.to_string()))
        }

        _ => unreachable!("dispatcher routed unsupported JSON function"),
    }
}

// ── Phase 11.22a helpers ─────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct MutationFlags {
    /// When the leaf (or intermediate) is missing, create containers / insert
    /// the leaf. PG `jsonb_set(create_if_missing=true)` default.
    create_if_missing: bool,
    /// For array leaves, insert after the indexed position instead of before.
    insert_after: bool,
    /// When the leaf already exists as an object key, raise an error instead
    /// of replacing it. PG `jsonb_insert` behavior.
    raise_on_existing_key: bool,
    /// When `true`, leaf writes on arrays are treated as insertions (before
    /// or after the indexed element) rather than replacements. PG
    /// `jsonb_insert` sets this; everything else leaves it `false`.
    allow_insert: bool,
}

/// Convert a SQL path argument to a schema-neutral `Vec<String>`:
///   - MySQL-style string: `'$.a.b'`, `'$[0]'`, `'$."quoted key"'`
///   - JSON-array literal (PG `text[]` workaround): `'["a","b"]'`
///   - Binary JSONB array (`Value::Jsonb`)
///
/// Wildcards (`$.*`, `$[*]`, `$..key`) are rejected — both PG and MySQL
/// reject them for mutation.
fn parse_mutation_path(arg: &Value) -> Result<Vec<String>, DbError> {
    match arg {
        Value::Text(s) | Value::Json(s) => {
            let trimmed = s.trim_start();
            if let Some(first) = trimmed.chars().next() {
                if first == '$' {
                    // MySQL JSONPath.
                    if s.contains("[*]") || s.contains(".*") || s.contains("..") {
                        return Err(DbError::InvalidValue {
                            reason: format!(
                                "wildcard paths are not allowed in mutation functions: {s}"
                            ),
                        });
                    }
                    return jsonpath_to_parts(s);
                }
                if first == '[' {
                    // PG text[] workaround as JSON-array literal.
                    let parsed: serde_json::Value =
                        serde_json::from_str(s).map_err(|e| DbError::InvalidValue {
                            reason: format!("invalid JSON-array path {s:?}: {e}"),
                        })?;
                    return json_array_to_path_parts(&parsed);
                }
            }
            Err(DbError::InvalidValue {
                reason: format!("unsupported path form: {s:?}"),
            })
        }
        Value::Jsonb(b) => {
            let sj = JsonbDecoder::decode(b.as_ref())?;
            json_array_to_path_parts(&sj)
        }
        Value::Null => Err(DbError::InvalidValue {
            reason: "NULL path argument".into(),
        }),
        other => Err(type_err("text or jsonb array path", other.variant_name())),
    }
}

/// Splits a MySQL-style `$.key.nested[0]."q.k"` path into schema-neutral
/// string parts. Array indices are preserved as their decimal string so
/// `set_path_ext` can parse them against the current container type.
fn jsonpath_to_parts(path: &str) -> Result<Vec<String>, DbError> {
    let mut parts: Vec<String> = Vec::new();
    let mut chars = path.trim_start().chars().peekable();
    if chars.next() != Some('$') {
        return Err(DbError::InvalidValue {
            reason: format!("path must start with $: {path}"),
        });
    }
    while let Some(&c) = chars.peek() {
        if c == '.' {
            chars.next();
            let mut buf = String::new();
            if chars.peek() == Some(&'"') {
                chars.next();
                for ch in chars.by_ref() {
                    if ch == '"' {
                        break;
                    }
                    buf.push(ch);
                }
            } else {
                while let Some(&nc) = chars.peek() {
                    if nc == '.' || nc == '[' {
                        break;
                    }
                    buf.push(nc);
                    chars.next();
                }
            }
            if buf.is_empty() {
                return Err(DbError::InvalidValue {
                    reason: format!("empty key in path: {path}"),
                });
            }
            parts.push(buf);
        } else if c == '[' {
            chars.next();
            let mut buf = String::new();
            for ch in chars.by_ref() {
                if ch == ']' {
                    break;
                }
                buf.push(ch);
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                return Err(DbError::InvalidValue {
                    reason: format!("empty index in path: {path}"),
                });
            }
            parts.push(trimmed.to_string());
        } else {
            return Err(DbError::InvalidValue {
                reason: format!("unexpected character {c:?} in path: {path}"),
            });
        }
    }
    Ok(parts)
}

fn json_array_to_path_parts(v: &serde_json::Value) -> Result<Vec<String>, DbError> {
    match v {
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(|e| match e {
                serde_json::Value::String(s) => Ok(s.clone()),
                serde_json::Value::Number(n) => Ok(n.to_string()),
                other => Err(DbError::InvalidValue {
                    reason: format!(
                        "JSON-array path element must be string or number, got {other}"
                    ),
                }),
            })
            .collect(),
        _ => Err(DbError::InvalidValue {
            reason: "path must be a JSON array or MySQL-style string".into(),
        }),
    }
}

/// Recursively walks `root` along `parts` and applies `new_val` at the leaf
/// honoring `flags`. Returns `Ok(())` on success; common PG errors surface
/// as `DbError::InvalidValue`.
fn set_path_ext(
    root: &mut serde_json::Value,
    parts: &[String],
    new_val: serde_json::Value,
    flags: MutationFlags,
) -> Result<(), DbError> {
    // Empty path — PG: jsonb_set on empty path returns target unchanged
    // (setPath in jsonfuncs.c line 4886). Same for jsonb_insert.
    if parts.is_empty() {
        return Ok(());
    }
    set_path_step(root, parts, 0, new_val, flags)
}

fn set_path_step(
    node: &mut serde_json::Value,
    parts: &[String],
    idx: usize,
    new_val: serde_json::Value,
    flags: MutationFlags,
) -> Result<(), DbError> {
    let is_leaf = idx == parts.len() - 1;
    let step = &parts[idx];

    match node {
        serde_json::Value::Object(map) => {
            let has_key = map.contains_key(step);
            if is_leaf {
                if has_key {
                    if flags.raise_on_existing_key {
                        return Err(DbError::InvalidValue {
                            reason: format!("cannot replace existing key {step:?}"),
                        });
                    }
                    map.insert(step.clone(), new_val);
                } else if flags.create_if_missing || flags.allow_insert {
                    map.insert(step.clone(), new_val);
                }
                Ok(())
            } else if has_key {
                set_path_step(map.get_mut(step).unwrap(), parts, idx + 1, new_val, flags)
            } else if flags.create_if_missing {
                // Create an empty object for the remaining path.
                map.insert(step.clone(), serde_json::Value::Object(Default::default()));
                set_path_step(map.get_mut(step).unwrap(), parts, idx + 1, new_val, flags)
            } else {
                Ok(())
            }
        }
        serde_json::Value::Array(arr) => {
            let len = arr.len() as i64;
            let parsed: i64 = step.parse().map_err(|_| DbError::InvalidValue {
                reason: format!("path element {step:?} is not an integer for array"),
            })?;
            let real = if parsed < 0 { len + parsed } else { parsed };
            if is_leaf {
                if flags.allow_insert {
                    // PG jsonb_insert — insert before/after index.
                    let clamped = real.max(0).min(len) as usize;
                    let position = if flags.insert_after {
                        (clamped + 1).min(arr.len())
                    } else {
                        clamped
                    };
                    arr.insert(position, new_val);
                } else if real >= 0 && real < len {
                    arr[real as usize] = new_val;
                } else if flags.create_if_missing {
                    if real < 0 {
                        arr.insert(0, new_val);
                    } else {
                        arr.push(new_val);
                    }
                }
                Ok(())
            } else if real >= 0 && real < len {
                set_path_step(&mut arr[real as usize], parts, idx + 1, new_val, flags)
            } else if flags.create_if_missing {
                // Extend the array with an empty object intermediate.
                arr.push(serde_json::Value::Object(Default::default()));
                let last = arr.len() - 1;
                set_path_step(&mut arr[last], parts, idx + 1, new_val, flags)
            } else {
                Ok(())
            }
        }
        _ => Err(DbError::InvalidValue {
            reason: "cannot set path in scalar JSONB".into(),
        }),
    }
}

fn remove_path_parts(node: &mut serde_json::Value, parts: &[String]) {
    if parts.is_empty() {
        return;
    }
    remove_path_step(node, parts, 0);
}

fn remove_path_step(node: &mut serde_json::Value, parts: &[String], idx: usize) {
    let step = &parts[idx];
    let is_leaf = idx == parts.len() - 1;
    match node {
        serde_json::Value::Object(map) => {
            if is_leaf {
                map.remove(step);
            } else if let Some(next) = map.get_mut(step) {
                remove_path_step(next, parts, idx + 1);
            }
        }
        serde_json::Value::Array(arr) => {
            let len = arr.len() as i64;
            let Ok(parsed) = step.parse::<i64>() else {
                return;
            };
            let real = if parsed < 0 { len + parsed } else { parsed };
            if real < 0 || real >= len {
                return;
            }
            if is_leaf {
                arr.remove(real as usize);
            } else {
                remove_path_step(&mut arr[real as usize], parts, idx + 1);
            }
        }
        _ => {}
    }
}

fn path_exists(root: &serde_json::Value, parts: &[String]) -> bool {
    let mut cur = root;
    for step in parts {
        match cur {
            serde_json::Value::Object(map) => match map.get(step) {
                Some(next) => cur = next,
                None => return false,
            },
            serde_json::Value::Array(arr) => {
                let Ok(parsed) = step.parse::<i64>() else {
                    return false;
                };
                let len = arr.len() as i64;
                let real = if parsed < 0 { len + parsed } else { parsed };
                if real < 0 || real >= len {
                    return false;
                }
                cur = &arr[real as usize];
            }
            _ => return false,
        }
    }
    true
}

fn jsonb_blob_from_serde(v: &serde_json::Value) -> Result<Value, DbError> {
    let blob = JsonbEncoder::encode(v)?;
    Ok(Value::Jsonb(Arc::new(blob)))
}

fn is_truthy_arg(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Int(n) => *n != 0,
        Value::BigInt(n) => *n != 0,
        _ => false,
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
pub(crate) enum PathStep {
    Root,
    Key(String),
    Index(usize),
    WildcardKey,
    WildcardIndex,
    Recursive,
    Filter(FilterExpr),
    /// `.size()` accessor — returns the array length, or 1 for non-arrays (PG parity).
    Size,
    /// `.type()` accessor — returns the JSON type name as a string.
    TypeOf,
}

#[derive(Debug, Clone)]
pub(crate) enum FilterExpr {
    Exists(Vec<String>),
    Compare {
        lhs: FilterSide,
        op: CmpOp,
        rhs: FilterSide,
    },
    And(Box<FilterExpr>, Box<FilterExpr>),
    Or(Box<FilterExpr>, Box<FilterExpr>),
    Not(Box<FilterExpr>),
}

/// Right-hand side of a filter comparison: a literal, a path reference, or
/// a numeric arithmetic combination of the above.
#[derive(Debug, Clone)]
pub(crate) enum FilterSide {
    Literal(serde_json::Value),
    Path(Vec<String>),
    Arith(Box<FilterSide>, ArithOp, Box<FilterSide>),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

pub(crate) fn parse_jsonpath(path: &str) -> Result<Vec<PathStep>, DbError> {
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
                    if key.is_empty() {
                        // Nothing between dots — skip.
                    } else if chars.peek() == Some(&'(') {
                        // `.size()` / `.type()` accessor.
                        chars.next();
                        if chars.peek() != Some(&')') {
                            return Err(DbError::InvalidValue {
                                reason: format!("accessor `.{key}(` must close with `)`: {path}"),
                            });
                        }
                        chars.next();
                        match key.as_str() {
                            "size" => steps.push(PathStep::Size),
                            "type" => steps.push(PathStep::TypeOf),
                            other => {
                                return Err(DbError::InvalidValue {
                                    reason: format!(
                                        "unsupported accessor `.{other}()` in path: {path}"
                                    ),
                                });
                            }
                        }
                    } else {
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
        if c == '.' || c == '[' || c == ']' || c == '(' {
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

    let mut fp = FilterParser {
        bytes: inner.as_bytes(),
        pos: 0,
    };
    let expr = fp.parse_or()?;
    fp.skip_ws();
    if fp.pos != fp.bytes.len() {
        return Err(DbError::InvalidValue {
            reason: format!(
                "unexpected trailing input in JSONPath filter at offset {}: {}",
                fp.pos, inner
            ),
        });
    }
    Ok(expr)
}

struct FilterParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl FilterParser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && (self.bytes[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, tok: &str) -> bool {
        self.skip_ws();
        let t = tok.as_bytes();
        if self.pos + t.len() <= self.bytes.len() && &self.bytes[self.pos..self.pos + t.len()] == t
        {
            self.pos += t.len();
            return true;
        }
        false
    }

    fn parse_or(&mut self) -> Result<FilterExpr, DbError> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.eat("||") {
                let right = self.parse_and()?;
                left = FilterExpr::Or(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_and(&mut self) -> Result<FilterExpr, DbError> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            if self.eat("&&") {
                let right = self.parse_unary()?;
                left = FilterExpr::And(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_unary(&mut self) -> Result<FilterExpr, DbError> {
        self.skip_ws();
        if self.eat("!") {
            let inner = self.parse_unary()?;
            return Ok(FilterExpr::Not(Box::new(inner)));
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Result<FilterExpr, DbError> {
        self.skip_ws();
        if self.eat("(") {
            let e = self.parse_or()?;
            self.skip_ws();
            if !self.eat(")") {
                return Err(DbError::InvalidValue {
                    reason: "missing ) in JSONPath filter".into(),
                });
            }
            return Ok(e);
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<FilterExpr, DbError> {
        // Slice from current position up to the next boolean combinator or
        // the matching close paren. Within that slice, parse LHS arith,
        // optional CmpOp, and RHS arith via SidesParser.
        self.skip_ws();
        let src = std::str::from_utf8(&self.bytes[self.pos..]).unwrap_or("");
        let end = src.find(['&', '|', ')']).unwrap_or(src.len());
        let (atom_str, _tail) = src.split_at(end);
        let atom_str = atom_str.trim_end();
        self.pos += atom_str.len();

        // Locate the comparison operator outside of any path identifier.
        // Operators considered: == != <= >= < > =. Search left-to-right.
        let mut op_pos: Option<(usize, usize, CmpOp)> = None;
        let bytes = atom_str.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let two = if i + 2 <= bytes.len() {
                Some(&bytes[i..i + 2])
            } else {
                None
            };
            if two == Some(b"==") {
                op_pos = Some((i, 2, CmpOp::Eq));
                break;
            }
            if two == Some(b"!=") {
                op_pos = Some((i, 2, CmpOp::Ne));
                break;
            }
            if two == Some(b"<=") {
                op_pos = Some((i, 2, CmpOp::Le));
                break;
            }
            if two == Some(b">=") {
                op_pos = Some((i, 2, CmpOp::Ge));
                break;
            }
            let one = bytes[i];
            if one == b'<' {
                op_pos = Some((i, 1, CmpOp::Lt));
                break;
            }
            if one == b'>' {
                op_pos = Some((i, 1, CmpOp::Gt));
                break;
            }
            if one == b'=' {
                op_pos = Some((i, 1, CmpOp::Eq));
                break;
            }
            i += 1;
        }

        if let Some((idx, op_len, op)) = op_pos {
            let lhs_str = atom_str[..idx].trim();
            let rhs_str = atom_str[idx + op_len..].trim();
            let lhs = parse_filter_side_str(lhs_str)?;
            let rhs = parse_filter_side_str(rhs_str)?;
            return Ok(FilterExpr::Compare { lhs, op, rhs });
        }

        // No comparison operator → existence atom (must be `@.path…`).
        let rest = atom_str
            .strip_prefix('@')
            .ok_or_else(|| DbError::InvalidValue {
                reason: format!("JSONPath filter atom must start with '@': {atom_str}"),
            })?;
        let mut path = Vec::new();
        let mut r = rest;
        while let Some(s2) = r.strip_prefix('.') {
            let stop = s2
                .find(|c: char| c.is_whitespace() || c == '.')
                .unwrap_or(s2.len());
            let key = &s2[..stop];
            if !key.is_empty() {
                path.push(key.to_string());
            }
            r = &s2[stop..];
            if r.is_empty() || r.starts_with(char::is_whitespace) {
                break;
            }
        }
        Ok(FilterExpr::Exists(path))
    }
}

/// Parse a filter side string (LHS or RHS of a comparison) as a
/// `FilterSide` arithmetic expression. Trailing-input check enforces
/// no slop.
fn parse_filter_side_str(s: &str) -> Result<FilterSide, DbError> {
    let mut sp = SidesParser {
        bytes: s.as_bytes(),
        pos: 0,
    };
    let side = sp.parse_addsub()?;
    sp.skip_ws();
    if sp.pos != sp.bytes.len() {
        return Err(DbError::InvalidValue {
            reason: format!("trailing input in filter side: {s}"),
        });
    }
    Ok(side)
}

/// Pratt-style parser for filter RHS arithmetic. Operator precedence:
/// `* / %` binds tighter than `+ -`. No parentheses (filters that need
/// them can lift the work out into multiple `&&`-combined comparisons).
struct SidesParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl SidesParser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && (self.bytes[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }

    fn parse_addsub(&mut self) -> Result<FilterSide, DbError> {
        let mut left = self.parse_muldiv()?;
        loop {
            self.skip_ws();
            let c = self.bytes.get(self.pos).copied();
            let op = match c {
                Some(b'+') => ArithOp::Add,
                Some(b'-') => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.parse_muldiv()?;
            left = FilterSide::Arith(Box::new(left), op, Box::new(right));
        }
    }

    fn parse_muldiv(&mut self) -> Result<FilterSide, DbError> {
        let mut left = self.parse_atom()?;
        loop {
            self.skip_ws();
            let c = self.bytes.get(self.pos).copied();
            let op = match c {
                Some(b'*') => ArithOp::Mul,
                Some(b'/') => ArithOp::Div,
                Some(b'%') => ArithOp::Mod,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.parse_atom()?;
            left = FilterSide::Arith(Box::new(left), op, Box::new(right));
        }
    }

    fn parse_atom(&mut self) -> Result<FilterSide, DbError> {
        self.skip_ws();
        let src = std::str::from_utf8(&self.bytes[self.pos..]).unwrap_or("");
        if let Some(rest_after_at) = src.strip_prefix('@') {
            // `@.k.k…` path reference — terminate at whitespace, arith op,
            // or end of RHS.
            let mut rpath = Vec::<String>::new();
            let mut r = rest_after_at;
            let mut consumed = 1usize; // for '@'
            while let Some(s2) = r.strip_prefix('.') {
                consumed += 1;
                let stop = s2
                    .find(|c: char| {
                        c.is_whitespace()
                            || c == '.'
                            || c == '+'
                            || c == '-'
                            || c == '*'
                            || c == '/'
                            || c == '%'
                    })
                    .unwrap_or(s2.len());
                let key = &s2[..stop];
                if !key.is_empty() {
                    rpath.push(key.to_string());
                }
                consumed += stop;
                r = &s2[stop..];
                if r.is_empty()
                    || r.starts_with(char::is_whitespace)
                    || r.starts_with(['+', '-', '*', '/', '%'])
                {
                    break;
                }
            }
            self.pos += consumed;
            return Ok(FilterSide::Path(rpath));
        }
        // Literal: scan up to the next arithmetic operator or whitespace.
        let stop = src
            .find(|c: char| c.is_whitespace() || matches!(c, '+' | '-' | '*' | '/' | '%'))
            .unwrap_or(src.len());
        // Allow leading minus sign for negative numeric literals.
        let stop = if stop == 0 && src.starts_with('-') {
            // Pick up `-N` as a literal.
            let r = &src[1..];
            let n = r
                .find(|c: char| c.is_whitespace() || matches!(c, '+' | '-' | '*' | '/' | '%'))
                .unwrap_or(r.len());
            n + 1
        } else {
            stop
        };
        if stop == 0 {
            return Err(DbError::InvalidValue {
                reason: format!("expected literal or @path in filter RHS: {src}"),
            });
        }
        let lit = &src[..stop];
        self.pos += stop;
        Ok(FilterSide::Literal(parse_jsonpath_literal(lit)?))
    }
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

/// Accessor-aware variant of [`execute_jsonpath`]. If the path ends with
/// `.size()` / `.type()` the trailing step is stripped before walking and
/// applied to each borrowed result, returning owned values.
pub(crate) fn execute_jsonpath_owned(
    root: &serde_json::Value,
    steps: &[PathStep],
) -> Vec<serde_json::Value> {
    let (body, accessor) = split_trailing_accessor(steps);
    let refs = execute_jsonpath(root, body);
    match accessor {
        None => refs.into_iter().cloned().collect(),
        Some(PathStep::Size) => refs
            .into_iter()
            .map(|v| match v {
                serde_json::Value::Array(a) => serde_json::json!(a.len()),
                _ => serde_json::json!(1),
            })
            .collect(),
        Some(PathStep::TypeOf) => refs
            .into_iter()
            .map(|v| serde_json::Value::String(json_node_type_name(v).into()))
            .collect(),
        Some(_) => unreachable!("split_trailing_accessor returns Size or TypeOf"),
    }
}

fn split_trailing_accessor(steps: &[PathStep]) -> (&[PathStep], Option<&PathStep>) {
    if let Some(last) = steps.last() {
        if matches!(last, PathStep::Size | PathStep::TypeOf) {
            return (&steps[..steps.len() - 1], Some(last));
        }
    }
    (steps, None)
}

pub(crate) fn execute_jsonpath<'a>(
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

        // Accessors are terminal and only applied in `execute_jsonpath_owned`.
        // If they appear in the mid-path through the ref-based walker, treat
        // as an empty match (caller should route through the owned variant).
        PathStep::Size | PathStep::TypeOf => vec![],
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
        FilterExpr::Compare { lhs, op, rhs } => {
            match (
                resolve_filter_side(lhs, node),
                resolve_filter_side(rhs, node),
            ) {
                (Some(l), Some(r)) => compare_json(&l, *op, &r),
                _ => false,
            }
        }
        FilterExpr::And(a, b) => eval_filter(node, a) && eval_filter(node, b),
        FilterExpr::Or(a, b) => eval_filter(node, a) || eval_filter(node, b),
        FilterExpr::Not(inner) => !eval_filter(node, inner),
    }
}

fn resolve_filter_side(side: &FilterSide, node: &serde_json::Value) -> Option<serde_json::Value> {
    match side {
        FilterSide::Literal(v) => Some(v.clone()),
        FilterSide::Path(p) => {
            let mut current = node;
            for k in p {
                current = current.get(k.as_str())?;
            }
            Some(current.clone())
        }
        FilterSide::Arith(lhs, op, rhs) => {
            let l = resolve_filter_side(lhs, node)?.as_f64()?;
            let r = resolve_filter_side(rhs, node)?.as_f64()?;
            let out = match op {
                ArithOp::Add => l + r,
                ArithOp::Sub => l - r,
                ArithOp::Mul => l * r,
                ArithOp::Div => {
                    if r == 0.0 {
                        return None;
                    }
                    l / r
                }
                ArithOp::Mod => {
                    if r == 0.0 {
                        return None;
                    }
                    l % r
                }
            };
            Some(serde_json::json!(out))
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

// ── Phase 11.23a: JSON Schema Draft-07 subset validator ─────────────────────-
//
// Supported keywords: type, enum, const, required, properties,
// additionalProperties (bool), items (object or array of schemas),
// minimum, maximum, exclusiveMinimum, exclusiveMaximum,
// minLength, maxLength, minItems, maxItems,
// multipleOf.
// `true` schema accepts all; `false` schema rejects all.
fn json_schema_validate(schema: &serde_json::Value, doc: &serde_json::Value) -> bool {
    validate_with_root(schema, doc, schema, 0)
}

const SCHEMA_REF_DEPTH_LIMIT: u32 = 32;

fn validate_with_root(
    schema: &serde_json::Value,
    doc: &serde_json::Value,
    root: &serde_json::Value,
    depth: u32,
) -> bool {
    if depth > SCHEMA_REF_DEPTH_LIMIT {
        return false;
    }
    match schema {
        serde_json::Value::Bool(true) => return true,
        serde_json::Value::Bool(false) => return false,
        serde_json::Value::Object(_) => {}
        _ => return false,
    }
    let obj = schema.as_object().unwrap();

    // $ref short-circuits per Draft-07: all sibling keywords are ignored.
    if let Some(r) = obj.get("$ref").and_then(|v| v.as_str()) {
        return match resolve_json_pointer(root, r) {
            Some(target) => validate_with_root(target, doc, root, depth + 1),
            None => false,
        };
    }

    if let Some(subs) = obj.get("allOf").and_then(|v| v.as_array()) {
        if !subs
            .iter()
            .all(|s| validate_with_root(s, doc, root, depth + 1))
        {
            return false;
        }
    }
    if let Some(subs) = obj.get("anyOf").and_then(|v| v.as_array()) {
        if !subs
            .iter()
            .any(|s| validate_with_root(s, doc, root, depth + 1))
        {
            return false;
        }
    }
    if let Some(subs) = obj.get("oneOf").and_then(|v| v.as_array()) {
        let hits = subs
            .iter()
            .filter(|s| validate_with_root(s, doc, root, depth + 1))
            .count();
        if hits != 1 {
            return false;
        }
    }
    if let Some(sub) = obj.get("not") {
        if validate_with_root(sub, doc, root, depth + 1) {
            return false;
        }
    }

    if let Some(t) = obj.get("type") {
        if !schema_type_matches(t, doc) {
            return false;
        }
    }
    if let Some(e) = obj.get("enum").and_then(|v| v.as_array()) {
        if !e.iter().any(|x| x == doc) {
            return false;
        }
    }
    if let Some(c) = obj.get("const") {
        if c != doc {
            return false;
        }
    }
    if let Some(n) = doc.as_f64() {
        if let Some(m) = obj.get("minimum").and_then(|v| v.as_f64()) {
            if n < m {
                return false;
            }
        }
        if let Some(m) = obj.get("maximum").and_then(|v| v.as_f64()) {
            if n > m {
                return false;
            }
        }
        if let Some(m) = obj.get("exclusiveMinimum").and_then(|v| v.as_f64()) {
            if n <= m {
                return false;
            }
        }
        if let Some(m) = obj.get("exclusiveMaximum").and_then(|v| v.as_f64()) {
            if n >= m {
                return false;
            }
        }
        if let Some(m) = obj.get("multipleOf").and_then(|v| v.as_f64()) {
            if m == 0.0 || (n / m).fract() != 0.0 {
                return false;
            }
        }
    }
    if let Some(s) = doc.as_str() {
        let len = s.chars().count() as u64;
        if let Some(m) = obj.get("minLength").and_then(|v| v.as_u64()) {
            if len < m {
                return false;
            }
        }
        if let Some(m) = obj.get("maxLength").and_then(|v| v.as_u64()) {
            if len > m {
                return false;
            }
        }
        if let Some(p) = obj.get("pattern").and_then(|v| v.as_str()) {
            match regex::Regex::new(p) {
                Ok(re) => {
                    if !re.is_match(s) {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
        if let Some(fmt) = obj.get("format").and_then(|v| v.as_str()) {
            if !schema_format_matches(fmt, s) {
                return false;
            }
        }
    }
    if let Some(arr) = doc.as_array() {
        if let Some(m) = obj.get("minItems").and_then(|v| v.as_u64()) {
            if (arr.len() as u64) < m {
                return false;
            }
        }
        if let Some(m) = obj.get("maxItems").and_then(|v| v.as_u64()) {
            if (arr.len() as u64) > m {
                return false;
            }
        }
        if obj.get("uniqueItems").and_then(|v| v.as_bool()) == Some(true) {
            for i in 0..arr.len() {
                for j in (i + 1)..arr.len() {
                    if arr[i] == arr[j] {
                        return false;
                    }
                }
            }
        }
        if let Some(items) = obj.get("items") {
            match items {
                serde_json::Value::Array(subs) => {
                    for (i, sub) in subs.iter().enumerate() {
                        if let Some(v) = arr.get(i) {
                            if !validate_with_root(sub, v, root, depth + 1) {
                                return false;
                            }
                        }
                    }
                }
                _ => {
                    for v in arr {
                        if !validate_with_root(items, v, root, depth + 1) {
                            return false;
                        }
                    }
                }
            }
        }
    }
    if let Some(map) = doc.as_object() {
        if let Some(req) = obj.get("required").and_then(|v| v.as_array()) {
            for r in req {
                if let Some(k) = r.as_str() {
                    if !map.contains_key(k) {
                        return false;
                    }
                }
            }
        }
        let empty_props = serde_json::Map::new();
        let props = obj
            .get("properties")
            .and_then(|v| v.as_object())
            .unwrap_or(&empty_props);
        let pattern_props = obj
            .get("patternProperties")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let compiled_pp: Vec<(regex::Regex, serde_json::Value)> = pattern_props
            .iter()
            .filter_map(|(k, v)| regex::Regex::new(k).ok().map(|re| (re, v.clone())))
            .collect();

        for (k, sub) in props {
            if let Some(v) = map.get(k) {
                if !validate_with_root(sub, v, root, depth + 1) {
                    return false;
                }
            }
        }
        for (k, v) in map {
            for (re, sub) in &compiled_pp {
                if re.is_match(k) && !validate_with_root(sub, v, root, depth + 1) {
                    return false;
                }
            }
        }
        if let Some(ap) = obj.get("additionalProperties") {
            let is_additional = |k: &str| {
                !props.contains_key(k) && !compiled_pp.iter().any(|(re, _)| re.is_match(k))
            };
            match ap {
                serde_json::Value::Bool(false) => {
                    for k in map.keys() {
                        if is_additional(k) {
                            return false;
                        }
                    }
                }
                serde_json::Value::Object(_) | serde_json::Value::Bool(true) => {
                    for (k, v) in map {
                        if is_additional(k) && !validate_with_root(ap, v, root, depth + 1) {
                            return false;
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(pn) = obj.get("propertyNames") {
            for k in map.keys() {
                let kv = serde_json::Value::String(k.clone());
                if !validate_with_root(pn, &kv, root, depth + 1) {
                    return false;
                }
            }
        }
        if let Some(deps) = obj.get("dependencies").and_then(|v| v.as_object()) {
            for (k, dep) in deps {
                if !map.contains_key(k) {
                    continue;
                }
                match dep {
                    serde_json::Value::Array(keys) => {
                        for r in keys {
                            if let Some(rk) = r.as_str() {
                                if !map.contains_key(rk) {
                                    return false;
                                }
                            }
                        }
                    }
                    _ => {
                        if !validate_with_root(dep, doc, root, depth + 1) {
                            return false;
                        }
                    }
                }
            }
        }
    }

    if let Some(cond) = obj.get("if") {
        let branch_key = if validate_with_root(cond, doc, root, depth + 1) {
            "then"
        } else {
            "else"
        };
        if let Some(branch) = obj.get(branch_key) {
            if !validate_with_root(branch, doc, root, depth + 1) {
                return false;
            }
        }
    }

    true
}

fn push_error(
    errors: &mut Vec<serde_json::Value>,
    path: &str,
    keyword: &str,
    message: impl Into<String>,
) {
    errors.push(serde_json::json!({
        "path": path,
        "keyword": keyword,
        "message": message.into(),
    }));
}

fn collect_schema_errors(
    schema: &serde_json::Value,
    doc: &serde_json::Value,
    root: &serde_json::Value,
    depth: u32,
    path: String,
    errors: &mut Vec<serde_json::Value>,
) {
    if depth > SCHEMA_REF_DEPTH_LIMIT {
        push_error(errors, &path, "$ref", "max ref depth exceeded");
        return;
    }
    match schema {
        serde_json::Value::Bool(true) => return,
        serde_json::Value::Bool(false) => {
            push_error(errors, &path, "false", "schema `false` rejects all");
            return;
        }
        serde_json::Value::Object(_) => {}
        _ => {
            push_error(errors, &path, "schema", "schema must be object or bool");
            return;
        }
    }
    let obj = schema.as_object().unwrap();

    if let Some(r) = obj.get("$ref").and_then(|v| v.as_str()) {
        match resolve_json_pointer(root, r) {
            Some(target) => collect_schema_errors(target, doc, root, depth + 1, path, errors),
            None => push_error(errors, &path, "$ref", format!("cannot resolve {r}")),
        }
        return;
    }

    // Logical combinators use the boolean validator to avoid noisy duplicate
    // errors inside anyOf/oneOf; allOf reports each branch's errors inline.
    if let Some(subs) = obj.get("allOf").and_then(|v| v.as_array()) {
        for (i, sub) in subs.iter().enumerate() {
            if !validate_with_root(sub, doc, root, depth + 1) {
                let sub_path = format!("{path}/allOf/{i}");
                collect_schema_errors(sub, doc, root, depth + 1, sub_path, errors);
            }
        }
    }
    if let Some(subs) = obj.get("anyOf").and_then(|v| v.as_array()) {
        if !subs
            .iter()
            .any(|s| validate_with_root(s, doc, root, depth + 1))
        {
            push_error(errors, &path, "anyOf", "value matched none of the schemas");
        }
    }
    if let Some(subs) = obj.get("oneOf").and_then(|v| v.as_array()) {
        let hits = subs
            .iter()
            .filter(|s| validate_with_root(s, doc, root, depth + 1))
            .count();
        if hits != 1 {
            push_error(
                errors,
                &path,
                "oneOf",
                format!("value matched {hits} schemas, expected exactly 1"),
            );
        }
    }
    if let Some(sub) = obj.get("not") {
        if validate_with_root(sub, doc, root, depth + 1) {
            push_error(errors, &path, "not", "value matched the `not` schema");
        }
    }

    if let Some(t) = obj.get("type") {
        if !schema_type_matches(t, doc) {
            push_error(
                errors,
                &path,
                "type",
                format!("value does not match type {t}"),
            );
        }
    }
    if let Some(e) = obj.get("enum").and_then(|v| v.as_array()) {
        if !e.iter().any(|x| x == doc) {
            push_error(errors, &path, "enum", "value not in enum");
        }
    }
    if let Some(c) = obj.get("const") {
        if c != doc {
            push_error(errors, &path, "const", "value does not equal const");
        }
    }
    if let Some(n) = doc.as_f64() {
        if let Some(m) = obj.get("minimum").and_then(|v| v.as_f64()) {
            if n < m {
                push_error(errors, &path, "minimum", format!("{n} < {m}"));
            }
        }
        if let Some(m) = obj.get("maximum").and_then(|v| v.as_f64()) {
            if n > m {
                push_error(errors, &path, "maximum", format!("{n} > {m}"));
            }
        }
        if let Some(m) = obj.get("exclusiveMinimum").and_then(|v| v.as_f64()) {
            if n <= m {
                push_error(errors, &path, "exclusiveMinimum", format!("{n} <= {m}"));
            }
        }
        if let Some(m) = obj.get("exclusiveMaximum").and_then(|v| v.as_f64()) {
            if n >= m {
                push_error(errors, &path, "exclusiveMaximum", format!("{n} >= {m}"));
            }
        }
        if let Some(m) = obj.get("multipleOf").and_then(|v| v.as_f64()) {
            if m == 0.0 || (n / m).fract() != 0.0 {
                push_error(
                    errors,
                    &path,
                    "multipleOf",
                    format!("{n} not multiple of {m}"),
                );
            }
        }
    }
    if let Some(s) = doc.as_str() {
        let len = s.chars().count() as u64;
        if let Some(m) = obj.get("minLength").and_then(|v| v.as_u64()) {
            if len < m {
                push_error(errors, &path, "minLength", format!("length {len} < {m}"));
            }
        }
        if let Some(m) = obj.get("maxLength").and_then(|v| v.as_u64()) {
            if len > m {
                push_error(errors, &path, "maxLength", format!("length {len} > {m}"));
            }
        }
        if let Some(p) = obj.get("pattern").and_then(|v| v.as_str()) {
            match regex::Regex::new(p) {
                Ok(re) => {
                    if !re.is_match(s) {
                        push_error(errors, &path, "pattern", format!("does not match /{p}/"));
                    }
                }
                Err(e) => push_error(errors, &path, "pattern", format!("invalid regex: {e}")),
            }
        }
        if let Some(fmt) = obj.get("format").and_then(|v| v.as_str()) {
            if !schema_format_matches(fmt, s) {
                push_error(errors, &path, "format", format!("not a valid {fmt}"));
            }
        }
    }
    if let Some(arr) = doc.as_array() {
        if let Some(m) = obj.get("minItems").and_then(|v| v.as_u64()) {
            if (arr.len() as u64) < m {
                push_error(errors, &path, "minItems", format!("{} < {m}", arr.len()));
            }
        }
        if let Some(m) = obj.get("maxItems").and_then(|v| v.as_u64()) {
            if (arr.len() as u64) > m {
                push_error(errors, &path, "maxItems", format!("{} > {m}", arr.len()));
            }
        }
        if obj.get("uniqueItems").and_then(|v| v.as_bool()) == Some(true) {
            'outer: for i in 0..arr.len() {
                for j in (i + 1)..arr.len() {
                    if arr[i] == arr[j] {
                        push_error(
                            errors,
                            &path,
                            "uniqueItems",
                            format!("duplicate at indices {i} and {j}"),
                        );
                        break 'outer;
                    }
                }
            }
        }
        if let Some(items) = obj.get("items") {
            match items {
                serde_json::Value::Array(subs) => {
                    for (i, sub) in subs.iter().enumerate() {
                        if let Some(v) = arr.get(i) {
                            let sub_path = format!("{path}/{i}");
                            collect_schema_errors(sub, v, root, depth + 1, sub_path, errors);
                        }
                    }
                }
                _ => {
                    for (i, v) in arr.iter().enumerate() {
                        let sub_path = format!("{path}/{i}");
                        collect_schema_errors(items, v, root, depth + 1, sub_path, errors);
                    }
                }
            }
        }
    }
    if let Some(map) = doc.as_object() {
        if let Some(req) = obj.get("required").and_then(|v| v.as_array()) {
            for r in req {
                if let Some(k) = r.as_str() {
                    if !map.contains_key(k) {
                        push_error(errors, &path, "required", format!("missing key `{k}`"));
                    }
                }
            }
        }
        let empty_props = serde_json::Map::new();
        let props = obj
            .get("properties")
            .and_then(|v| v.as_object())
            .unwrap_or(&empty_props);
        let pattern_props = obj
            .get("patternProperties")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let compiled_pp: Vec<(regex::Regex, serde_json::Value)> = pattern_props
            .iter()
            .filter_map(|(k, v)| regex::Regex::new(k).ok().map(|re| (re, v.clone())))
            .collect();

        for (k, sub) in props {
            if let Some(v) = map.get(k) {
                let sub_path = format!("{path}/{k}");
                collect_schema_errors(sub, v, root, depth + 1, sub_path, errors);
            }
        }
        for (k, v) in map {
            for (re, sub) in &compiled_pp {
                if re.is_match(k) {
                    let sub_path = format!("{path}/{k}");
                    collect_schema_errors(sub, v, root, depth + 1, sub_path, errors);
                }
            }
        }
        if let Some(ap) = obj.get("additionalProperties") {
            let is_additional = |k: &str| {
                !props.contains_key(k) && !compiled_pp.iter().any(|(re, _)| re.is_match(k))
            };
            match ap {
                serde_json::Value::Bool(false) => {
                    for k in map.keys() {
                        if is_additional(k) {
                            push_error(
                                errors,
                                &path,
                                "additionalProperties",
                                format!("extra key `{k}` not allowed"),
                            );
                        }
                    }
                }
                serde_json::Value::Object(_) | serde_json::Value::Bool(true) => {
                    for (k, v) in map {
                        if is_additional(k) {
                            let sub_path = format!("{path}/{k}");
                            collect_schema_errors(ap, v, root, depth + 1, sub_path, errors);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(pn) = obj.get("propertyNames") {
            for k in map.keys() {
                let kv = serde_json::Value::String(k.clone());
                if !validate_with_root(pn, &kv, root, depth + 1) {
                    push_error(
                        errors,
                        &path,
                        "propertyNames",
                        format!("key `{k}` does not satisfy propertyNames"),
                    );
                }
            }
        }
        if let Some(deps) = obj.get("dependencies").and_then(|v| v.as_object()) {
            for (k, dep) in deps {
                if !map.contains_key(k) {
                    continue;
                }
                match dep {
                    serde_json::Value::Array(keys) => {
                        for r in keys {
                            if let Some(rk) = r.as_str() {
                                if !map.contains_key(rk) {
                                    push_error(
                                        errors,
                                        &path,
                                        "dependencies",
                                        format!("`{k}` requires `{rk}`"),
                                    );
                                }
                            }
                        }
                    }
                    _ => {
                        if !validate_with_root(dep, doc, root, depth + 1) {
                            push_error(
                                errors,
                                &path,
                                "dependencies",
                                format!("`{k}` schema-dependency failed"),
                            );
                        }
                    }
                }
            }
        }
    }

    if let Some(cond) = obj.get("if") {
        let branch_key = if validate_with_root(cond, doc, root, depth + 1) {
            "then"
        } else {
            "else"
        };
        if let Some(branch) = obj.get(branch_key) {
            if !validate_with_root(branch, doc, root, depth + 1) {
                let sub_path = format!("{path}/{branch_key}");
                collect_schema_errors(branch, doc, root, depth + 1, sub_path, errors);
            }
        }
    }
}

/// Resolve a RFC 6901 JSON pointer (with optional leading `#`) against a root.
fn resolve_json_pointer<'a>(
    root: &'a serde_json::Value,
    pointer: &str,
) -> Option<&'a serde_json::Value> {
    let p = pointer.strip_prefix('#').unwrap_or(pointer);
    if p.is_empty() || p == "/" {
        // "#" and "#/" both mean the root document.
        return Some(root);
    }
    let p = p.strip_prefix('/')?;
    let mut current = root;
    for raw in p.split('/') {
        let decoded = raw.replace("~1", "/").replace("~0", "~");
        match current {
            serde_json::Value::Object(map) => {
                current = map.get(&decoded)?;
            }
            serde_json::Value::Array(arr) => {
                let idx: usize = decoded.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn schema_format_matches(fmt: &str, s: &str) -> bool {
    match fmt {
        "email" => regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$")
            .map(|re| re.is_match(s))
            .unwrap_or(false),
        "uuid" => regex::Regex::new(
            r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
        )
        .map(|re| re.is_match(s))
        .unwrap_or(false),
        "date" => regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$")
            .map(|re| re.is_match(s))
            .unwrap_or(false),
        "date-time" | "datetime" => regex::Regex::new(
            r"^\d{4}-\d{2}-\d{2}[Tt ]\d{2}:\d{2}:\d{2}(\.\d+)?([Zz]|[+-]\d{2}:\d{2})?$",
        )
        .map(|re| re.is_match(s))
        .unwrap_or(false),
        "time" => regex::Regex::new(r"^\d{2}:\d{2}:\d{2}(\.\d+)?$")
            .map(|re| re.is_match(s))
            .unwrap_or(false),
        "ipv4" => s.parse::<std::net::Ipv4Addr>().is_ok(),
        "ipv6" => s.parse::<std::net::Ipv6Addr>().is_ok(),
        "uri" => s.contains("://") && !s.contains(' '),
        "regex" => regex::Regex::new(s).is_ok(),
        // Unknown format → validation success (Draft-07 default: formats are annotations).
        _ => true,
    }
}

fn schema_type_matches(t: &serde_json::Value, doc: &serde_json::Value) -> bool {
    match t {
        serde_json::Value::String(s) => schema_type_single(s, doc),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .any(|s| schema_type_single(s, doc)),
        _ => false,
    }
}

fn schema_type_single(name: &str, doc: &serde_json::Value) -> bool {
    match name {
        "string" => doc.is_string(),
        "number" => doc.is_number(),
        "integer" => doc.is_i64() || doc.is_u64() || doc.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => doc.is_boolean(),
        "null" => doc.is_null(),
        "array" => doc.is_array(),
        "object" => doc.is_object(),
        _ => false,
    }
}

// ── Phase 11.25a helpers: MySQL JSON_ARRAY_APPEND / INSERT ───────────────────

fn descend_mut<'a>(
    root: &'a mut serde_json::Value,
    parts: &[String],
) -> Option<&'a mut serde_json::Value> {
    let mut cur = root;
    for p in parts {
        cur = match cur {
            serde_json::Value::Object(m) => m.get_mut(p)?,
            serde_json::Value::Array(a) => {
                let idx: usize = p.parse().ok()?;
                a.get_mut(idx)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn json_array_append_at(root: &mut serde_json::Value, parts: &[String], val: serde_json::Value) {
    let Some(target) = descend_mut(root, parts) else {
        return;
    };
    match target {
        serde_json::Value::Array(a) => a.push(val),
        other => {
            let taken = std::mem::replace(other, serde_json::Value::Null);
            *other = serde_json::Value::Array(vec![taken, val]);
        }
    }
}

fn json_node_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn json_dataguide_walk(node: &serde_json::Value, path: &str, out: &mut Vec<serde_json::Value>) {
    out.push(serde_json::json!({
        "path": path,
        "type": json_node_type_name(node),
    }));
    match node {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let sub = format!("{path}.{k}");
                json_dataguide_walk(v, &sub, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                let sub = format!("{path}[{i}]");
                json_dataguide_walk(v, &sub, out);
            }
        }
        _ => {}
    }
}

/// LIKE-style pattern match: `%` any-length, `_` single char.
fn like_match(pat: &str, text: &str) -> bool {
    let pat: Vec<char> = pat.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    fn rec(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some((&'%', rest)) => {
                if rec(rest, t) {
                    return true;
                }
                for i in 0..t.len() {
                    if rec(rest, &t[i + 1..]) {
                        return true;
                    }
                }
                false
            }
            Some((&'_', rest)) => !t.is_empty() && rec(rest, &t[1..]),
            Some((&c, rest)) => match t.split_first() {
                Some((&tc, trest)) if tc == c => rec(rest, trest),
                _ => false,
            },
        }
    }
    rec(&pat, &txt)
}

fn json_search_walk(
    node: &serde_json::Value,
    path: &str,
    pattern: &str,
    hits: &mut Vec<String>,
    stop_at_one: bool,
) {
    if stop_at_one && !hits.is_empty() {
        return;
    }
    match node {
        serde_json::Value::String(s) => {
            if like_match(pattern, s) {
                hits.push(path.to_string());
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                let sub = format!("{path}[{i}]");
                json_search_walk(v, &sub, pattern, hits, stop_at_one);
                if stop_at_one && !hits.is_empty() {
                    return;
                }
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let sub = format!("{path}.{k}");
                json_search_walk(v, &sub, pattern, hits, stop_at_one);
                if stop_at_one && !hits.is_empty() {
                    return;
                }
            }
        }
        _ => {}
    }
}

/// MySQL JSON_MERGE_PRESERVE semantics: arrays concatenate; objects key-merge
/// recursively (on conflict, recursive preserve — values wrap into an array);
/// other type mismatches promote both to an array.
fn merge_preserve(a: serde_json::Value, b: serde_json::Value) -> serde_json::Value {
    use serde_json::Value as J;
    match (a, b) {
        (J::Array(mut xa), J::Array(mut xb)) => {
            xa.append(&mut xb);
            J::Array(xa)
        }
        (J::Array(mut xa), other) => {
            xa.push(other);
            J::Array(xa)
        }
        (other, J::Array(mut xb)) => {
            let mut out = vec![other];
            out.append(&mut xb);
            J::Array(out)
        }
        (J::Object(mut ma), J::Object(mb)) => {
            for (k, v) in mb {
                match ma.remove(&k) {
                    Some(prev) => {
                        ma.insert(k, merge_preserve(prev, v));
                    }
                    None => {
                        ma.insert(k, v);
                    }
                }
            }
            J::Object(ma)
        }
        (a, b) => J::Array(vec![a, b]),
    }
}

/// Rename an object key identified by `parts`. The last component must name
/// an object key; renames it to `new_name` in-place, preserving value. No-op
/// if the path doesn't resolve to an object key.
fn json_rename_at(root: &mut serde_json::Value, parts: &[String], new_name: &str) {
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let Some(target) = descend_mut(root, parents) else {
        return;
    };
    if let serde_json::Value::Object(map) = target {
        if let Some(v) = map.remove(last) {
            map.insert(new_name.to_string(), v);
        }
    }
}

fn json_array_insert_at(
    root: &mut serde_json::Value,
    parent_parts: &[String],
    idx: usize,
    val: serde_json::Value,
) {
    let Some(target) = descend_mut(root, parent_parts) else {
        return;
    };
    if let serde_json::Value::Array(a) = target {
        let at = idx.min(a.len());
        a.insert(at, val);
    }
}
