//! Phase 11.19a — SQL:2016 `JSON_VALUE` / `JSON_QUERY` / `JSON_EXISTS`
//! evaluator.
//!
//! Cross-engine semantic reference:
//! - PostgreSQL `src/backend/utils/adt/jsonpath_exec.c` (strict/lax,
//!   empty/error detection) and `execExprInterp.c:4977-5030` (behavior
//!   dispatch).
//! - MariaDB `sql/item_jsonfunc.cc` (barebones JSON_VALUE / JSON_QUERY
//!   — no clause support, so we only borrow path conventions).

use axiomdb_core::error::DbError;
use axiomdb_types::{
    coerce::{coerce, CoercionMode},
    DataType, Value,
};

use crate::eval::SubqueryRunner;
use crate::expr::{
    Expr, SqlJsonOnBehavior, SqlJsonPathMode, SqlJsonQueryKind, SqlJsonQuotes, SqlJsonWrapper,
};

/// Entry point called from `eval::core::eval{,_with}` when the AST node is
/// [`Expr::SqlJsonQuery`]. Applies strict/lax path semantics, dispatches
/// the `ON EMPTY` / `ON ERROR` handlers, and coerces to the `RETURNING`
/// type when present.
pub(crate) fn eval_sql_json_query<R: SubqueryRunner>(
    expr: &Expr,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    let Expr::SqlJsonQuery {
        kind,
        doc,
        path,
        path_mode,
        passing,
        returning,
        wrapper,
        quotes,
        on_empty,
        on_error,
    } = expr
    else {
        return Err(DbError::Internal {
            message: "eval_sql_json_query called on non-SqlJsonQuery expr".into(),
        });
    };

    // Doc evaluation — NULL propagates through all three functions.
    let doc_val = crate::eval::eval_with(doc, row, sq)?;
    if matches!(doc_val, Value::Null) {
        return Ok(Value::Null);
    }

    // Convert the doc to a serde_json::Value for path walking. Reuse the
    // json-functions helpers via a tiny to-serde converter that handles
    // Jsonb, Json, and Text inputs.
    let sj = match &doc_val {
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::decode(b.as_ref())?,
        Value::Json(s) | Value::Text(s) => {
            serde_json::from_str(s).map_err(|e| DbError::InvalidValue {
                reason: format!("invalid JSON document: {e}"),
            })?
        }
        other => {
            return Err(DbError::TypeMismatch {
                expected: "JSON or JSONB document".into(),
                got: other.variant_name().into(),
            })
        }
    };

    // Phase 11.19c: substitute PASSING bindings `$name` → literal form.
    let effective_path = if passing.is_empty() {
        path.clone()
    } else {
        let mut out = path.clone();
        for (bind_expr, name) in passing {
            let bound = crate::eval::eval_with(bind_expr, row, sq)?;
            let literal = sql_value_to_jsonpath_literal(&bound);
            // Replace `$name` (token boundary: next char must not be alnum/_).
            let placeholder = format!("${name}");
            out = replace_jsonpath_var(&out, &placeholder, &literal);
        }
        out
    };

    // Walk the path with strict/lax semantics, returning one of:
    //   Matched(Vec<serde_json::Value>) — zero-or-more results
    //   Error(DbError)                  — strict-mode failure
    let outcome = execute_path(&sj, &effective_path, *path_mode);

    match kind {
        SqlJsonQueryKind::Exists => match outcome {
            PathOutcome::Matched(results) => Ok(Value::Bool(!results.is_empty())),
            PathOutcome::Error(e) => apply_on_error_exists(on_error, e, row, sq),
        },
        SqlJsonQueryKind::Value => match outcome {
            PathOutcome::Error(e) => apply_on_behavior(on_error, Some(e), returning, row, sq),
            PathOutcome::Matched(results) => {
                if results.is_empty() {
                    apply_on_behavior(on_empty, None, returning, row, sq)
                } else if results.len() > 1 {
                    // Multi-item → route via ON ERROR (MVP: WITH WRAPPER
                    // not yet supported).
                    apply_on_behavior(
                        on_error,
                        Some(DbError::InvalidValue {
                            reason: "JSON_VALUE matched more than one item".into(),
                        }),
                        returning,
                        row,
                        sq,
                    )
                } else {
                    let only = &results[0];
                    if !is_scalar(only) {
                        apply_on_behavior(
                            on_error,
                            Some(DbError::InvalidValue {
                                reason:
                                    "JSON_VALUE only accepts scalar matches (use JSON_QUERY for objects/arrays)"
                                        .into(),
                            }),
                            returning,
                            row,
                            sq,
                        )
                    } else {
                        match scalar_to_value(only, returning.as_ref()) {
                            Ok(v) => Ok(v),
                            Err(e) => apply_on_behavior(on_error, Some(e), returning, row, sq),
                        }
                    }
                }
            }
        },
        SqlJsonQueryKind::Query => match outcome {
            PathOutcome::Error(e) => apply_on_behavior(on_error, Some(e), returning, row, sq),
            PathOutcome::Matched(results) => {
                if results.is_empty() {
                    apply_on_behavior(on_empty, None, returning, row, sq)
                } else {
                    let wrapped = apply_wrapper(&results, *wrapper);
                    match wrapped {
                        WrapOutcome::Scalar(v) => {
                            if matches!(quotes, SqlJsonQuotes::Omit)
                                && matches!(v, serde_json::Value::String(_))
                            {
                                // OMIT QUOTES on a scalar string → render as TEXT.
                                let s = match &v {
                                    serde_json::Value::String(s) => s.clone(),
                                    _ => unreachable!(),
                                };
                                match returning {
                                    Some(dt) => coerce(Value::Text(s), *dt, CoercionMode::Strict),
                                    None => Ok(Value::Text(s)),
                                }
                            } else {
                                query_result_to_value(&v, returning.as_ref())
                            }
                        }
                        WrapOutcome::MultiError => apply_on_behavior(
                            on_error,
                            Some(DbError::InvalidValue {
                                reason: "JSON_QUERY matched more than one item (use WITH WRAPPER)"
                                    .into(),
                            }),
                            returning,
                            row,
                            sq,
                        ),
                    }
                }
            }
        },
    }
}

