use axiomdb_core::error::DbError;
use axiomdb_types::{coerce::coerce_for_op, Value};

use crate::{
    expr::{BinaryOp, Expr, UnaryOp},
    text_semantics::compare_text,
};

use super::{array_ops, current_eval_collation};

/// Returns `true` only for `Value::Bool(true)`.
///
/// Used by the executor to filter rows from WHERE predicates:
/// - `NULL` (UNKNOWN) → `false` — row excluded
/// - `Value::Bool(false)` → `false` — row excluded
/// - `Value::Bool(true)` → `true` — row included
/// - Any other value → `false` — type error in predicate; row excluded
pub fn is_truthy(value: &Value) -> bool {
    matches!(value, Value::Bool(true))
}

// ── NULL helpers ──────────────────────────────────────────────────────────────

/// AND truth table applied to already-evaluated values (no row context needed).
/// Used by BETWEEN to combine two comparison results.
pub(super) fn apply_and_values(l: Value, r: Value) -> Value {
    match (&l, &r) {
        (Value::Bool(false), _) | (_, Value::Bool(false)) => Value::Bool(false),
        (Value::Bool(true), Value::Bool(true)) => Value::Bool(true),
        _ => Value::Null, // NULL AND TRUE = NULL, NULL AND NULL = NULL
    }
}

/// NOT applied to an already-evaluated value.
pub(super) fn apply_not(v: Value) -> Value {
    match v {
        Value::Bool(b) => Value::Bool(!b),
        Value::Null => Value::Null,
        other => other, // unreachable in well-typed expressions
    }
}

// ── Short-circuit AND / OR ────────────────────────────────────────────────────

pub(super) fn eval_and(left: &Expr, right: &Expr, row: &[Value]) -> Result<Value, DbError> {
    let l = crate::eval::eval(left, row)?;
    match l {
        // FALSE dominates: short-circuit — do NOT evaluate right.
        Value::Bool(false) => Ok(Value::Bool(false)),
        // TRUE: result is entirely determined by right.
        Value::Bool(true) => crate::eval::eval(right, row),
        // NULL (UNKNOWN): must evaluate right.
        Value::Null => {
            let r = crate::eval::eval(right, row)?;
            Ok(match r {
                // FALSE wins over NULL.
                Value::Bool(false) => Value::Bool(false),
                // TRUE or NULL → UNKNOWN.
                _ => Value::Null,
            })
        }
        // Non-boolean left operand.
        other => Err(DbError::TypeMismatch {
            expected: "Bool".into(),
            got: other.variant_name().into(),
        }),
    }
}

pub(super) fn eval_or(left: &Expr, right: &Expr, row: &[Value]) -> Result<Value, DbError> {
    let l = crate::eval::eval(left, row)?;
    match l {
        // TRUE dominates: short-circuit — do NOT evaluate right.
        Value::Bool(true) => Ok(Value::Bool(true)),
        // FALSE: result is entirely determined by right.
        Value::Bool(false) => crate::eval::eval(right, row),
        // NULL (UNKNOWN): must evaluate right.
        Value::Null => {
            let r = crate::eval::eval(right, row)?;
            Ok(match r {
                // TRUE wins over NULL.
                Value::Bool(true) => Value::Bool(true),
                // FALSE or NULL → UNKNOWN.
                _ => Value::Null,
            })
        }
        other => Err(DbError::TypeMismatch {
            expected: "Bool".into(),
            got: other.variant_name().into(),
        }),
    }
}

// ── Unary evaluation ──────────────────────────────────────────────────────────

pub(super) fn eval_unary(op: UnaryOp, v: Value) -> Result<Value, DbError> {
    // NULL propagates through all unary ops.
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    match op {
        UnaryOp::Neg => match v {
            Value::Int(n) => n.checked_neg().map(Value::Int).ok_or(DbError::Overflow),
            Value::BigInt(n) => n.checked_neg().map(Value::BigInt).ok_or(DbError::Overflow),
            Value::Real(f) => Ok(Value::Real(-f)),
            Value::Decimal(m, s) => m
                .checked_neg()
                .map(|neg| Value::Decimal(neg, s))
                .ok_or(DbError::Overflow),
            other => Err(DbError::TypeMismatch {
                expected: "numeric".into(),
                got: other.variant_name().into(),
            }),
        },
        UnaryOp::Not => match v {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            other => Err(DbError::TypeMismatch {
                expected: "Bool".into(),
                got: other.variant_name().into(),
            }),
        },
        UnaryOp::BitNot => {
            // MySQL: ~n → bitwise NOT of the integer representation.
            // Cast to u64, apply bitwise NOT, return as BigInt.
            let n = value_to_i64_bits(&v);
            Ok(Value::BigInt(!(n as u64) as i64))
        }
    }
}

// ── XOR ───────────────────────────────────────────────────────────────────────

/// Boolean XOR — no short-circuit because both sides are needed.
pub(super) fn eval_xor(left: &Expr, right: &Expr, row: &[Value]) -> Result<Value, DbError> {
    let l = crate::eval::eval(left, row)?;
    let r = crate::eval::eval(right, row)?;
    match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a ^ b)),
        _ => Err(DbError::TypeMismatch {
            expected: "Bool".into(),
            got: l.variant_name().into(),
        }),
    }
}

// ── Array operator dispatch (Phase 20.4, Step 5) ───────────────────────────────

/// Handles binary operators when the LHS is `Value::Array`.
///
/// Array operators handle NULL internally following SQL 3VL semantics.
/// This function is called BEFORE the generic NULL propagation check in `eval_binary`.
fn eval_binary_array(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    // Array operators all handle NULL internally.
    match op {
        // Equality / inequality — element-by-element with NULL propagation
        BinaryOp::Eq => array_ops::array_equals(&l, &r),
        BinaryOp::NotEq => {
            let eq = array_ops::array_equals(&l, &r)?;
            // NULL equality → NULL; invert the bool
            match eq {
                Value::Bool(b) => Ok(Value::Bool(!b)),
                Value::Null => Ok(Value::Null),
                _ => unreachable!("array_equals returns Bool or Null"),
            }
        }

        // Containment: @> and <@
        BinaryOp::JsonContains => array_ops::array_contains(&l, &r),
        BinaryOp::JsonContainedBy => array_ops::array_contained_by(&l, &r),

        // Overlap: &&
        BinaryOp::ArrayOverlap => array_ops::array_overlap(&l, &r),

        // Concatenation: ||
        BinaryOp::Concat => {
            if matches!(&r, Value::Array(_)) {
                array_ops::array_concat(&l, &r)
            } else {
                // element || array → prepend element to array
                array_ops::array_concat_element_to_array(&r, &l)
            }
        }

        // All other operators: type error on array
        _ => Err(DbError::TypeMismatch {
            expected: "array operator".into(),
            got: format!("{} on Array", op_variant_name(&op)),
        }),
    }
}

fn op_variant_name(op: &BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "Add",
        BinaryOp::Sub => "Sub",
        BinaryOp::Mul => "Mul",
        BinaryOp::Div => "Div",
        BinaryOp::Mod => "Mod",
        BinaryOp::Eq => "Eq",
        BinaryOp::NotEq => "NotEq",
        BinaryOp::Lt => "Lt",
        BinaryOp::LtEq => "LtEq",
        BinaryOp::Gt => "Gt",
        BinaryOp::GtEq => "GtEq",
        BinaryOp::And => "And",
        BinaryOp::Or => "Or",
        BinaryOp::Xor => "Xor",
        BinaryOp::Concat => "Concat",
        BinaryOp::NullSafe => "NullSafe",
        BinaryOp::IntDiv => "IntDiv",
        BinaryOp::BitAnd => "BitAnd",
        BinaryOp::BitOr => "BitOr",
        BinaryOp::BitXor => "BitXor",
        BinaryOp::ShiftLeft => "ShiftLeft",
        BinaryOp::ShiftRight => "ShiftRight",
        BinaryOp::Regexp => "Regexp",
        BinaryOp::RegexpTilde => "RegexpTilde",
        BinaryOp::RegexpITilde => "RegexpITilde",
        BinaryOp::RegexpNotTilde => "RegexpNotTilde",
        BinaryOp::RegexpNotITilde => "RegexpNotITilde",
        BinaryOp::JsonSub => "JsonSub",
        BinaryOp::JsonContains => "JsonContains",
        BinaryOp::JsonContainedBy => "JsonContainedBy",
        BinaryOp::JsonExists => "JsonExists",
        BinaryOp::JsonbPathExists => "JsonbPathExists",
        BinaryOp::JsonbPathMatch => "JsonbPathMatch",
        BinaryOp::JsonExistsAny => "JsonExistsAny",
        BinaryOp::JsonExistsAll => "JsonExistsAll",
        BinaryOp::JsonPathExtract => "JsonPathExtract",
        BinaryOp::JsonPathExtractText => "JsonPathExtractText",
        BinaryOp::JsonPathDelete => "JsonPathDelete",
        BinaryOp::ArrayOverlap => "ArrayOverlap",
    }
}

// ── Binary evaluation ─────────────────────────────────────────────────────────

