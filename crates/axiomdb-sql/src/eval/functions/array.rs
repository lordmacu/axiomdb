//! Array functions — PostgreSQL-compatible SQL array manipulation functions (Phase 20.4, Step 6).
//!
//! Implements 17 functions:
//! - Metadata: array_length, array_lower, array_upper, array_ndims, array_dims, cardinality
//! - Mutation: array_append, array_prepend, array_cat, array_remove, array_replace
//! - Search: array_position
//! - Conversion: array_to_string, string_to_array

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

/// Returns true if value is SQL-null-equivalent.
fn is_null(v: &Value) -> bool {
    matches!(v, Value::Null)
}

/// Extracts elements from a Value::Array, or returns None if not an array.
fn extract_array(v: &Value) -> Option<&Vec<Value>> {
    match v {
        Value::Array(elems) => Some(elems),
        _ => None,
    }
}

/// array_length(arr, dim) — returns the length of dimension dim.
/// Returns NULL if arr is NULL or dim exceeds ndim.
pub(super) fn array_length(arr: &Value, dim: &Value) -> Result<Value, DbError> {
    if is_null(arr) || is_null(dim) {
        return Ok(Value::Null);
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    let dim_idx = match dim {
        Value::Int(d) => *d as usize,
        Value::BigInt(d) => (*d).max(0) as usize,
        _ => {
            return Err(DbError::TypeMismatch {
                expected: "integer dimension".to_string(),
                got: dim.variant_name().to_string(),
            });
        }
    };

    // Compute the number of dimensions
    let ndim = compute_ndim(elems);
    if dim_idx == 0 || dim_idx > ndim {
        return Ok(Value::Null);
    }

    // Return the size of dimension dim_idx
    let dim_size = get_dim_size(elems, ndim, dim_idx);
    Ok(Value::Int(dim_size))
}

/// Computes the number of dimensions from a Value::Array.
fn compute_ndim(elems: &[Value]) -> usize {
    if elems.is_empty() {
        return 0;
    }
    if elems.iter().all(|e| matches!(e, Value::Array(_))) {
        // Multi-dimensional
        let first_inner = &elems[0];
        if let Value::Array(inner) = first_inner {
            return 1 + compute_ndim(inner);
        }
        return 1;
    }
    1
}

/// Gets the size of dimension `dim_idx` (1-indexed) for an array.
fn get_dim_size(elems: &[Value], ndim: usize, dim_idx: usize) -> i32 {
    if elems.is_empty() || dim_idx > ndim {
        return 0;
    }
    if ndim == 1 {
        return elems.len() as i32;
    }
    // For multi-dimensional, recursively get the dimension
    if let Value::Array(first) = &elems[0] {
        return get_dim_size(first, ndim - 1, dim_idx);
    }
    elems.len() as i32
}

/// array_lower(arr, dim) — returns the lower bound of dimension dim.
/// PostgreSQL default lbound is 1.
pub(super) fn array_lower(arr: &Value, dim: &Value) -> Result<Value, DbError> {
    if is_null(arr) || is_null(dim) {
        return Ok(Value::Null);
    }
    let _elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    let dim_idx = match dim {
        Value::Int(d) => *d as usize,
        Value::BigInt(d) => (*d).max(0) as usize,
        _ => {
            return Err(DbError::TypeMismatch {
                expected: "integer dimension".to_string(),
                got: dim.variant_name().to_string(),
            });
        }
    };

    if dim_idx == 0 {
        return Ok(Value::Null);
    }

    // PG arrays default to lbound=1
    Ok(Value::Int(1))
}

/// array_upper(arr, dim) — returns the upper bound of dimension dim.
/// Upper = lbound + dim_size - 1. For default lbound=1, this equals element count.
pub(super) fn array_upper(arr: &Value, dim: &Value) -> Result<Value, DbError> {
    if is_null(arr) || is_null(dim) {
        return Ok(Value::Null);
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    let dim_idx = match dim {
        Value::Int(d) => *d as usize,
        Value::BigInt(d) => (*d).max(0) as usize,
        _ => {
            return Err(DbError::TypeMismatch {
                expected: "integer dimension".to_string(),
                got: dim.variant_name().to_string(),
            });
        }
    };

    if dim_idx == 0 || dim_idx > 1 {
        // Only 1D arrays supported for now; dim > 1 is out of bounds
        return Ok(Value::Null);
    }

    // PG upper = lbound + n - 1. With lbound=1, this is just n.
    Ok(Value::Int(elems.len() as i32))
}

/// array_ndims(arr) — returns the number of dimensions.
/// Returns 0 for an empty array.
pub(super) fn array_ndims(arr: &Value) -> Result<Value, DbError> {
    if is_null(arr) {
        return Ok(Value::Null);
    }
    match arr {
        Value::Array(elems) => {
            let ndim = compute_ndim(elems);
            Ok(Value::Int(ndim as i32))
        }
        _ => Err(DbError::TypeMismatch {
            expected: "array".to_string(),
            got: arr.variant_name().to_string(),
        }),
    }
}

/// array_dims(arr) — returns a text representation of array dimensions.
/// Format: '[1:3]' for 1D, '[1:2][1:3]' for 2D, etc.
pub(super) fn array_dims(arr: &Value) -> Result<Value, DbError> {
    if is_null(arr) {
        return Ok(Value::Null);
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    if elems.is_empty() {
        return Ok(Value::Text("{}".to_string()));
    }

    // Check if multi-dimensional
    if elems.iter().all(|e| matches!(e, Value::Array(_))) {
        // 2D array
        let first_inner = &elems[0];
        if let Value::Array(inner) = first_inner {
            let nrows = elems.len();
            let ncols = inner.len();
            Ok(Value::Text(format!("[1:{}][1:{}]", nrows, ncols)))
        } else {
            Ok(Value::Text(format!("[1:{}]", elems.len())))
        }
    } else {
        // 1D array
        Ok(Value::Text(format!("[1:{}]", elems.len())))
    }
}

/// cardinality(arr) — returns the total number of elements in the array.
pub(super) fn cardinality(arr: &Value) -> Result<Value, DbError> {
    if is_null(arr) {
        return Ok(Value::Null);
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    // Count all elements recursively for multi-dimensional arrays
    fn count_elements(elems: &[Value]) -> i64 {
        let mut count = 0i64;
        for e in elems {
            match e {
                Value::Array(inner) => count += count_elements(inner),
                _ => count += 1,
            }
        }
        count
    }

    let total = count_elements(elems);
    Ok(Value::BigInt(total))
}

/// array_append(arr, elem) — appends elem to the end of a 1D array.
pub(super) fn array_append(arr: &Value, elem: &Value) -> Result<Value, DbError> {
    if is_null(arr) {
        return Ok(Value::Null);
    }
    if is_null(elem) {
        // Appending NULL is allowed; just append Value::Null
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    // Only support 1D arrays
    if elems.iter().any(|e| matches!(e, Value::Array(_))) {
        return Err(DbError::InvalidValue {
            reason: "array_append only supports 1D arrays".to_string(),
        });
    }

    let mut new_elems = elems.clone();
    new_elems.push(elem.clone());
    Ok(Value::Array(new_elems))
}

/// array_prepend(elem, arr) — inserts elem at the front of a 1D array.
pub(super) fn array_prepend(elem: &Value, arr: &Value) -> Result<Value, DbError> {
    if is_null(arr) {
        return Ok(Value::Null);
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    // Only support 1D arrays
    if elems.iter().any(|e| matches!(e, Value::Array(_))) {
        return Err(DbError::InvalidValue {
            reason: "array_prepend only supports 1D arrays".to_string(),
        });
    }

    let mut new_elems = vec![elem.clone()];
    new_elems.extend(elems.clone());
    Ok(Value::Array(new_elems))
}

/// array_cat(arr1, arr2) — concatenates two 1D arrays.
pub(super) fn array_cat(arr1: &Value, arr2: &Value) -> Result<Value, DbError> {
    if is_null(arr1) && is_null(arr2) {
        return Ok(Value::Null);
    }
    if is_null(arr1) {
        return Ok(arr2.clone());
    }
    if is_null(arr2) {
        return Ok(arr1.clone());
    }
    let elems1 = extract_array(arr1).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr1.variant_name().to_string(),
    })?;
    let elems2 = extract_array(arr2).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr2.variant_name().to_string(),
    })?;

    // Only support 1D arrays
    if elems1.iter().any(|e| matches!(e, Value::Array(_)))
        || elems2.iter().any(|e| matches!(e, Value::Array(_)))
    {
        return Err(DbError::InvalidValue {
            reason: "array_cat only supports 1D arrays".to_string(),
        });
    }

    let mut new_elems = elems1.clone();
    new_elems.extend(elems2.clone());
    Ok(Value::Array(new_elems))
}

/// array_remove(arr, elem) — removes all elements equal to the given value from a 1D array.
/// Special case: if elem is NULL, removes elements that ARE NULL (not equals, since NULL=NULL is UNKNOWN).
pub(super) fn array_remove(arr: &Value, elem: &Value) -> Result<Value, DbError> {
    if is_null(arr) {
        return Ok(Value::Null);
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    // Only support 1D arrays
    if elems.iter().any(|e| matches!(e, Value::Array(_))) {
        return Err(DbError::InvalidValue {
            reason: "array_remove only supports 1D arrays".to_string(),
        });
    }

    // Special case: if elem is NULL, remove elements that ARE NULL
    let new_elems: Vec<Value> = if is_null(elem) {
        elems
            .iter()
            .filter(|e| !matches!(e, Value::Null))
            .cloned()
            .collect()
    } else {
        elems
            .iter()
            .filter(|e| !values_equal(e, elem))
            .cloned()
            .collect()
    };
    Ok(Value::Array(new_elems))
}

/// values_equal — SQL equality comparison (handles NULL = NULL as false).
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Int(a), Value::BigInt(b)) => *a as i64 == *b,
        (Value::BigInt(a), Value::Int(b)) => *a == *b as i64,
        (Value::BigInt(a), Value::BigInt(b)) => a == b,
        (Value::Real(a), Value::Real(b)) => a == b,
        (Value::Text(a), Value::Text(b)) => a == b,
        (Value::Date(a), Value::Date(b)) => a == b,
        (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
        (Value::Uuid(a), Value::Uuid(b)) => a == b,
        _ => false,
    }
}

/// array_replace(arr, old, new) — replaces all occurrences of old with new in a 1D array.
pub(super) fn array_replace(arr: &Value, old: &Value, new: &Value) -> Result<Value, DbError> {
    if is_null(arr) {
        return Ok(Value::Null);
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    // Only support 1D arrays
    if elems.iter().any(|e| matches!(e, Value::Array(_))) {
        return Err(DbError::InvalidValue {
            reason: "array_replace only supports 1D arrays".to_string(),
        });
    }

    let new_elems: Vec<Value> = elems
        .iter()
        .map(|e| {
            if values_equal(e, old) {
                new.clone()
            } else {
                e.clone()
            }
        })
        .collect();
    Ok(Value::Array(new_elems))
}

/// array_position(arr, elem) / array_position(arr, elem, start)
/// Returns the 1-indexed position of elem in arr, or 0 if not found.
/// Start index is 1-indexed (PostgreSQL behavior).
pub(super) fn array_position(
    arr: &Value,
    elem: &Value,
    start: Option<&Value>,
) -> Result<Value, DbError> {
    if is_null(arr) {
        return Ok(Value::Null);
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    // Only support 1D arrays
    if elems.iter().any(|e| matches!(e, Value::Array(_))) {
        return Err(DbError::InvalidValue {
            reason: "array_position only supports 1D arrays".to_string(),
        });
    }

    let start_idx = if let Some(s) = start {
        if is_null(s) {
            0 // NULL start means search from beginning
        } else {
            match s {
                Value::Int(n) => (*n).max(0) as usize,
                Value::BigInt(n) => (*n).max(0) as usize,
                _ => 0,
            }
        }
    } else {
        0 // No start specified, begin at index 0
    };

    for (i, e) in elems.iter().enumerate().skip(start_idx) {
        if values_equal(e, elem) {
            return Ok(Value::Int((i + 1) as i32)); // 1-indexed
        }
    }
    Ok(Value::Int(0)) // Not found
}

/// array_to_string(arr, delim) / array_to_string(arr, delim, null_str)
/// Joins array elements with delimiter. Null elements are skipped or replaced.
pub(super) fn array_to_string(
    arr: &Value,
    delim: &Value,
    null_str: Option<&Value>,
) -> Result<Value, DbError> {
    if is_null(arr) {
        return Ok(Value::Null);
    }
    if is_null(delim) {
        return Ok(Value::Null);
    }
    let elems = extract_array(arr).ok_or_else(|| DbError::TypeMismatch {
        expected: "array".to_string(),
        got: arr.variant_name().to_string(),
    })?;

    let delimiter = match delim {
        Value::Text(s) => s.clone(),
        Value::Json(s) => s.clone(),
        _ => {
            return Err(DbError::TypeMismatch {
                expected: "text delimiter".to_string(),
                got: delim.variant_name().to_string(),
            });
        }
    };

    let null_replace = null_str.and_then(|s| match s {
        Value::Text(t) => Some(t.clone()),
        Value::Json(t) => Some(t.clone()),
        _ => None,
    });

    let mut parts: Vec<String> = vec![];
    for e in elems {
        match e {
            Value::Null => {
                if let Some(ref replacement) = null_replace {
                    parts.push(replacement.clone());
                }
                // else: skip null
            }
            Value::Text(s) => parts.push(s.clone()),
            Value::Json(s) => parts.push(s.clone()),
            Value::Int(n) => parts.push(n.to_string()),
            Value::BigInt(n) => parts.push(n.to_string()),
            Value::Real(f) => parts.push(f.to_string()),
            Value::Bool(b) => parts.push(if *b { "t".to_string() } else { "f".to_string() }),
            Value::Date(d) => parts.push(d.to_string()),
            Value::Timestamp(t) => parts.push(t.to_string()),
            Value::Uuid(u) => parts.push(format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
            )),
            _ => {
                return Err(DbError::InvalidValue {
                    reason: format!("cannot convert {} to text in array_to_string", e.variant_name()),
                });
            }
        }
    }

    Ok(Value::Text(parts.join(&delimiter)))
}

/// string_to_array(str, delim) / string_to_array(str, delim, null_str)
/// Splits a string by delimiter and returns a text[] array.
/// If null_str is provided, occurrences of it are replaced with NULL elements.
pub(super) fn string_to_array(
    str_val: &Value,
    delim: &Value,
    null_str: Option<&Value>,
) -> Result<Value, DbError> {
    if is_null(str_val) {
        return Ok(Value::Null);
    }
    if is_null(delim) {
        return Err(DbError::InvalidValue {
            reason: "delimiter cannot be NULL in string_to_array".to_string(),
        });
    }

    let input = match str_val {
        Value::Text(s) => s.clone(),
        Value::Json(s) => s.clone(),
        _ => {
            return Err(DbError::TypeMismatch {
                expected: "text input".to_string(),
                got: str_val.variant_name().to_string(),
            });
        }
    };

    let delimiter = match delim {
        Value::Text(s) => s.clone(),
        Value::Json(s) => s.clone(),
        _ => {
            return Err(DbError::TypeMismatch {
                expected: "text delimiter".to_string(),
                got: delim.variant_name().to_string(),
            });
        }
    };

    let null_marker = null_str.and_then(|s| match s {
        Value::Text(t) => Some(t.clone()),
        Value::Json(t) => Some(t.clone()),
        _ => None,
    });

    // PostgreSQL string_to_array: split by delimiter
    // Empty string returns single-element array with that string
    // If delimiter not found, returns single-element array with the whole string
    let parts: Vec<String> = if delimiter.is_empty() {
        // Empty delimiter: each character becomes an element
        input.chars().map(|c| c.to_string()).collect()
    } else {
        input.split(&delimiter).map(|s| s.to_string()).collect()
    };

    let elems: Vec<Value> = parts
        .iter()
        .map(|s| {
            if let Some(ref marker) = null_marker {
                if *s == *marker {
                    return Value::Null;
                }
            }
            Value::Text(s.to_string())
        })
        .collect();

    Ok(Value::Array(elems))
}

/// unnest(arr) — returns a set of rows, one per array element.
/// This is a simple scalar eval path; full SRF handling is in Step 7.
pub(super) fn unnest(arr: &Value) -> Result<Value, DbError> {
    // Just return the array as-is; the SRF path handles row expansion
    if is_null(arr) {
        return Ok(Value::Null);
    }
    Ok(arr.clone())
}

// ── Main dispatcher ──────────────────────────────────────────────────────────

/// Evaluates an array function by name.
pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    // Evaluate all arguments first
    let mut arg_values: Vec<Value> = vec![];
    for arg in args {
        arg_values.push(crate::eval::eval(arg, row)?);
    }

    match name {
        "array_length" => {
            if arg_values.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "array_length(arr, dim): 2 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_length(&arg_values[0], &arg_values[1])
        }
        "array_lower" => {
            if arg_values.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "array_lower(arr, dim): 2 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_lower(&arg_values[0], &arg_values[1])
        }
        "array_upper" => {
            if arg_values.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "array_upper(arr, dim): 2 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_upper(&arg_values[0], &arg_values[1])
        }
        "array_ndims" => {
            if arg_values.len() != 1 {
                return Err(DbError::TypeMismatch {
                    expected: "array_ndims(arr): 1 argument".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_ndims(&arg_values[0])
        }
        "array_dims" => {
            if arg_values.len() != 1 {
                return Err(DbError::TypeMismatch {
                    expected: "array_dims(arr): 1 argument".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_dims(&arg_values[0])
        }
        "cardinality" => {
            if arg_values.len() != 1 {
                return Err(DbError::TypeMismatch {
                    expected: "cardinality(arr): 1 argument".into(),
                    got: arg_values.len().to_string(),
                });
            }
            cardinality(&arg_values[0])
        }
        "array_append" => {
            if arg_values.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "array_append(arr, elem): 2 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_append(&arg_values[0], &arg_values[1])
        }
        "array_prepend" => {
            if arg_values.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "array_prepend(elem, arr): 2 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_prepend(&arg_values[0], &arg_values[1])
        }
        "array_cat" => {
            if arg_values.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "array_cat(arr1, arr2): 2 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_cat(&arg_values[0], &arg_values[1])
        }
        "array_remove" => {
            if arg_values.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "array_remove(arr, elem): 2 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_remove(&arg_values[0], &arg_values[1])
        }
        "array_replace" => {
            if arg_values.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "array_replace(arr, old, new): 3 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            array_replace(&arg_values[0], &arg_values[1], &arg_values[2])
        }
        "array_position" => {
            if arg_values.len() != 2 && arg_values.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "array_position(arr, elem[, start]): 2-3 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            let start = if arg_values.len() == 3 {
                Some(&arg_values[2])
            } else {
                None
            };
            array_position(&arg_values[0], &arg_values[1], start)
        }
        "array_to_string" => {
            if arg_values.len() != 2 && arg_values.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "array_to_string(arr, delim[, null_str]): 2-3 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            let null_str = if arg_values.len() == 3 {
                Some(&arg_values[2])
            } else {
                None
            };
            array_to_string(&arg_values[0], &arg_values[1], null_str)
        }
        "string_to_array" => {
            if arg_values.len() != 2 && arg_values.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "string_to_array(str, delim[, null_str]): 2-3 arguments".into(),
                    got: arg_values.len().to_string(),
                });
            }
            let null_str = if arg_values.len() == 3 {
                Some(&arg_values[2])
            } else {
                None
            };
            string_to_array(&arg_values[0], &arg_values[1], null_str)
        }
        "unnest" => {
            if arg_values.len() != 1 {
                return Err(DbError::TypeMismatch {
                    expected: "unnest(arr): 1 argument".into(),
                    got: arg_values.len().to_string(),
                });
            }
            unnest(&arg_values[0])
        }
        _ => Err(DbError::NotImplemented {
            feature: format!("array function '{}'", name),
        }),
    }
}
