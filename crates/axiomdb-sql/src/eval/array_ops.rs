//! Array operators — PostgreSQL-compatible SQL array operations (Phase 20.4, Step 5).
//!
//! All operators follow PG semantics:
//! - 1-indexed arrays (not 0-indexed)
//! - Out-of-bounds / negative index → NULL
//! - NULL element in comparison → NULL (UNKNOWN) result

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

/// Returns `true` if two values are SQL-null-equivalent (either is NULL).
#[inline]
fn either_null(l: &Value, r: &Value) -> bool {
    matches!(l, Value::Null) || matches!(r, Value::Null)
}

// ── Subscript ────────────────────────────────────────────────────────────────

/// `arr[index]` — 1-indexed element access.
///
/// Returns `Value::Null` when:
/// - `arr` is `NULL`
/// - `index` is `NULL`
/// - `index` is negative
/// - `index` is 0
/// - `index` exceeds the array length (OOB)
pub fn array_subscript(arr: &Value, index: &Value) -> Result<Value, DbError> {
    // NULL arr → NULL
    if matches!(arr, Value::Null) {
        return Ok(Value::Null);
    }
    // NULL index → NULL
    if matches!(index, Value::Null) {
        return Ok(Value::Null);
    }

    let Value::Array(elems) = arr else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: arr.variant_name().into(),
        });
    };

    let idx = match index {
        Value::Int(n) => *n as i64,
        Value::BigInt(n) => *n,
        other => {
            return Err(DbError::TypeMismatch {
                expected: "integer index".into(),
                got: other.variant_name().into(),
            });
        }
    };

    // PG arrays are 1-indexed: negative or 0 is always OOB
    if idx <= 0 {
        return Ok(Value::Null);
    }

    let pos = idx as usize;
    if pos > elems.len() {
        return Ok(Value::Null);
    }

    // 1-indexed → Rust 0-indexed
    Ok(elems[pos - 1].clone())
}

/// `arr[lo:hi]` — slice access returning a sub-array.
///
/// Returns `Value::Null` when:
/// - `arr` is `NULL`
/// - `lo` or `hi` is `NULL`
/// - `lo` > `hi` (empty slice → empty array, not NULL)
/// - Start is past end of array
///
/// Empty slice (lo > hi) returns an empty `Value::Array(vec![])`.
///
/// Out-of-bounds slice boundaries are clamped to array bounds.
pub fn array_slice(arr: &Value, lo: &Value, hi: &Value) -> Result<Value, DbError> {
    // NULL arr → NULL
    if matches!(arr, Value::Null) {
        return Ok(Value::Null);
    }
    // NULL bounds → NULL
    if matches!(lo, Value::Null) || matches!(hi, Value::Null) {
        return Ok(Value::Null);
    }

    let Value::Array(elems) = arr else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: arr.variant_name().into(),
        });
    };

    let lo_idx = to_i64(lo)?;
    let hi_idx = to_i64(hi)?;

    let len = elems.len() as i64;

    // PG: lo > hi → empty array (before clamping)
    if lo_idx > hi_idx {
        return Ok(Value::Array(vec![]));
    }

    // If lo exceeds array length, return empty (nothing to slice)
    if lo_idx > len {
        return Ok(Value::Array(vec![]));
    }

    // Clamp boundaries to [1, len]. PG clamps, not error.
    let start = lo_idx.clamp(1, len) as usize;
    let end = hi_idx.clamp(1, len) as usize;

    // 1-indexed → Rust 0-indexed. end is inclusive in PG slice notation.
    let rust_start = start.saturating_sub(1);
    let rust_end = end.min(elems.len());

    if rust_start >= elems.len() {
        return Ok(Value::Array(vec![]));
    }

    Ok(Value::Array(elems[rust_start..rust_end].to_vec()))
}

/// `arr[i][j]` — 2D element access via chained subscripts.
///
/// Returns `Value::Null` when any index is OOB or negative.
#[allow(dead_code)]
pub fn array_subscript_2d(arr: &Value, i: &Value, j: &Value) -> Result<Value, DbError> {
    let row = array_subscript(arr, i)?;
    if matches!(row, Value::Null) {
        return Ok(Value::Null);
    }
    array_subscript(&row, j)
}

// ── Equality ────────────────────────────────────────────────────────────────

/// `a = b` — element-by-element array equality.
///
/// Returns `Value::Null` when:
/// - Either array is `NULL`
/// - Any pair of elements is (NULL, non-NULL) → NULL
/// - All pairs equal (including NULL = NULL) → TRUE
///
/// Unlike `eval_comparison` which uses total ordering, array equality
/// uses SQL 3VL semantics: NULL = NULL is UNKNOWN (not TRUE).
pub fn array_equals(a: &Value, b: &Value) -> Result<Value, DbError> {
    // NULL propagation
    if either_null(a, b) {
        return Ok(Value::Null);
    }

    let Value::Array(a_elems) = a else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: a.variant_name().into(),
        });
    };
    let Value::Array(b_elems) = b else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: b.variant_name().into(),
        });
    };

    // Different lengths → not equal (but not NULL)
    if a_elems.len() != b_elems.len() {
        return Ok(Value::Bool(false));
    }

    for (ae, be) in a_elems.iter().zip(b_elems.iter()) {
        match (ae, be) {
            // NULL = NULL → NULL (UNKNOWN in SQL 3VL)
            (Value::Null, Value::Null) => return Ok(Value::Null),
            // NULL = anything → NULL (UNKNOWN result)
            (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
            // Nested array - recursively compare
            (Value::Array(_), Value::Array(_)) => {
                let eq = array_equals(ae, be)?;
                match eq {
                    Value::Bool(true) => {}
                    Value::Bool(false) => return Ok(Value::Bool(false)),
                    Value::Null => return Ok(Value::Null),
                    _ => {
                        return Err(DbError::TypeMismatch {
                            expected: "Bool or Null".into(),
                            got: eq.variant_name().into(),
                        })
                    }
                }
            }
            // Non-null, non-array comparison
            _ => {
                let ord = crate::eval::compare_values(ae, be)?;
                if ord != std::cmp::Ordering::Equal {
                    return Ok(Value::Bool(false));
                }
            }
        }
    }

    Ok(Value::Bool(true))
}