/// Evaluates a binary op on already-evaluated operands (non-AND/OR/XOR).
/// NULL propagates: if either operand is NULL, the result is NULL
/// (except `<=>` which handles NULL explicitly).
pub(crate) fn eval_binary(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    // `<=>` (null-safe equality) must run BEFORE the NULL propagation check.
    if op == BinaryOp::NullSafe {
        let result = match (&l, &r) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            _ => match eval_comparison(BinaryOp::Eq, l, r)? {
                Value::Bool(b) => b,
                _ => false,
            },
        };
        return Ok(Value::Bool(result));
    }

    // ── Array operator dispatch (Phase 20.4, Step 5) ──────────────────────────
    // Array operators handle NULL internally and follow SQL 3VL semantics,
    // so we intercept BEFORE the generic NULL propagation check.
    if matches!(&l, Value::Array(_)) {
        return eval_binary_array(op, l, r);
    }

    // ── Range operator dispatch (Phase 20.13) ─────────────────────────────────
    if matches!(&l, Value::Range(_))
        || (matches!(&r, Value::Range(_)) && matches!(op, BinaryOp::JsonContainedBy))
    {
        return eval_binary_range(op, l, r);
    }

    // ── Money operator dispatch (Phase 20.17) ─────────────────────────────────
    if matches!(&l, Value::Money(..)) || matches!(&r, Value::Money(..)) {
        return eval_binary_money(op, l, r);
    }

    // ── Ltree operator dispatch (Phase 20.19) ─────────────────────────────────
    if matches!(&l, Value::Ltree(_)) {
        return eval_binary_ltree(op, l, r);
    }

    // NULL propagation for all other binary ops.
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    match op {
        BinaryOp::Add | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => eval_arithmetic(op, l, r),
        // `-` is polymorphic (Phase 11.18a): JSONB LHS triggers key / index
        // deletion; otherwise it stays arithmetic subtraction.
        BinaryOp::Sub => match (&l, &r) {
            (Value::Jsonb(doc), Value::Text(key)) => eval_jsonb_delete_key(doc, key),
            (Value::Jsonb(doc), Value::Int(i)) => eval_jsonb_delete_idx(doc, *i as i64),
            (Value::Jsonb(doc), Value::BigInt(i)) => eval_jsonb_delete_idx(doc, *i),
            _ => eval_arithmetic(BinaryOp::Sub, l, r),
        },
        BinaryOp::IntDiv => eval_int_div(l, r),

        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => eval_comparison(op, l, r),

        // `||` is polymorphic (Phase 11.18a): JSONB on both sides → PG
        // concat semantics; element || array → array with prepended element;
        // array || element → array with appended element.
        // NULL propagation already handled above.
        BinaryOp::Concat => match (&l, &r) {
            (Value::Jsonb(a), Value::Jsonb(b)) => eval_jsonb_concat(a, b),
            (Value::Array(_), Value::Array(_)) => array_ops::array_concat(&l, &r),
            (Value::Array(_), elem) => array_ops::array_concat_element_to_array(&l, elem),
            (elem, Value::Array(_)) => array_ops::array_concat_element_to_array(&r, elem),
            _ => eval_concat(l, r),
        },

        BinaryOp::BitAnd => Ok(Value::BigInt(value_to_i64_bits(&l) & value_to_i64_bits(&r))),
        BinaryOp::BitOr => Ok(Value::BigInt(value_to_i64_bits(&l) | value_to_i64_bits(&r))),
        BinaryOp::BitXor => Ok(Value::BigInt(value_to_i64_bits(&l) ^ value_to_i64_bits(&r))),
        BinaryOp::ShiftLeft => {
            let n = value_to_i64_bits(&l);
            let s = value_to_i64_bits(&r);
            if !(0..64).contains(&s) {
                Ok(Value::BigInt(0))
            } else {
                Ok(Value::BigInt(n << s))
            }
        }
        BinaryOp::ShiftRight => {
            let n = value_to_i64_bits(&l) as u64;
            let s = value_to_i64_bits(&r);
            if !(0..64).contains(&s) {
                Ok(Value::BigInt(0))
            } else {
                Ok(Value::BigInt((n >> s) as i64))
            }
        }

        BinaryOp::Regexp => eval_regexp(l, r),
        BinaryOp::RegexpTilde => eval_regexp_tilde(l, r, false, false),
        BinaryOp::RegexpITilde => eval_regexp_tilde(l, r, true, false),
        BinaryOp::RegexpNotTilde => eval_regexp_tilde(l, r, false, true),
        BinaryOp::RegexpNotITilde => eval_regexp_tilde(l, r, true, true),

        BinaryOp::Xor => match (&l, &r) {
            (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a ^ b)),
            _ => Err(DbError::TypeMismatch {
                expected: "Bool".into(),
                got: l.variant_name().into(),
            }),
        },

        // AND, OR, XOR are handled in `eval` before calling here.
        BinaryOp::And | BinaryOp::Or => unreachable!("AND/OR handled in eval"),
        // NullSafe handled above.
        BinaryOp::NullSafe => unreachable!(),

        // ── JSON sub-document extraction: -> (Phase 11.16) ───────────────────
        // `json -> 'key'` returns the sub-document as Value::Jsonb.
        // `json -> 0`     returns the array element at index 0 as Value::Jsonb.
        BinaryOp::JsonSub => eval_json_sub(l, r),

        // ── JSONB containment: @> (Phase 11.17) ──────────────────────────────
        // `doc @> query` returns 1 if every key/value in query is in doc.
        BinaryOp::JsonContains => eval_json_contains(l, r),

        // ── JSONB contained-by: <@ (Phase 11.18a) ───────────────────────────-
        // Reverse of @>: delegate with swapped arguments to reuse the
        // existing deep-containment walk.
        BinaryOp::JsonContainedBy => eval_json_contains(r, l),

        // ── JSONB key / array-string exists: ? (Phase 11.18a) ────────────────
        BinaryOp::JsonExists => eval_jsonb_exists(l, r),

        // ── JSONB JSONPath exists: @? (Phase 11.21b) ─────────────────────────
        BinaryOp::JsonbPathExists => eval_jsonb_path_exists(l, r),

        // ── JSONB JSONPath match:  @@ (Phase 11.21c) ─────────────────────────
        BinaryOp::JsonbPathMatch => eval_jsonb_path_match(l, r),

        // ── JSONB any/all-keys exists: ?| / ?& (Phase 11.18b) ────────────────
        BinaryOp::JsonExistsAny => eval_jsonb_exists_set(l, r, false),
        BinaryOp::JsonExistsAll => eval_jsonb_exists_set(l, r, true),

        // ── JSONB path operators: #>, #>>, #- (Phase 11.18c) ─────────────────
        BinaryOp::JsonPathExtract => eval_jsonb_path_extract(l, r, false),
        BinaryOp::JsonPathExtractText => eval_jsonb_path_extract(l, r, true),
        BinaryOp::JsonPathDelete => eval_jsonb_path_delete(l, r),

        // Array overlap operator (Phase 20.4, Step 5)
        // Only valid when LHS is Value::Array; otherwise type error.
        // (If LHS were Array, eval_binary_array would have handled it already.)
        BinaryOp::ArrayOverlap => Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: l.variant_name().into(),
        }),
    }
}

fn jsonb_path_segments(rhs: Value) -> Result<Vec<String>, DbError> {
    let sj = value_to_serde_json(rhs)?;
    let arr = match sj {
        serde_json::Value::Array(a) => a,
        other => {
            return Err(DbError::TypeMismatch {
                expected: "JSONB array of path segments".into(),
                got: other.to_string(),
            });
        }
    };
    arr.into_iter()
        .map(|e| match e {
            serde_json::Value::String(s) => Ok(s),
            serde_json::Value::Number(n) => Ok(n.to_string()),
            other => Err(DbError::TypeMismatch {
                expected: "string or number path segment".into(),
                got: other.to_string(),
            }),
        })
        .collect()
}

