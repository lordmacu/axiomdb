//! ANY/ALL array constructs — Phase 20.4, Step 7.
//!
//! Implements `expr = ANY(array)` and `expr = ALL(array)` with PostgreSQL-compatible
//! 3-valued logic (3VL).
//!
//! ## ANY semantics
//! - TRUE if any element comparison is TRUE
//! - FALSE if all comparisons are FALSE (no NULLs involved)
//! - NULL if: all comparisons are NULL, OR at least one NULL comparison with no TRUE
//!
//! ## ALL semantics
//! - TRUE if all element comparisons are TRUE (no NULLs involved)
//! - FALSE if any element comparison is FALSE
//! - NULL if: at least one NULL comparison with no FALSE, OR empty array

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::eval::ops::eval_binary;

/// Evaluate `expr = ANY(array)` or `expr <op> ANY(array)` for comparison operators.
/// Supports: =, <>, <, <=, >, >=.
pub(crate) fn eval_any_of(
    elem_expr: &Value,
    arr_val: &Value,
    op: crate::expr::BinaryOp,
) -> Result<Value, DbError> {
    let Value::Array(elems) = arr_val else {
        return Err(DbError::TypeMismatch {
            expected: "array".into(),
            got: arr_val.variant_name().into(),
        });
    };

    let mut saw_null = false;

    for elem in elems {
        let cmp = eval_binary(op, elem_expr.clone(), elem.clone())?;

        match cmp {
            Value::Bool(true) => return Ok(Value::Bool(true)),
            Value::Bool(false) => {
                // FALSE doesn't short-circuit ANY, continue checking
            }
            Value::Null => {
                saw_null = true;
            }
            _ => {}
        }
    }

    // No TRUE found
    if saw_null {
        Ok(Value::Null) // NULL if any NULLs were encountered
    } else {
        Ok(Value::Bool(false)) // All FALSE, no NULLs
    }
}

/// Evaluate `expr = ALL(array)` or `expr <op> ALL(array)` for comparison operators.
/// Supports: =, <>, <, <=, >, >=.
pub(crate) fn eval_all_of(
    elem_expr: &Value,
    arr_val: &Value,
    op: crate::expr::BinaryOp,
) -> Result<Value, DbError> {
    let Value::Array(elems) = arr_val else {
        return Err(DbError::TypeMismatch {
            expected: "array".into(),
            got: arr_val.variant_name().into(),
        });
    };

    // Empty array → NULL per PG
    if elems.is_empty() {
        return Ok(Value::Null);
    }

    let mut saw_null = false;

    for elem in elems {
        let cmp = eval_binary(op, elem_expr.clone(), elem.clone())?;

        match cmp {
            Value::Bool(false) => return Ok(Value::Bool(false)), // FALSE short-circuits ALL
            Value::Bool(true) => {
                // TRUE doesn't short-circuit ALL, continue checking
            }
            Value::Null => {
                saw_null = true;
            }
            _ => {}
        }
    }

    // No FALSE found
    if saw_null {
        Ok(Value::Null) // NULL if any NULLs were encountered
    } else {
        Ok(Value::Bool(true)) // All TRUE, no NULLs
    }
}

/// Evaluate `expr LIKE ANY(array)` where array contains patterns.
/// Implements 3VL: TRUE if any comparison is TRUE, NULL if any comparison is NULL
/// (with no TRUE), FALSE if all comparisons are FALSE (no NULLs).
pub(crate) fn eval_like_any(
    text: &Value,
    arr_val: &Value,
    escape_ch: Option<char>,
) -> Result<Value, DbError> {
    let Value::Array(patterns) = arr_val else {
        return Err(DbError::TypeMismatch {
            expected: "array".into(),
            got: arr_val.variant_name().into(),
        });
    };

    let Value::Text(text_str) = text else {
        return Err(DbError::TypeMismatch {
            expected: "text".into(),
            got: text.variant_name().into(),
        });
    };

    let mut saw_true = false;
    let mut saw_null = false;

    for pat_val in patterns {
        // NULL pattern → NULL result per 3VL (unknown)
        if matches!(pat_val, Value::Null) {
            saw_null = true;
            continue;
        }

        let Value::Text(pattern) = pat_val else {
            continue; // Skip non-text patterns
        };

        let matched = if let Some(ch) = escape_ch {
            crate::eval::ops::like_match_with_escape(text_str, pattern, ch)
        } else {
            crate::text_semantics::like_match_collated(
                crate::eval::context::current_eval_collation(),
                text_str,
                pattern,
            )
        };

        if matched {
            saw_true = true;
            break;
        }
    }

    if saw_true {
        Ok(Value::Bool(true))
    } else if saw_null {
        Ok(Value::Null) // NULL if any NULLs were encountered with no TRUE
    } else {
        Ok(Value::Bool(false)) // All FALSE, no NULLs
    }
}

/// Evaluate `expr LIKE ALL(array)` where array contains patterns.
/// Implements 3VL: FALSE if any comparison is FALSE, NULL if any comparison is NULL
/// (with no FALSE), TRUE if all comparisons are TRUE (no NULLs), NULL if array is empty.
pub(crate) fn eval_like_all(
    text: &Value,
    arr_val: &Value,
    escape_ch: Option<char>,
) -> Result<Value, DbError> {
    let Value::Array(patterns) = arr_val else {
        return Err(DbError::TypeMismatch {
            expected: "array".into(),
            got: arr_val.variant_name().into(),
        });
    };

    // Empty array → NULL per PG
    if patterns.is_empty() {
        return Ok(Value::Null);
    }

    let Value::Text(text_str) = text else {
        return Err(DbError::TypeMismatch {
            expected: "text".into(),
            got: text.variant_name().into(),
        });
    };

    let mut saw_false = false;
    let mut saw_null = false;

    for pat_val in patterns {
        // NULL pattern → NULL result per 3VL (unknown)
        if matches!(pat_val, Value::Null) {
            saw_null = true;
            continue;
        }

        let Value::Text(pattern) = pat_val else {
            continue; // Skip non-text patterns
        };

        let matched = if let Some(ch) = escape_ch {
            crate::eval::ops::like_match_with_escape(text_str, pattern, ch)
        } else {
            crate::text_semantics::like_match_collated(
                crate::eval::context::current_eval_collation(),
                text_str,
                pattern,
            )
        };

        if !matched {
            saw_false = true;
            break;
        }
    }

    if saw_false {
        Ok(Value::Bool(false)) // FALSE short-circuits ALL
    } else if saw_null {
        Ok(Value::Null) // NULL if any NULLs were encountered with no FALSE
    } else {
        Ok(Value::Bool(true)) // All TRUE, no NULLs
    }
}