// ── Containment ────────────────────────────────────────────────────────────

/// `subject @> query` — true if `subject` contains all elements in `query`.
///
/// A query element is "contained" if there exists an element in `subject`
/// that is NULL-equivalent or equal to it.
///
/// Returns `Value::Null` when:
/// - Either operand is `NULL`
/// - Any element in `query` is `NULL` (cannot determine containment of NULL)
///
/// Performance: O(n*m) with early exit.
pub fn array_contains(subject: &Value, query: &Value) -> Result<Value, DbError> {
    // NULL propagation
    if either_null(subject, query) {
        return Ok(Value::Null);
    }

    let Value::Array(subj) = subject else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: subject.variant_name().into(),
        });
    };
    let Value::Array(qry) = query else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: query.variant_name().into(),
        });
    };

    for qelem in qry {
        // NULL in query → UNKNOWN (cannot determine containment)
        if matches!(qelem, Value::Null) {
            return Ok(Value::Null);
        }

        let found = subj.iter().any(|selem| {
            match (selem, qelem) {
                // NULL in subject treated as a regular element
                (Value::Null, _) => true,
                // NULL query element already handled above
                (_, Value::Null) => false,
                _ => {
                    matches!(
                        crate::eval::compare_values(selem, qelem),
                        Ok(std::cmp::Ordering::Equal)
                    )
                }
            }
        });

        if !found {
            return Ok(Value::Bool(false));
        }
    }

    Ok(Value::Bool(true))
}

/// `a <@ b` — true if `a` is contained by (is a subset of) `b`.
///
/// This is `b @> a` semantically. Implemented directly to preserve
/// symmetrical NULL handling.
pub fn array_contained_by(a: &Value, b: &Value) -> Result<Value, DbError> {
    array_contains(b, a)
}

// ── Overlap ─────────────────────────────────────────────────────────────────

/// `a && b` — true if `a` and `b` share at least one common element.
///
/// Returns `Value::Null` when:
/// - Either operand is `NULL`
/// - A shared element is found but involves a NULL on one side only
///
/// Performance: O(n*m) with early exit.
pub fn array_overlap(a: &Value, b: &Value) -> Result<Value, DbError> {
    // NULL propagation
    if either_null(a, b) {
        return Ok(Value::Null);
    }

    let Value::Array(a_elems) = a else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: a.variant_name().into(),
        });
    };
    let Value::Array(b_elems) = b else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: b.variant_name().into(),
        });
    };

    for ae in a_elems {
        for be in b_elems {
            let result = match (ae, be) {
                // NULL = NULL → TRUE (NULL shares with NULL in PG)
                (Value::Null, Value::Null) => true,
                // NULL on one side only → UNKNOWN (cannot determine)
                (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
                // Both non-null: compare
                _ => {
                    matches!(
                        crate::eval::compare_values(ae, be),
                        Ok(std::cmp::Ordering::Equal)
                    )
                }
            };
            if result {
                return Ok(Value::Bool(true));
            }
        }
    }

    Ok(Value::Bool(false))
}

// ── Concatenation ───────────────────────────────────────────────────────────

/// `a || b` — concatenate two 1D arrays.
///
/// Returns error if either operand is not a 1D array.
pub fn array_concat(a: &Value, b: &Value) -> Result<Value, DbError> {
    // NULL propagation
    if either_null(a, b) {
        return Ok(Value::Null);
    }

    let Value::Array(a_elems) = a else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: a.variant_name().into(),
        });
    };
    let Value::Array(b_elems) = b else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: b.variant_name().into(),
        });
    };

    let mut result = Vec::with_capacity(a_elems.len() + b_elems.len());
    result.extend(a_elems.iter().cloned());
    result.extend(b_elems.iter().cloned());
    Ok(Value::Array(result))
}

/// `elem || arr` or `arr || elem` — concatenate a scalar element with an array.
///
/// PG rule: scalar is promoted to a 1-element array and concatenated.
/// This is asymmetric: element+array returns array+element.
pub fn array_concat_element_to_array(arr: &Value, elem: &Value) -> Result<Value, DbError> {
    // NULL propagation
    if either_null(arr, elem) {
        return Ok(Value::Null);
    }

    let Value::Array(arr_elems) = arr else {
        return Err(DbError::TypeMismatch {
            expected: "Array".into(),
            got: arr.variant_name().into(),
        });
    };

    // Prepend element as a new first element
    let mut result = Vec::with_capacity(arr_elems.len() + 1);
    result.push(elem.clone());
    result.extend(arr_elems.iter().cloned());
    Ok(Value::Array(result))
}

// ── Helper ──────────────────────────────────────────────────────────────────

#[inline]
fn to_i64(v: &Value) -> Result<i64, DbError> {
    match v {
        Value::Int(n) => Ok(*n as i64),
        Value::BigInt(n) => Ok(*n),
        other => Err(DbError::TypeMismatch {
            expected: "integer".into(),
            got: other.variant_name().into(),
        }),
    }
}