fn descend_serde<'a>(
    node: &'a serde_json::Value,
    parts: &[String],
) -> Option<&'a serde_json::Value> {
    let mut cur = node;
    for p in parts {
        cur = match cur {
            serde_json::Value::Object(m) => m.get(p)?,
            serde_json::Value::Array(a) => {
                let idx: i64 = p.parse().ok()?;
                let real = if idx < 0 { a.len() as i64 + idx } else { idx };
                if real < 0 {
                    return None;
                }
                a.get(real as usize)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn eval_jsonb_path_extract(left: Value, right: Value, as_text: bool) -> Result<Value, DbError> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let parts = jsonb_path_segments(right)?;
    // Fast path: navigate the binary JSONB directly, decoding only the target
    // node instead of the whole document. The serde path below decodes every
    // row's full blob into a serde_json tree, which dominates JSONB scan cost.
    if let Value::Jsonb(blob) = &left {
        return jsonb_extract_binary(blob, &parts, as_text);
    }
    let doc = value_to_serde_json(left)?;
    let target = match descend_serde(&doc, &parts) {
        Some(v) => v.clone(),
        None => return Ok(Value::Null),
    };
    if as_text {
        match target {
            serde_json::Value::String(s) => Ok(Value::Text(s)),
            other => Ok(Value::Text(other.to_string())),
        }
    } else {
        let blob = axiomdb_types::jsonb::JsonbEncoder::encode(&target)?;
        Ok(Value::Jsonb(std::sync::Arc::new(blob)))
    }
}

/// Binary JSONB path extraction for `->`/`->>` on a `Value::Jsonb` operand.
///
/// Walks the blob with `JsonbRef` (O(log k) key lookups, O(1) array index) and
/// decodes only the target node via `JsonbValue::to_serde`. The tail conversion
/// is identical to the serde path in [`eval_jsonb_path_extract`], so results
/// match exactly — only the per-row full-document decode is avoided.
fn jsonb_extract_binary(blob: &[u8], parts: &[String], as_text: bool) -> Result<Value, DbError> {
    use axiomdb_types::{JsonbRef, JsonbValue};

    let target: serde_json::Value = if parts.is_empty() {
        axiomdb_types::jsonb::JsonbDecoder::decode(blob)?
    } else {
        // `held` keeps the current container blob alive while descending: the top
        // level borrows `blob`; each descent stores the sub-container's `Arc`
        // (a refcount bump, never a deep copy).
        let mut held: Option<std::sync::Arc<Vec<u8>>> = None;
        let mut found: Option<JsonbValue> = None;
        for (i, p) in parts.iter().enumerate() {
            let cur: &[u8] = match &held {
                Some(arc) => arc.as_slice(),
                None => blob,
            };
            let r = JsonbRef::new(cur);
            // Mirror `descend_serde`: array container indexes by integer segment
            // (negative counts from the end); otherwise look the segment up as a
            // key. A scalar yields `None` from both `get_index`/`get_key`.
            let got = if r.is_array() {
                match p.parse::<i64>() {
                    Ok(idx) => r.get_index(idx)?,
                    Err(_) => return Ok(Value::Null),
                }
            } else {
                r.get_key(p)?
            };
            let Some(val) = got else {
                return Ok(Value::Null);
            };
            if i + 1 == parts.len() {
                found = Some(val);
                break;
            }
            match val {
                JsonbValue::Container(sub) => held = Some(sub),
                // Scalar with path segments still to consume → no match.
                _ => return Ok(Value::Null),
            }
        }
        match found {
            Some(v) => v.to_serde(),
            None => return Ok(Value::Null),
        }
    };

    if as_text {
        match target {
            serde_json::Value::String(s) => Ok(Value::Text(s)),
            other => Ok(Value::Text(other.to_string())),
        }
    } else {
        let out = axiomdb_types::jsonb::JsonbEncoder::encode(&target)?;
        Ok(Value::Jsonb(std::sync::Arc::new(out)))
    }
}

fn eval_jsonb_path_delete(left: Value, right: Value) -> Result<Value, DbError> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let parts = jsonb_path_segments(right)?;
    let mut doc = value_to_serde_json(left)?;
    if !parts.is_empty() {
        prune_serde(&mut doc, &parts);
    }
    let blob = axiomdb_types::jsonb::JsonbEncoder::encode(&doc)?;
    Ok(Value::Jsonb(std::sync::Arc::new(blob)))
}

fn prune_serde(node: &mut serde_json::Value, parts: &[String]) {
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut cur: &mut serde_json::Value = node;
    for p in parents {
        cur = match cur {
            serde_json::Value::Object(m) => match m.get_mut(p) {
                Some(v) => v,
                None => return,
            },
            serde_json::Value::Array(a) => {
                let idx: i64 = match p.parse() {
                    Ok(i) => i,
                    Err(_) => return,
                };
                let real = if idx < 0 { a.len() as i64 + idx } else { idx };
                if real < 0 || real as usize >= a.len() {
                    return;
                }
                &mut a[real as usize]
            }
            _ => return,
        };
    }
    match cur {
        serde_json::Value::Object(m) => {
            m.remove(last);
        }
        serde_json::Value::Array(a) => {
            if let Ok(idx) = last.parse::<i64>() {
                let real = if idx < 0 { a.len() as i64 + idx } else { idx };
                if real >= 0 && (real as usize) < a.len() {
                    a.remove(real as usize);
                }
            }
        }
        _ => {}
    }
}

fn eval_jsonb_exists_set(left: Value, right: Value, all: bool) -> Result<Value, DbError> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let doc = value_to_serde_json(left)?;
    let rhs = value_to_serde_json(right)?;
    let keys: Vec<String> = match rhs {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .filter_map(|e| match e {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "JSONB array of strings".into(),
                got: other.to_string(),
            });
        }
    };
    let exists = |k: &str| -> bool {
        match &doc {
            serde_json::Value::Object(map) => map.contains_key(k),
            serde_json::Value::Array(arr) => arr
                .iter()
                .any(|e| matches!(e, serde_json::Value::String(s) if s == k)),
            _ => false,
        }
    };
    let result = if all {
        keys.iter().all(|k| exists(k))
    } else {
        keys.iter().any(|k| exists(k))
    };
    Ok(Value::Bool(result))
}

fn eval_jsonb_path_match(doc: Value, path: Value) -> Result<Value, DbError> {
    if doc.is_null() || path.is_null() {
        return Ok(Value::Null);
    }
    let path_str = match path {
        Value::Text(s) | Value::Json(s) => s,
        other => {
            return Err(DbError::TypeMismatch {
                expected: "TEXT jsonpath".into(),
                got: other.variant_name().into(),
            });
        }
    };
    let sj = match &doc {
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::decode(b.as_ref())?,
        Value::Json(s) | Value::Text(s) => {
            serde_json::from_str(s).map_err(|e| DbError::InvalidValue {
                reason: format!("invalid JSON: {e}"),
            })?
        }
        other => {
            return Err(DbError::TypeMismatch {
                expected: "JSON or JSONB".into(),
                got: other.variant_name().into(),
            });
        }
    };
    let steps = crate::eval::functions::parse_jsonpath_public(&path_str)?;
    let results = crate::eval::functions::execute_jsonpath_owned_public(&sj, &steps);
    if results.len() != 1 {
        return Ok(Value::Null);
    }
    match &results[0] {
        serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
        _ => Ok(Value::Null),
    }
}

fn eval_jsonb_path_exists(doc: Value, path: Value) -> Result<Value, DbError> {
    if doc.is_null() || path.is_null() {
        return Ok(Value::Null);
    }
    let path_str = match path {
        Value::Text(s) | Value::Json(s) => s,
        other => {
            return Err(DbError::TypeMismatch {
                expected: "TEXT jsonpath".into(),
                got: other.variant_name().into(),
            });
        }
    };
    let sj = match &doc {
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::decode(b.as_ref())?,
        Value::Json(s) | Value::Text(s) => {
            serde_json::from_str(s).map_err(|e| DbError::InvalidValue {
                reason: format!("invalid JSON: {e}"),
            })?
        }
        other => {
            return Err(DbError::TypeMismatch {
                expected: "JSON or JSONB".into(),
                got: other.variant_name().into(),
            });
        }
    };
    let steps = crate::eval::functions::parse_jsonpath_public(&path_str)?;
    Ok(Value::Bool(
        !crate::eval::functions::execute_jsonpath_owned_public(&sj, &steps).is_empty(),
    ))
}

fn eval_json_sub(left: Value, right: Value) -> Result<Value, DbError> {
    use axiomdb_types::{JsonbEncoder, JsonbRef, JsonbValue};

    if left.is_null() {
        return Ok(Value::Null);
    }

    // Obtain the JSONB binary blob for the left side.
    let blob_owned: Vec<u8>;
    let blob: &[u8] = match &left {
        Value::Jsonb(b) => b.as_ref(),
        Value::Json(s) => {
            // Lazy encode text JSON to binary for navigation
            let parsed = serde_json::from_str::<serde_json::Value>(s).map_err(|e| {
                DbError::InvalidValue {
                    reason: format!("invalid JSON: {e}"),
                }
            })?;
            blob_owned = JsonbEncoder::encode(&parsed)?;
            &blob_owned
        }
        _ => {
            return Err(DbError::TypeMismatch {
                expected: "JSON or JSONB".into(),
                got: left.variant_name().into(),
            });
        }
    };

    let jref = JsonbRef::new(blob);
    let result = match &right {
        Value::Text(key) | Value::Json(key) => jref.get_key(key)?,
        Value::Int(idx) => jref.get_index(*idx as i64)?,
        Value::BigInt(idx) => jref.get_index(*idx)?,
        _ => {
            return Err(DbError::TypeMismatch {
                expected: "string key or integer index for ->".into(),
                got: right.variant_name().into(),
            });
        }
    };

    match result {
        None => Ok(Value::Null),
        Some(JsonbValue::Null) => Ok(Value::Null),
        Some(JsonbValue::Bool(b)) => Ok(Value::Bool(b)),
        Some(JsonbValue::Int(n)) => Ok(Value::BigInt(n)),
        Some(JsonbValue::Float(f)) => Ok(Value::Real(f)),
        Some(JsonbValue::String(s)) => Ok(Value::Text(s.as_ref().to_owned())),
        Some(JsonbValue::Container(bytes)) => Ok(Value::Jsonb(bytes)),
    }
}

// ── Arithmetic ────────────────────────────────────────────────────────────────

fn eval_arithmetic(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    let (l, r) = coerce_for_op(l, r)?;
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => int_arith(op, a, b),
        (Value::BigInt(a), Value::BigInt(b)) => bigint_arith(op, a, b),
        (Value::Real(a), Value::Real(b)) => {
            // MySQL: float division by zero → NULL (not ±Infinity).
            if op == BinaryOp::Div && b == 0.0 {
                return Ok(Value::Null);
            }
            Ok(Value::Real(real_arith(op, a, b)?))
        }
        (Value::Decimal(m1, s1), Value::Decimal(m2, s2)) => decimal_arith(op, m1, s1, m2, s2),
        _ => unreachable!("coerce_for_op ensures matching types"),
    }
}