// ── Path walker ─────────────────────────────────────────────────────────────-

enum PathOutcome {
    /// Zero or more matches.
    Matched(Vec<serde_json::Value>),
    /// Strict-mode violation (missing key, OOB index, type mismatch).
    Error(DbError),
}

/// Outcome of applying a `WITH/WITHOUT WRAPPER` clause over a non-empty
/// match set (Phase 11.19b).
enum WrapOutcome {
    /// A single serde_json value ready to be encoded/coerced.
    Scalar(serde_json::Value),
    /// Multi-item match with `WITHOUT WRAPPER` → route via ON ERROR.
    MultiError,
}

fn apply_wrapper(results: &[serde_json::Value], wrapper: SqlJsonWrapper) -> WrapOutcome {
    match wrapper {
        SqlJsonWrapper::Without => {
            if results.len() == 1 {
                WrapOutcome::Scalar(results[0].clone())
            } else {
                WrapOutcome::MultiError
            }
        }
        SqlJsonWrapper::Unconditional => {
            WrapOutcome::Scalar(serde_json::Value::Array(results.to_vec()))
        }
        SqlJsonWrapper::Conditional => {
            // PG: wrap unless the result is a single item that is already
            // a JSON array.
            if results.len() == 1 && results[0].is_array() {
                WrapOutcome::Scalar(results[0].clone())
            } else {
                WrapOutcome::Scalar(serde_json::Value::Array(results.to_vec()))
            }
        }
    }
}