fn int_arith(op: BinaryOp, a: i32, b: i32) -> Result<Value, DbError> {
    let result = match op {
        BinaryOp::Add => a.checked_add(b).ok_or(DbError::Overflow)?,
        BinaryOp::Sub => a.checked_sub(b).ok_or(DbError::Overflow)?,
        BinaryOp::Mul => a.checked_mul(b).ok_or(DbError::Overflow)?,
        BinaryOp::Div => {
            if b == 0 {
                return Err(DbError::DivisionByZero);
            }
            a.checked_div(b).ok_or(DbError::Overflow)? // handles MIN/-1
        }
        BinaryOp::Mod => {
            if b == 0 {
                return Err(DbError::DivisionByZero);
            }
            a.checked_rem(b).ok_or(DbError::Overflow)?
        }
        _ => unreachable!(),
    };
    Ok(Value::Int(result))
}

fn bigint_arith(op: BinaryOp, a: i64, b: i64) -> Result<Value, DbError> {
    let result = match op {
        BinaryOp::Add => a.checked_add(b).ok_or(DbError::Overflow)?,
        BinaryOp::Sub => a.checked_sub(b).ok_or(DbError::Overflow)?,
        BinaryOp::Mul => a.checked_mul(b).ok_or(DbError::Overflow)?,
        BinaryOp::Div => {
            if b == 0 {
                return Err(DbError::DivisionByZero);
            }
            a.checked_div(b).ok_or(DbError::Overflow)?
        }
        BinaryOp::Mod => {
            if b == 0 {
                return Err(DbError::DivisionByZero);
            }
            a.checked_rem(b).ok_or(DbError::Overflow)?
        }
        _ => unreachable!(),
    };
    Ok(Value::BigInt(result))
}

fn real_arith(op: BinaryOp, a: f64, b: f64) -> Result<f64, DbError> {
    Ok(match op {
        BinaryOp::Add => a + b,
        BinaryOp::Sub => a - b,
        BinaryOp::Mul => a * b,
        // IEEE 754: division by zero gives ±Infinity, which is allowed for Real.
        BinaryOp::Div => a / b,
        BinaryOp::Mod => a % b,
        _ => unreachable!(),
    })
}

fn decimal_arith(op: BinaryOp, m1: i128, s1: u8, m2: i128, s2: u8) -> Result<Value, DbError> {
    match op {
        BinaryOp::Add | BinaryOp::Sub => {
            // Align scales: bring both to the higher scale.
            let (a, b, scale) = if s1 >= s2 {
                let factor = 10i128.pow((s1 - s2) as u32);
                (m1, m2.checked_mul(factor).ok_or(DbError::Overflow)?, s1)
            } else {
                let factor = 10i128.pow((s2 - s1) as u32);
                (m1.checked_mul(factor).ok_or(DbError::Overflow)?, m2, s2)
            };
            let result = if op == BinaryOp::Add {
                a.checked_add(b).ok_or(DbError::Overflow)?
            } else {
                a.checked_sub(b).ok_or(DbError::Overflow)?
            };
            Ok(Value::Decimal(result, scale))
        }
        BinaryOp::Mul => {
            let result = m1.checked_mul(m2).ok_or(DbError::Overflow)?;
            let scale = s1.saturating_add(s2);
            Ok(Value::Decimal(result, scale))
        }
        BinaryOp::Div => {
            if m2 == 0 {
                return Err(DbError::DivisionByZero);
            }
            // Scale numerator by 10^(s2 + extra) so the result carries s1+extra
            // fractional digits. extra is capped so total scale stays ≤ 38.
            let extra = 6u8.min(38u8.saturating_sub(s1));
            let scale_up = (s2 as u32) + (extra as u32);
            let scaled = m1
                .checked_mul(10i128.pow(scale_up))
                .ok_or(DbError::Overflow)?;
            let result = scaled.checked_div(m2).ok_or(DbError::Overflow)?;
            let scale = s1.saturating_add(extra);
            Ok(Value::Decimal(result, scale))
        }
        BinaryOp::Mod => {
            if m2 == 0 {
                return Err(DbError::DivisionByZero);
            }
            let result = m1.checked_rem(m2).ok_or(DbError::Overflow)?;
            Ok(Value::Decimal(result, s1))
        }
        _ => unreachable!(),
    }
}

// ── Comparison ────────────────────────────────────────────────────────────────

fn eval_comparison(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    // Text equality fast path — `compare_text` adds a raw-text tie-break
    // on top of the canonical comparison so it can be used for ORDER BY
    // (deterministic sort), but that tie-break makes case-equal strings
    // like `"Alice"` and `"alice"` report `Less` under collations like
    // `Es` (utf8mb4_unicode_ci) when their canonical forms match. For
    // `=` / `<>` we want the canonical comparison alone — call `text_eq`
    // directly, which doesn't tie-break.
    if matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
        if let (Value::Text(a), Value::Text(b)) = (&l, &r) {
            let eq = crate::text_semantics::text_eq(current_eval_collation(), a, b);
            return Ok(Value::Bool(if op == BinaryOp::Eq { eq } else { !eq }));
        }
    }
    let ord = compare_values(&l, &r)?;
    Ok(Value::Bool(match op {
        BinaryOp::Eq => ord == std::cmp::Ordering::Equal,
        BinaryOp::NotEq => ord != std::cmp::Ordering::Equal,
        BinaryOp::Lt => ord == std::cmp::Ordering::Less,
        BinaryOp::LtEq => ord != std::cmp::Ordering::Greater,
        BinaryOp::Gt => ord == std::cmp::Ordering::Greater,
        BinaryOp::GtEq => ord != std::cmp::Ordering::Less,
        _ => unreachable!(),
    }))
}

/// Compares two non-NULL values of compatible types.
pub(crate) fn compare_values(l: &Value, r: &Value) -> Result<std::cmp::Ordering, DbError> {
    // Try numeric widening for mixed types first; fall through on incompatible types.
    let (l, r) = match coerce_for_op(l.clone(), r.clone()) {
        Ok(pair) => pair,
        Err(_) => (l.clone(), r.clone()),
    };

    match (&l, &r) {
        (Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        (Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
        (Value::BigInt(a), Value::BigInt(b)) => Ok(a.cmp(b)),
        (Value::Real(a), Value::Real(b)) => a.partial_cmp(b).ok_or(DbError::TypeMismatch {
            expected: "comparable Real".into(),
            got: "NaN".into(),
        }),
        (Value::Decimal(m1, s1), Value::Decimal(m2, s2)) => {
            // Align scales for comparison.
            if s1 == s2 {
                Ok(m1.cmp(m2))
            } else if s1 > s2 {
                let factor = 10i128.pow((*s1 - *s2) as u32);
                Ok(m1.cmp(&m2.saturating_mul(factor)))
            } else {
                let factor = 10i128.pow((*s2 - *s1) as u32);
                Ok(m1.saturating_mul(factor).cmp(m2))
            }
        }
        (Value::Text(a), Value::Text(b)) => Ok(compare_text(current_eval_collation(), a, b)),
        (Value::Bytes(a), Value::Bytes(b)) => Ok(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Ok(a.cmp(b)),
        (Value::Timestamp(a), Value::Timestamp(b)) => Ok(a.cmp(b)),
        (Value::TimestampTz(a), Value::TimestampTz(b)) => Ok(a.cmp(b)),
        // Cross-type comparison: strip tz annotation, compare µs values
        (Value::Timestamp(a), Value::TimestampTz(b))
        | (Value::TimestampTz(a), Value::Timestamp(b)) => Ok(a.cmp(b)),
        (Value::Uuid(a), Value::Uuid(b)) => Ok(a.cmp(b)),
        // MySQL compatibility: allow comparing DECIMAL with numeric text.
        // Common ORM pattern: `WHERE dec_col = '123.45'`.
        (Value::Decimal(m, s), Value::Text(t)) => text_vs_decimal_cmp(t, *m, *s),
        (Value::Text(t), Value::Decimal(m, s)) => {
            text_vs_decimal_cmp(t, *m, *s).map(|o| o.reverse())
        }
        // 4.18d: implicit coercion of string literals to date/timestamp (MySQL
        // compatibility). `WHERE ts_col = '2024-01-01'` and similar patterns used
        // by every ORM must not fail with TypeMismatch.
        // text_vs_ts_cmp returns `stored.cmp(parsed_text)` — i.e. ordering of the
        // stored value relative to the text. For (Timestamp, Text) that's exactly
        // what we want. For (Text, Timestamp) we need the reverse.
        (Value::Timestamp(micros), Value::Text(s)) => text_vs_ts_cmp(s, *micros),
        (Value::Text(s), Value::Timestamp(micros)) => {
            text_vs_ts_cmp(s, *micros).map(|o| o.reverse())
        }
        (Value::TimestampTz(micros), Value::Text(s)) => text_vs_ts_cmp(s, *micros),
        (Value::Text(s), Value::TimestampTz(micros)) => {
            text_vs_ts_cmp(s, *micros).map(|o| o.reverse())
        }
        (Value::Date(days), Value::Text(s)) => text_vs_date_cmp(s, *days),
        (Value::Text(s), Value::Date(days)) => text_vs_date_cmp(s, *days).map(|o| o.reverse()),
        _ => Err(DbError::TypeMismatch {
            expected: "comparable types".into(),
            got: format!("{} and {}", l.variant_name(), r.variant_name()),
        }),
    }
}

// ── Date/timestamp text-coercion helpers (4.18d) ──────────────────────────────

/// Parses `s` as `%Y-%m-%d %H:%i:%s` or `%Y-%m-%d` and compares with `micros`.
fn text_vs_ts_cmp(s: &str, micros: i64) -> Result<std::cmp::Ordering, DbError> {
    use crate::eval::functions::datetime::{micros_to_ndt, str_to_date_inner};
    let parsed =
        str_to_date_inner(s, "%Y-%m-%d %H:%i:%s").or_else(|| str_to_date_inner(s, "%Y-%m-%d"));
    match parsed {
        Some((ndt, _)) => Ok(micros_to_ndt(micros).cmp(&ndt)),
        None => Err(DbError::TypeMismatch {
            expected: "date/time string".into(),
            got: s.to_string(),
        }),
    }
}

/// Parses `s` as `%Y-%m-%d` (or datetime with time part ignored) and compares
/// with `days` (days since 1970-01-01).
fn text_vs_date_cmp(s: &str, days: i32) -> Result<std::cmp::Ordering, DbError> {
    use crate::eval::functions::datetime::{days_to_ndate, str_to_date_inner};
    let parsed =
        str_to_date_inner(s, "%Y-%m-%d").or_else(|| str_to_date_inner(s, "%Y-%m-%d %H:%i:%s"));
    match parsed {
        Some((ndt, _)) => Ok(days_to_ndate(days).cmp(&ndt.date())),
        None => Err(DbError::TypeMismatch {
            expected: "date string".into(),
            got: s.to_string(),
        }),
    }
}

/// Parses `s` as DECIMAL and compares with `(mantissa, scale)`.
fn text_vs_decimal_cmp(s: &str, mantissa: i128, scale: u8) -> Result<std::cmp::Ordering, DbError> {
    use axiomdb_types::{coerce, CoercionMode, DataType, Value};

    let parsed = match coerce(
        Value::Text(s.to_string()),
        DataType::Decimal,
        CoercionMode::Strict,
    ) {
        Ok(Value::Decimal(m, sc)) => (m, sc),
        _ => {
            return Err(DbError::TypeMismatch {
                expected: "decimal string".into(),
                got: s.to_string(),
            });
        }
    };

    let (m2, s2) = parsed;
    if scale == s2 {
        return Ok(mantissa.cmp(&m2));
    }
    if scale > s2 {
        let factor = 10i128.pow((scale - s2) as u32);
        Ok(mantissa.cmp(&m2.saturating_mul(factor)))
    } else {
        let factor = 10i128.pow((s2 - scale) as u32);
        Ok(mantissa.saturating_mul(factor).cmp(&m2))
    }
}

// ── String concat ─────────────────────────────────────────────────────────────

fn eval_concat(l: Value, r: Value) -> Result<Value, DbError> {
    match (l, r) {
        (Value::Text(a), Value::Text(b)) => Ok(Value::Text(a + &b)),
        (Value::Bytes(mut a), Value::Bytes(b)) => {
            a.extend_from_slice(&b);
            Ok(Value::Bytes(a))
        }
        (l, r) => Err(DbError::TypeMismatch {
            expected: "Text || Text or Bytes || Bytes".into(),
            got: format!("{} || {}", l.variant_name(), r.variant_name()),
        }),
    }
}

// ── IN list ───────────────────────────────────────────────────────────────────

pub(super) fn eval_in(v: Value, list: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    // NULL expr → UNKNOWN.
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }

    let mut has_null_in_list = false;

    for item_expr in list {
        let item = crate::eval::eval(item_expr, row)?;
        match item {
            Value::Null => {
                has_null_in_list = true;
            }
            ref iv => {
                // Check equality (NULL-safe at the item level).
                match compare_values(&v, iv) {
                    Ok(std::cmp::Ordering::Equal) => return Ok(Value::Bool(true)),
                    Ok(_) => {}  // not equal, continue
                    Err(_) => {} // incompatible types, treat as not equal
                }
            }
        }
    }

    // No match found.
    if has_null_in_list {
        Ok(Value::Null) // UNKNOWN — can't determine definitively
    } else {
        Ok(Value::Bool(false)) // definitively not in list
    }
}

// ── LIKE ──────────────────────────────────────────────────────────────────────

/// Iterative LIKE pattern matching on Unicode characters.
///
/// `%` matches any sequence of zero or more characters.
/// `_` matches exactly one character.
/// All other characters match literally (case-sensitive).
///
/// Algorithm: O(n·m) with backtracking, handles all patterns including
/// multiple `%` without exponential blowup.
pub fn like_match(text: &str, pattern: &str) -> bool {
    // Phase 11.14: fast paths for common LIKE patterns — avoid O(n·m)
    // backtracking and Vec<char> allocation when a simpler check suffices.

    // Fast path 1: 'prefix%' — starts_with (O(prefix_len), zero alloc).
    // Matches InnoDB's internal optimization in field.cc `Field::key_cmp`.
    if let Some(prefix) = pattern.strip_suffix('%') {
        if !prefix.contains('%') && !prefix.contains('_') {
            return text.starts_with(prefix);
        }
    }

    // Fast path 2: '%suffix' — ends_with (O(suffix_len), zero alloc).
    if let Some(suffix) = pattern.strip_prefix('%') {
        if !suffix.contains('%') && !suffix.contains('_') {
            return text.ends_with(suffix);
        }
    }

    // Fast path 3: '%infix%' — contains (O(n), zero alloc).
    if pattern.starts_with('%') && pattern.ends_with('%') && pattern.len() >= 2 {
        let infix = &pattern[1..pattern.len() - 1];
        if !infix.contains('%') && !infix.contains('_') {
            return text.contains(infix);
        }
    }

    // Fast path 4: exact match (no wildcards at all).
    if !pattern.contains('%') && !pattern.contains('_') {
        return text == pattern;
    }

    // General path: O(n·m) with backtracking.
    let text: Vec<char> = text.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();
    let (n, m) = (text.len(), pat.len());

    let mut ti: usize = 0;
    let mut pi: usize = 0;
    // Backtrack points: last '%' in pattern and the text position at that time.
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < n {
        if pi < m && (pat[pi] == '_' || pat[pi] == text[ti]) {
            // Literal or '_' match — advance both.
            ti += 1;
            pi += 1;
        } else if pi < m && pat[pi] == '%' {
            // '%' — record backtrack point, advance only pattern.
            // '%' matches zero characters to start.
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(spi) = star_pi {
            // Mismatch — backtrack: '%' matches one more text character.
            star_ti += 1;
            ti = star_ti;
            pi = spi + 1;
        } else {
            // No backtrack point — definitive mismatch.
            return false;
        }
    }

    // Consume any trailing '%' in the pattern (they match empty string).
    while pi < m && pat[pi] == '%' {
        pi += 1;
    }

    pi == m
}

/// Evaluates `text LIKE pattern ESCAPE escape_ch`.
///
/// When `escape_ch` is Some(c), any pattern character immediately following `c`
/// is treated as a literal (not as `%` or `_`). The escape char itself is matched
/// by doubling it: `LIKE 'a%%' ESCAPE '%'` matches literal `a%`.
pub fn like_match_with_escape(text: &str, pattern: &str, escape_ch: char) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();
    let (n, m) = (text.len(), pat.len());

    let mut ti: usize = 0;
    let mut pi: usize = 0;
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < n {
        if pi < m && pat[pi] == escape_ch {
            // Escaped character: treat next pattern char as literal.
            pi += 1;
            if pi < m && pat[pi] == text[ti] {
                ti += 1;
                pi += 1;
            } else if let Some(spi) = star_pi {
                star_ti += 1;
                ti = star_ti;
                pi = spi + 1;
            } else {
                return false;
            }
        } else if pi < m && (pat[pi] == '_' || pat[pi] == text[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < m && pat[pi] == '%' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(spi) = star_pi {
            star_ti += 1;
            ti = star_ti;
            pi = spi + 1;
        } else {
            return false;
        }
    }

    // Consume trailing '%' (skip escaped chars — they require a character).
    while pi < m {
        if pat[pi] == escape_ch {
            break; // escaped char needs text char → no match
        }
        if pat[pi] != '%' {
            break;
        }
        pi += 1;
    }

    pi == m
}

// ── Bitwise helpers ───────────────────────────────────────────────────────────

/// Cast a Value to its i64 bit-pattern for bitwise operations.
/// NULL must be handled by the caller before calling this.
pub(super) fn value_to_i64_bits(v: &Value) -> i64 {
    match v {
        Value::Bool(b) => *b as i64,
        Value::Int(n) => *n as i64,
        Value::BigInt(n) => *n,
        Value::Real(f) => *f as i64,
        Value::Text(s) => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

// ── Integer division ──────────────────────────────────────────────────────────

/// `a DIV b` — integer division truncated toward zero.
fn eval_int_div(l: Value, r: Value) -> Result<Value, DbError> {
    let a = value_to_i64_bits(&l);
    let b = value_to_i64_bits(&r);
    if b == 0 {
        // MySQL: integer DIV by zero returns NULL (not an error).
        return Ok(Value::Null);
    }
    Ok(Value::BigInt(a / b))
}

// ── REGEXP ────────────────────────────────────────────────────────────────────

/// `text REGEXP pattern` — evaluates the regex against the text.
/// Returns Bool or propagates NULL.
fn eval_regexp(l: Value, r: Value) -> Result<Value, DbError> {
    #[cfg(not(feature = "regexp"))]
    {
        let _ = (l, r);
        return Err(DbError::NotImplemented {
            feature: "REGEXP operator (compile with regexp feature to enable)".into(),
        });
    }
    #[cfg(feature = "regexp")]
    {
        let text = match l {
            Value::Text(s) => s,
            other => {
                return Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                })
            }
        };
        let pattern = match r {
            Value::Text(s) => s,
            other => {
                return Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                })
            }
        };
        let re = regex::Regex::new(&pattern).map_err(|e| DbError::InvalidValue {
            reason: format!("invalid REGEXP pattern: {e}"),
        })?;
        Ok(Value::Bool(re.is_match(&text)))
    }
}

// ── PostgreSQL tilde regex operators (Phase 20.15) ────────────────────────────

/// `~`, `~*`, `!~`, `!~*` — POSIX regex operators.
/// NULL propagation is handled by `eval_binary` before this call.
fn eval_regexp_tilde(
    l: Value,
    r: Value,
    case_insensitive: bool,
    negate: bool,
) -> Result<Value, DbError> {
    #[cfg(not(feature = "regexp"))]
    {
        let _ = (l, r, case_insensitive, negate);
        return Err(DbError::NotImplemented {
            feature: "REGEXP tilde operators (compile with regexp feature to enable)".into(),
        });
    }
    #[cfg(feature = "regexp")]
    {
        let text = match l {
            Value::Text(s) => s,
            other => {
                return Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                })
            }
        };
        let pattern = match r {
            Value::Text(s) => s,
            other => {
                return Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                })
            }
        };
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| DbError::InvalidValue {
                reason: format!("invalid regex pattern: {e}"),
            })?;
        Ok(Value::Bool(re.is_match(&text) ^ negate))
    }
}