/// Walk the path against `doc` with the requested mode. Mirrors PG's
/// `executeJsonPath`: strict mode surfaces every absence/type error;
/// lax mode silently drops missing parts and auto-unwraps arrays.
fn execute_path(doc: &serde_json::Value, path: &str, mode: SqlJsonPathMode) -> PathOutcome {
    let trimmed = path.trim();
    // Reject wildcards for MVP — PG allows them but the semantic around
    // strict/lax + multi-item + JSON_VALUE (scalar-only) gets intricate.
    // Users can fall back to JSON_EXTRACT for wildcarded extractions.
    if trimmed.contains("[*]") || trimmed.contains(".*") || trimmed.contains("..") {
        return PathOutcome::Error(DbError::InvalidValue {
            reason: format!("wildcard paths not supported in SQL/JSON MVP: {path}"),
        });
    }

    let parts = match parse_path_steps(trimmed) {
        Ok(p) => p,
        Err(e) => return PathOutcome::Error(e),
    };

    let mut current = doc.clone();
    for (idx, step) in parts.iter().enumerate() {
        match walk_step(&current, step, mode) {
            WalkResult::Descend(next) => current = next,
            WalkResult::Missing => {
                return match mode {
                    SqlJsonPathMode::Lax => PathOutcome::Matched(vec![]),
                    SqlJsonPathMode::Strict => PathOutcome::Error(DbError::InvalidValue {
                        reason: format!(
                            "strict SQL/JSON path: missing element at step {} ({:?})",
                            idx, step
                        ),
                    }),
                };
            }
            WalkResult::Mismatch(msg) => {
                return match mode {
                    SqlJsonPathMode::Lax => PathOutcome::Matched(vec![]),
                    SqlJsonPathMode::Strict => {
                        PathOutcome::Error(DbError::InvalidValue { reason: msg })
                    }
                };
            }
        }
    }
    PathOutcome::Matched(vec![current])
}

#[derive(Debug)]
enum PathStep {
    Key(String),
    Index(i64),
}

enum WalkResult {
    Descend(serde_json::Value),
    Missing,
    Mismatch(String),
}

fn parse_path_steps(path: &str) -> Result<Vec<PathStep>, DbError> {
    let mut parts: Vec<PathStep> = Vec::new();
    let mut chars = path.chars().peekable();
    if chars.next() != Some('$') {
        return Err(DbError::InvalidValue {
            reason: format!("SQL/JSON path must start with $: {path:?}"),
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
            parts.push(PathStep::Key(buf));
        } else if c == '[' {
            chars.next();
            let mut buf = String::new();
            for ch in chars.by_ref() {
                if ch == ']' {
                    break;
                }
                buf.push(ch);
            }
            let n: i64 = buf.trim().parse().map_err(|_| DbError::InvalidValue {
                reason: format!("array index {buf:?} is not an integer in path: {path}"),
            })?;
            parts.push(PathStep::Index(n));
        } else {
            return Err(DbError::InvalidValue {
                reason: format!("unexpected character {c:?} in path: {path}"),
            });
        }
    }
    Ok(parts)
}

fn walk_step(node: &serde_json::Value, step: &PathStep, mode: SqlJsonPathMode) -> WalkResult {
    match (node, step) {
        (serde_json::Value::Object(map), PathStep::Key(k)) => match map.get(k) {
            Some(v) => WalkResult::Descend(v.clone()),
            None => WalkResult::Missing,
        },
        (serde_json::Value::Array(arr), PathStep::Index(i)) => {
            let len = arr.len() as i64;
            let real = if *i < 0 { len + *i } else { *i };
            if real < 0 || real >= len {
                WalkResult::Missing
            } else {
                WalkResult::Descend(arr[real as usize].clone())
            }
        }
        // lax: auto-unwrap — arrays can be walked with a key step by
        // iterating elements. For MVP single-key we take the first
        // element's key if present.
        (serde_json::Value::Array(arr), PathStep::Key(k))
            if matches!(mode, SqlJsonPathMode::Lax) =>
        {
            for elem in arr {
                if let serde_json::Value::Object(m) = elem {
                    if let Some(v) = m.get(k) {
                        return WalkResult::Descend(v.clone());
                    }
                }
            }
            WalkResult::Missing
        }
        // Scalar with further path steps → type mismatch.
        _ => WalkResult::Mismatch(format!(
            "SQL/JSON path step {:?} cannot be applied to {}",
            step,
            serde_json_type_name(node)
        )),
    }
}

fn serde_json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn is_scalar(v: &serde_json::Value) -> bool {
    matches!(
        v,
        serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_)
    )
}

// ── Behavior dispatch ───────────────────────────────────────────────────────-

fn apply_on_behavior<R: SubqueryRunner>(
    behavior: &SqlJsonOnBehavior,
    err_cause: Option<DbError>,
    returning: &Option<DataType>,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    match behavior {
        SqlJsonOnBehavior::Null => Ok(Value::Null),
        SqlJsonOnBehavior::Error => match err_cause {
            Some(e) => Err(e),
            None => Err(DbError::InvalidValue {
                reason: "SQL/JSON path returned no matches (ON EMPTY ERROR)".into(),
            }),
        },
        SqlJsonOnBehavior::Default(expr) => {
            let v = crate::eval::eval_with(expr, row, sq)?;
            match returning {
                Some(dt) => coerce(v, *dt, CoercionMode::Strict),
                None => Ok(v),
            }
        }
        SqlJsonOnBehavior::TrueLit | SqlJsonOnBehavior::FalseLit | SqlJsonOnBehavior::Unknown => {
            Err(DbError::InvalidValue {
                reason: "TRUE/FALSE/UNKNOWN are only valid for JSON_EXISTS ON ERROR".into(),
            })
        }
    }
}