// ── JSONB containment: @> (Phase 11.17) ──────────────────────────────────────

fn eval_json_contains(left: Value, right: Value) -> Result<Value, DbError> {
    use axiomdb_types::jsonb::{jsonb_contains, JsonbEncoder};

    // NULL propagation
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }

    // Obtain binary JSONB bytes for both sides.
    // Text literals ('{"a":1}') are treated as JSON strings, parsed on the fly.
    let to_blob = |v: Value| -> Result<Vec<u8>, DbError> {
        match v {
            Value::Jsonb(b) => Ok(b.as_ref().clone()),
            Value::Json(s) | Value::Text(s) => {
                let parsed = serde_json::from_str::<serde_json::Value>(&s).map_err(|e| {
                    DbError::InvalidValue {
                        reason: format!("invalid JSON in @> operand: {e}"),
                    }
                })?;
                JsonbEncoder::encode(&parsed)
            }
            other => Err(DbError::TypeMismatch {
                expected: "JSON or JSONB".into(),
                got: other.variant_name().into(),
            }),
        }
    };

    let doc_blob = to_blob(left)?;
    let query_blob = to_blob(right)?;
    let result = jsonb_contains(&doc_blob, &query_blob)?;
    Ok(Value::Bool(result))
}

// ── JSONB ? / <@ / || / - helpers (Phase 11.18a) ─────────────────────────────
//
// All four helpers round-trip through `serde_json::Value` because the current
// JSONB binary layout does not expose mutation primitives (Phase 11.22 will
// add those). Decoding to serde_json and re-encoding with `JsonbEncoder` is
// exact — no precision loss — and keeps the key-sort / JEntry-stride
// invariants owned by the encoder, not by these helpers.

/// Coerces a `Value` to a `serde_json::Value` by way of its JSONB blob.
/// Accepts `Jsonb`, `Json`, or `Text` (the latter parsed as a JSON literal,
/// matching the tolerant surface used by `@>` and `->`).
fn value_to_serde_json(v: Value) -> Result<serde_json::Value, DbError> {
    use axiomdb_types::jsonb::JsonbDecoder;
    match v {
        Value::Jsonb(b) => JsonbDecoder::decode(b.as_ref()),
        Value::Json(s) | Value::Text(s) => {
            serde_json::from_str(&s).map_err(|e| DbError::InvalidValue {
                reason: format!("invalid JSON operand: {e}"),
            })
        }
        other => Err(DbError::TypeMismatch {
            expected: "JSON or JSONB".into(),
            got: other.variant_name().into(),
        }),
    }
}

fn serde_json_to_jsonb_value(v: &serde_json::Value) -> Result<Value, DbError> {
    use axiomdb_types::JsonbEncoder;
    use std::sync::Arc;
    let blob = JsonbEncoder::encode(v)?;
    Ok(Value::Jsonb(Arc::new(blob)))
}

/// PG `?` — object key exists OR array contains the text as a string element.
/// Every other LHS kind returns false (not error).
fn eval_jsonb_exists(left: Value, right: Value) -> Result<Value, DbError> {
    let key = match right {
        Value::Text(s) | Value::Json(s) => s,
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "text key for ?".into(),
                got: other.variant_name().into(),
            })
        }
    };
    let doc = value_to_serde_json(left)?;
    let found = match &doc {
        serde_json::Value::Object(map) => map.contains_key(&key),
        serde_json::Value::Array(arr) => arr
            .iter()
            .any(|e| matches!(e, serde_json::Value::String(s) if s == &key)),
        _ => false,
    };
    Ok(Value::Bool(found))
}

/// PG `||` — concatenate two JSONB values.
/// Rules (from `jsonfuncs.c::IteratorConcat`):
///   object || object → shallow merge (RHS keys override)
///   array  || array  → append
///   object || array  → [obj, ...arr]
///   array  || object → [...arr, obj]
///   scalar on either side → wrap as a 1-element array and recurse
fn eval_jsonb_concat(a: &[u8], b: &[u8]) -> Result<Value, DbError> {
    use axiomdb_types::jsonb::JsonbDecoder;
    let lhs = JsonbDecoder::decode(a)?;
    let rhs = JsonbDecoder::decode(b)?;
    let merged = jsonb_concat_serde(lhs, rhs);
    serde_json_to_jsonb_value(&merged)
}

fn jsonb_concat_serde(a: serde_json::Value, b: serde_json::Value) -> serde_json::Value {
    use serde_json::Value as J;
    let is_container = |v: &J| matches!(v, J::Object(_) | J::Array(_));
    match (a, b) {
        (J::Object(mut la), J::Object(rb)) => {
            for (k, v) in rb {
                la.insert(k, v);
            }
            J::Object(la)
        }
        (J::Array(mut la), J::Array(rb)) => {
            la.extend(rb);
            J::Array(la)
        }
        (obj @ J::Object(_), J::Array(mut arr)) => {
            let mut out = Vec::with_capacity(arr.len() + 1);
            out.push(obj);
            out.append(&mut arr);
            J::Array(out)
        }
        (J::Array(mut arr), obj @ J::Object(_)) => {
            arr.push(obj);
            J::Array(arr)
        }
        // scalar on either side (but not both sides being containers above).
        (lhs, rhs) => {
            let la = if is_container(&lhs) {
                lhs
            } else {
                J::Array(vec![lhs])
            };
            let rb = if is_container(&rhs) {
                rhs
            } else {
                J::Array(vec![rb_from_scalar(rhs)])
            };
            // `rb` has been moved; reconstruct via second pass.
            jsonb_concat_serde(la, rb)
        }
    }
}

// Helper to sidestep the borrow checker in the scalar-wrap branch.
#[inline]
fn rb_from_scalar(v: serde_json::Value) -> serde_json::Value {
    v
}

/// PG `-(jsonb, text)` — drop an object key or every string array element
/// equal to the text. Scalar LHS is an error (matches `jsonfuncs.c:4673`).
fn eval_jsonb_delete_key(doc: &[u8], key: &str) -> Result<Value, DbError> {
    use axiomdb_types::jsonb::JsonbDecoder;
    let v = JsonbDecoder::decode(doc)?;
    let out = match v {
        serde_json::Value::Object(mut map) => {
            map.remove(key);
            serde_json::Value::Object(map)
        }
        serde_json::Value::Array(arr) => {
            let kept: Vec<_> = arr
                .into_iter()
                .filter(|e| !matches!(e, serde_json::Value::String(s) if s == key))
                .collect();
            serde_json::Value::Array(kept)
        }
        _ => {
            return Err(DbError::InvalidValue {
                reason: "cannot delete from scalar JSONB".into(),
            })
        }
    };
    serde_json_to_jsonb_value(&out)
}

/// PG `-(jsonb, int)` — drop an array element at the given index.
/// Negative counts from end. Out-of-range is a no-op (not an error).
/// Object LHS → error. Scalar LHS → error.
fn eval_jsonb_delete_idx(doc: &[u8], idx: i64) -> Result<Value, DbError> {
    use axiomdb_types::jsonb::JsonbDecoder;
    let v = JsonbDecoder::decode(doc)?;
    let out = match v {
        serde_json::Value::Array(mut arr) => {
            let n = arr.len() as i64;
            let real = if idx < 0 { n + idx } else { idx };
            if real >= 0 && real < n {
                arr.remove(real as usize);
            }
            serde_json::Value::Array(arr)
        }
        serde_json::Value::Object(_) => {
            return Err(DbError::InvalidValue {
                reason: "cannot delete from object using integer index".into(),
            })
        }
        _ => {
            return Err(DbError::InvalidValue {
                reason: "cannot delete from scalar JSONB".into(),
            })
        }
    };
    serde_json_to_jsonb_value(&out)
}

// ── Range binary operators (Phase 20.13) ─────────────────────────────────────

/// Dispatch binary operators when LHS (or RHS for <@) is a range value.
fn eval_binary_range(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    match op {
        BinaryOp::Eq => {
            let (a, b) = require_both_ranges(l, r, "=")?;
            Ok(Value::Bool(a == b))
        }
        BinaryOp::NotEq => {
            let (a, b) = require_both_ranges(l, r, "<>")?;
            Ok(Value::Bool(a != b))
        }
        BinaryOp::Lt => {
            let (a, b) = require_both_ranges(l, r, "<")?;
            Ok(Value::Bool(a < b))
        }
        BinaryOp::LtEq => {
            let (a, b) = require_both_ranges(l, r, "<=")?;
            Ok(Value::Bool(a <= b))
        }
        BinaryOp::Gt => {
            let (a, b) = require_both_ranges(l, r, ">")?;
            Ok(Value::Bool(a > b))
        }
        BinaryOp::GtEq => {
            let (a, b) = require_both_ranges(l, r, ">=")?;
            Ok(Value::Bool(a >= b))
        }

        // @> containment: range @> range  OR  range @> element
        BinaryOp::JsonContains => match l {
            Value::Range(ref rv) => match &r {
                Value::Range(rv2) => Ok(Value::Bool(rv.contains_range(rv2))),
                _ => Ok(Value::Bool(rv.contains_value(&r))),
            },
            _ => Err(DbError::TypeMismatch {
                expected: "RANGE @> RANGE or RANGE @> element".to_string(),
                got: format!("{} @> {}", l.variant_name(), r.variant_name()),
            }),
        },

        // <@ contained-by: range <@ range  OR  element <@ range
        BinaryOp::JsonContainedBy => match r {
            Value::Range(ref rv) => match &l {
                Value::Range(rv_l) => Ok(Value::Bool(rv.contains_range(rv_l))),
                _ => Ok(Value::Bool(rv.contains_value(&l))),
            },
            _ => Err(DbError::TypeMismatch {
                expected: "RANGE <@ RANGE or element <@ RANGE".to_string(),
                got: format!("{} <@ {}", l.variant_name(), r.variant_name()),
            }),
        },

        // && overlap
        BinaryOp::ArrayOverlap => {
            let (a, b) = require_both_ranges(l, r, "&&")?;
            Ok(Value::Bool(a.overlaps(&b)))
        }

        // + union
        BinaryOp::Add => {
            let (a, b) = require_both_ranges(l, r, "+")?;
            a.union(&b)
                .map(|rv| Value::Range(Box::new(rv)))
                .ok_or_else(|| DbError::InvalidValue {
                    reason: "ranges do not overlap or are not adjacent — cannot compute union"
                        .to_string(),
                })
        }

        // * intersection
        BinaryOp::Mul => {
            let (a, b) = require_both_ranges(l, r, "*")?;
            Ok(Value::Range(Box::new(a.intersection(&b))))
        }

        // - difference
        BinaryOp::Sub => {
            let (a, b) = require_both_ranges(l, r, "-")?;
            match a.difference(&b) {
                Some(rv) => Ok(Value::Range(Box::new(rv))),
                None => Err(DbError::InvalidValue {
                    reason: "range difference would produce a non-contiguous result".to_string(),
                }),
            }
        }

        _ => Err(DbError::TypeMismatch {
            expected: "range operator".to_string(),
            got: format!("{} on Range", op_variant_name(&op)),
        }),
    }
}

fn require_both_ranges(
    l: Value,
    r: Value,
    op: &str,
) -> Result<
    (
        Box<axiomdb_types::range_value::RangeValue>,
        Box<axiomdb_types::range_value::RangeValue>,
    ),
    DbError,
> {
    match (l, r) {
        (Value::Range(a), Value::Range(b)) => Ok((a, b)),
        (l, r) => Err(DbError::TypeMismatch {
            expected: format!("RANGE {op} RANGE"),
            got: format!("{} {op} {}", l.variant_name(), r.variant_name()),
        }),
    }
}

// ── Money arithmetic (Phase 20.17) ────────────────────────────────────────────

/// Handles binary operations involving at least one `Value::Money` operand.
///
/// Same-currency `+` and `-` are always valid. Cross-currency operations and
/// any other op on Money return `TypeMismatch`.
fn eval_binary_money(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    match (op, l, r) {
        (BinaryOp::Add, Value::Money(lm, ls, lc), Value::Money(rm, rs, rc)) => {
            if lc != rc {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "cannot add {} and {} — use CONVERT() to unify currencies first",
                        std::str::from_utf8(&lc).unwrap_or("?"),
                        std::str::from_utf8(&rc).unwrap_or("?"),
                    ),
                });
            }
            let (mantissa, scale) = align_and_add(lm, ls, rm, rs, false)?;
            Ok(Value::Money(mantissa, scale, lc))
        }
        (BinaryOp::Sub, Value::Money(lm, ls, lc), Value::Money(rm, rs, rc)) => {
            if lc != rc {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "cannot subtract {} and {} — use CONVERT() to unify currencies first",
                        std::str::from_utf8(&lc).unwrap_or("?"),
                        std::str::from_utf8(&rc).unwrap_or("?"),
                    ),
                });
            }
            let (mantissa, scale) = align_and_add(lm, ls, rm, rs, true)?;
            Ok(Value::Money(mantissa, scale, lc))
        }
        (BinaryOp::Eq, Value::Money(lm, ls, lc), Value::Money(rm, rs, rc)) => {
            if lc != rc {
                return Ok(Value::Bool(false));
            }
            let ord = compare_money(lm, ls, rm, rs);
            Ok(Value::Bool(ord == std::cmp::Ordering::Equal))
        }
        (BinaryOp::NotEq, Value::Money(lm, ls, lc), Value::Money(rm, rs, rc)) => {
            if lc != rc {
                return Ok(Value::Bool(true));
            }
            let ord = compare_money(lm, ls, rm, rs);
            Ok(Value::Bool(ord != std::cmp::Ordering::Equal))
        }
        (BinaryOp::Lt, Value::Money(lm, ls, lc), Value::Money(rm, rs, rc)) if lc == rc => Ok(
            Value::Bool(compare_money(lm, ls, rm, rs) == std::cmp::Ordering::Less),
        ),
        (BinaryOp::LtEq, Value::Money(lm, ls, lc), Value::Money(rm, rs, rc)) if lc == rc => Ok(
            Value::Bool(compare_money(lm, ls, rm, rs) != std::cmp::Ordering::Greater),
        ),
        (BinaryOp::Gt, Value::Money(lm, ls, lc), Value::Money(rm, rs, rc)) if lc == rc => Ok(
            Value::Bool(compare_money(lm, ls, rm, rs) == std::cmp::Ordering::Greater),
        ),
        (BinaryOp::GtEq, Value::Money(lm, ls, lc), Value::Money(rm, rs, rc)) if lc == rc => Ok(
            Value::Bool(compare_money(lm, ls, rm, rs) != std::cmp::Ordering::Less),
        ),
        // Scalar multiplication: MONEY * Int, MONEY * BigInt, Int * MONEY, BigInt * MONEY
        (BinaryOp::Mul, Value::Money(m, s, c), Value::Int(f)) => {
            let result = m
                .checked_mul(f as i128)
                .ok_or_else(|| DbError::InvalidValue {
                    reason: "money multiplication overflow".into(),
                })?;
            Ok(Value::Money(result, s, c))
        }
        (BinaryOp::Mul, Value::Money(m, s, c), Value::BigInt(f)) => {
            let result = m
                .checked_mul(f as i128)
                .ok_or_else(|| DbError::InvalidValue {
                    reason: "money multiplication overflow".into(),
                })?;
            Ok(Value::Money(result, s, c))
        }
        (BinaryOp::Mul, Value::Int(f), Value::Money(m, s, c)) => {
            let result = m
                .checked_mul(f as i128)
                .ok_or_else(|| DbError::InvalidValue {
                    reason: "money multiplication overflow".into(),
                })?;
            Ok(Value::Money(result, s, c))
        }
        (BinaryOp::Mul, Value::BigInt(f), Value::Money(m, s, c)) => {
            let result = m
                .checked_mul(f as i128)
                .ok_or_else(|| DbError::InvalidValue {
                    reason: "money multiplication overflow".into(),
                })?;
            Ok(Value::Money(result, s, c))
        }
        // Scalar division: MONEY / Int, MONEY / BigInt
        (BinaryOp::Div, Value::Money(m, s, c), Value::Int(f)) => {
            if f == 0 {
                return Err(DbError::InvalidValue {
                    reason: "division by zero".into(),
                });
            }
            Ok(Value::Money(m / (f as i128), s, c))
        }
        (BinaryOp::Div, Value::Money(m, s, c), Value::BigInt(f)) => {
            if f == 0 {
                return Err(DbError::InvalidValue {
                    reason: "division by zero".into(),
                });
            }
            Ok(Value::Money(m / (f as i128), s, c))
        }
        (op, l, r) => Err(DbError::TypeMismatch {
            expected: "MONEY +/- MONEY (same currency)".into(),
            got: format!(
                "{} {} {}",
                l.variant_name(),
                op_variant_name(&op),
                r.variant_name()
            ),
        }),
    }
}