fn apply_on_error_exists<R: SubqueryRunner>(
    behavior: &SqlJsonOnBehavior,
    err: DbError,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    match behavior {
        SqlJsonOnBehavior::TrueLit => Ok(Value::Bool(true)),
        SqlJsonOnBehavior::FalseLit => Ok(Value::Bool(false)),
        SqlJsonOnBehavior::Unknown => Ok(Value::Null),
        SqlJsonOnBehavior::Null => Ok(Value::Null),
        SqlJsonOnBehavior::Error => Err(err),
        SqlJsonOnBehavior::Default(expr) => {
            let v = crate::eval::eval_with(expr, row, sq)?;
            coerce(v, DataType::Bool, CoercionMode::Strict)
        }
    }
}

// ── Coercion helpers ────────────────────────────────────────────────────────-

fn scalar_to_value(sj: &serde_json::Value, returning: Option<&DataType>) -> Result<Value, DbError> {
    let base = match sj {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::BigInt(i)
            } else if let Some(f) = n.as_f64() {
                Value::Real(f)
            } else {
                Value::Text(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::Text(s.clone()),
        _ => {
            return Err(DbError::InvalidValue {
                reason: "JSON_VALUE expected a scalar match".into(),
            })
        }
    };
    match returning {
        Some(dt) => coerce(base, *dt, CoercionMode::Strict),
        None => match base {
            // Default: return as TEXT for JSON_VALUE.
            Value::Null => Ok(Value::Null),
            Value::Text(_) => Ok(base),
            other => Ok(Value::Text(format_scalar(other))),
        },
    }
}

fn format_scalar(v: Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(s) => s,
        other => format!("{other:?}"),
    }
}

fn query_result_to_value(
    sj: &serde_json::Value,
    returning: Option<&DataType>,
) -> Result<Value, DbError> {
    use axiomdb_types::JsonbEncoder;
    use std::sync::Arc;

    match returning {
        Some(DataType::Text) => Ok(Value::Text(sj.to_string())),
        Some(DataType::Jsonb) | None => {
            let blob = JsonbEncoder::encode(sj)?;
            Ok(Value::Jsonb(Arc::new(blob)))
        }
        Some(other) => {
            // Coerce the JSON text via the generic coercer. Works for BOOL,
            // numeric, DATE, TIMESTAMP when the match is a scalar.
            if is_scalar(sj) {
                scalar_to_value(sj, Some(other))
            } else {
                Err(DbError::InvalidValue {
                    reason: format!(
                        "JSON_QUERY: cannot coerce {} to {:?}",
                        serde_json_type_name(sj),
                        other
                    ),
                })
            }
        }
    }
}

// ── Phase 11.19c helpers: PASSING variable substitution ─────────────────────

/// Render a SQL `Value` as a jsonpath literal for direct string substitution
/// into the path before parse. Strings are JSON-quoted, bools become `true`/
/// `false`, integers/floats use their decimal form, NULL becomes `null`.
fn sql_value_to_jsonpath_literal(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(s) | Value::Json(s) => {
            // JSON-quote the string so comparisons work in filter contexts.
            serde_json::Value::String(s.clone()).to_string()
        }
        other => other.to_string(),
    }
}

/// Replace every occurrence of `$name` in `path` with `literal`. `$name`
/// matches only when the next character is NOT a word char (letter, digit,
/// underscore) so `$foo` does not match inside `$foobar`.
fn replace_jsonpath_var(path: &str, placeholder: &str, literal: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let bytes = path.as_bytes();
    let ph = placeholder.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if i + ph.len() <= bytes.len() && &bytes[i..i + ph.len()] == ph {
            let next = bytes.get(i + ph.len()).copied();
            let is_word = next.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_');
            if !is_word {
                out.push_str(literal);
                i += ph.len();
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