/// Aligns two fixed-point values to the same scale then adds (or subtracts).
fn align_and_add(
    lm: i128,
    ls: u8,
    rm: i128,
    rs: u8,
    subtract: bool,
) -> Result<(i128, u8), DbError> {
    let scale = ls.max(rs);
    let lm_aligned = if ls < scale {
        lm.checked_mul(10i128.pow((scale - ls) as u32))
            .ok_or_else(|| DbError::InvalidValue {
                reason: "money arithmetic overflow".into(),
            })?
    } else {
        lm
    };
    let rm_aligned = if rs < scale {
        rm.checked_mul(10i128.pow((scale - rs) as u32))
            .ok_or_else(|| DbError::InvalidValue {
                reason: "money arithmetic overflow".into(),
            })?
    } else {
        rm
    };
    let result = if subtract {
        lm_aligned.checked_sub(rm_aligned)
    } else {
        lm_aligned.checked_add(rm_aligned)
    }
    .ok_or_else(|| DbError::InvalidValue {
        reason: "money arithmetic overflow".into(),
    })?;
    Ok((result, scale))
}

// ── Ltree operators (Phase 20.19) ────────────────────────────────────────────

/// Handles binary operators when LHS is `Value::Ltree`.
///
/// | Operator           | BinaryOp          | Semantics                                  |
/// |--------------------|-------------------|--------------------------------------------|
/// | `@>`               | JsonContains      | left is ancestor-or-equal of right         |
/// | `<@`               | JsonContainedBy   | left is descendant-or-equal of right       |
/// | `~`                | RegexpTilde       | left path matches lquery pattern (right)   |
/// | `\|\|`             | Concat            | concatenate left || right → new ltree      |
/// | `=`, `<`, `>`, … | comparison ops    | lexicographic string comparison            |
fn eval_binary_ltree(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    use axiomdb_types::{lquery_match, ltree_concat, ltree_is_ancestor};

    // NULL propagation: Ltree operators return NULL when either operand is NULL.
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }

    let lpath = match &l {
        Value::Ltree(s) => s.as_str(),
        _ => unreachable!("dispatch guarantees LHS is Ltree"),
    };

    match op {
        // @> — left is ancestor-or-equal of right
        BinaryOp::JsonContains => {
            let rpath = ltree_rhs_path(&r)?;
            Ok(Value::Bool(ltree_is_ancestor(lpath, rpath)))
        }

        // <@ — left is descendant-or-equal of right (right is ancestor of left)
        BinaryOp::JsonContainedBy => {
            let rpath = ltree_rhs_path(&r)?;
            Ok(Value::Bool(ltree_is_ancestor(rpath, lpath)))
        }

        // ~ — lquery pattern match; RHS is the pattern (Text or Ltree string)
        BinaryOp::RegexpTilde => {
            let pattern = ltree_rhs_text(&r)?;
            Ok(Value::Bool(lquery_match(lpath, pattern)))
        }

        // || — concatenate two ltree paths
        BinaryOp::Concat => {
            let rpath = ltree_rhs_path(&r)?;
            Ok(Value::Ltree(ltree_concat(lpath, rpath)))
        }

        // Comparison operators — compare ltree paths lexicographically
        BinaryOp::Eq => Ok(Value::Bool(lpath == ltree_rhs_path(&r)?)),
        BinaryOp::NotEq => Ok(Value::Bool(lpath != ltree_rhs_path(&r)?)),
        BinaryOp::Lt => Ok(Value::Bool(lpath < ltree_rhs_path(&r)?)),
        BinaryOp::LtEq => Ok(Value::Bool(lpath <= ltree_rhs_path(&r)?)),
        BinaryOp::Gt => Ok(Value::Bool(lpath > ltree_rhs_path(&r)?)),
        BinaryOp::GtEq => Ok(Value::Bool(lpath >= ltree_rhs_path(&r)?)),

        _ => Err(DbError::TypeMismatch {
            expected: "ltree operator (@>, <@, ~, ||, =, <, >)".into(),
            got: format!("{} on Ltree", op_variant_name(&op)),
        }),
    }
}

/// Extracts the path string from an ltree RHS value.
/// Accepts `Value::Ltree` or `Value::Text` (implicit coerce).
fn ltree_rhs_path(r: &Value) -> Result<&str, DbError> {
    match r {
        Value::Ltree(s) | Value::Text(s) => Ok(s.as_str()),
        _ => Err(DbError::TypeMismatch {
            expected: "LTREE".into(),
            got: r.variant_name().into(),
        }),
    }
}

/// Extracts a pattern string from an lquery RHS value.
/// Accepts `Value::Text` or `Value::Ltree` (pattern may contain `*`).
fn ltree_rhs_text(r: &Value) -> Result<&str, DbError> {
    match r {
        Value::Text(s) | Value::Ltree(s) => Ok(s.as_str()),
        _ => Err(DbError::TypeMismatch {
            expected: "TEXT (lquery pattern)".into(),
            got: r.variant_name().into(),
        }),
    }
}

/// Compares two same-currency money amounts by aligning their scales.
fn compare_money(lm: i128, ls: u8, rm: i128, rs: u8) -> std::cmp::Ordering {
    if ls == rs {
        return lm.cmp(&rm);
    }
    if ls > rs {
        let factor = 10i128.pow((ls - rs) as u32);
        lm.cmp(&rm.saturating_mul(factor))
    } else {
        let factor = 10i128.pow((rs - ls) as u32);
        lm.saturating_mul(factor).cmp(&rm)
    }
}

#[cfg(test)]
mod jsonb_extract_binary_tests {
    use super::*;
    use std::sync::Arc;

    fn blob_of(doc: &str) -> Vec<u8> {
        let sj: serde_json::Value = serde_json::from_str(doc).unwrap();
        axiomdb_types::jsonb::JsonbEncoder::encode(&sj).unwrap()
    }

    fn segs(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// Reference walk: the pre-optimization serde path (decode the whole blob,
    /// then `descend_serde`). It runs on the SAME blob as the binary navigator,
    /// so JSONB key ordering matches on both sides and every case — including
    /// object/array results serialized as text — is directly comparable.
    fn extract_via_serde(blob: &[u8], parts: &[String], as_text: bool) -> Value {
        let doc = axiomdb_types::jsonb::JsonbDecoder::decode(blob).unwrap();
        let target = match descend_serde(&doc, parts) {
            Some(v) => v.clone(),
            None => return Value::Null,
        };
        if as_text {
            match target {
                serde_json::Value::String(s) => Value::Text(s),
                other => Value::Text(other.to_string()),
            }
        } else {
            let b = axiomdb_types::jsonb::JsonbEncoder::encode(&target).unwrap();
            Value::Jsonb(Arc::new(b))
        }
    }

    fn check(doc: &str, parts: &[&str]) {
        let blob = blob_of(doc);
        let p = segs(parts);
        for as_text in [true, false] {
            assert_eq!(
                jsonb_extract_binary(&blob, &p, as_text).unwrap(),
                extract_via_serde(&blob, &p, as_text),
                "doc={doc} parts={parts:?} as_text={as_text}",
            );
        }
    }

    #[test]
    fn binary_matches_serde_reference() {
        let doc = r#"{"id":7,"active":1,"name":"alice","score":3.5,"flag":true,"none":null,"profile":{"plan":"pro","country":"US"},"tags":["web","paid"]}"#;
        // scalars of each type
        check(doc, &["id"]);
        check(doc, &["active"]);
        check(doc, &["name"]);
        check(doc, &["score"]);
        check(doc, &["flag"]);
        check(doc, &["none"]);
        // nested object + whole sub-object
        check(doc, &["profile", "plan"]);
        check(doc, &["profile", "country"]);
        check(doc, &["profile"]);
        // arrays: whole, positive index, negative index
        check(doc, &["tags"]);
        check(doc, &["tags", "0"]);
        check(doc, &["tags", "1"]);
        check(doc, &["tags", "-1"]);
        // misses
        check(doc, &["missing"]);
        check(doc, &["profile", "missing"]);
        check(doc, &["tags", "9"]);
        check(doc, &["active", "x"]); // descend into a scalar → no match
        // whole document
        check(doc, &[]);
    }

    #[test]
    fn binary_explicit_values() {
        let blob = blob_of(r#"{"active":1,"name":"alice","profile":{"plan":"pro"},"tags":["web","paid"]}"#);
        let t = |parts: &[&str]| jsonb_extract_binary(&blob, &segs(parts), true).unwrap();
        assert_eq!(t(&["active"]), Value::Text("1".into()));
        assert_eq!(t(&["name"]), Value::Text("alice".into()));
        assert_eq!(t(&["profile", "plan"]), Value::Text("pro".into()));
        assert_eq!(t(&["tags", "0"]), Value::Text("web".into()));
        assert_eq!(t(&["tags", "-1"]), Value::Text("paid".into()));
        assert_eq!(t(&["missing"]), Value::Null);
    }
}
