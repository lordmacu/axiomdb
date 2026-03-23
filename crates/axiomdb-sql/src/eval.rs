//! Expression evaluator — evaluates [`Expr`] trees against a row of [`Value`]s.
//!
//! ## NULL semantics (3-valued logic)
//!
//! SQL uses three truth values: TRUE, FALSE, and UNKNOWN. UNKNOWN is
//! represented here as [`Value::Null`]. The evaluator propagates NULL
//! according to the full SQL 3-valued logic specification:
//!
//! - Arithmetic with NULL → NULL
//! - Comparison with NULL → NULL (`NULL = NULL` is NULL, not TRUE)
//! - `IS NULL` is immune: always returns TRUE or FALSE
//! - `AND`: FALSE short-circuits (FALSE AND NULL = FALSE)
//! - `OR`: TRUE short-circuits (TRUE OR NULL = TRUE)
//! - `NOT NULL = NULL`
//! - `IN`: TRUE if match found; NULL if no match but NULL in list; FALSE otherwise
//!
//! Use [`is_truthy`] to convert a result to a Rust `bool` for row filtering.

use axiomdb_core::error::DbError;
use axiomdb_types::{coerce::coerce_for_op, Value};

use crate::expr::{BinaryOp, Expr, UnaryOp};

// ── Public API ────────────────────────────────────────────────────────────────

/// Evaluates `expr` against `row` and returns the resulting [`Value`].
///
/// `row[col_idx]` must be pre-populated by the executor for each tuple.
/// Column references must have been resolved to indices by the semantic
/// analyzer (Phase 4.18) before calling this function.
///
/// ## Errors
/// - [`DbError::DivisionByZero`] — integer or decimal division / modulo by zero.
/// - [`DbError::Overflow`] — integer arithmetic overflow.
/// - [`DbError::TypeMismatch`] — incompatible operand types.
/// - [`DbError::ColumnIndexOutOfBounds`] — `col_idx >= row.len()`.
/// - [`DbError::NotImplemented`] — function call (Phase 4.19).
pub fn eval(expr: &Expr, row: &[Value]) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),

        Expr::Column { col_idx, name: _ } => {
            row.get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: row.len(),
                })
        }

        Expr::UnaryOp { op, operand } => {
            let v = eval(operand, row)?;
            eval_unary(*op, v)
        }

        // AND and OR short-circuit BEFORE evaluating the right operand.
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => eval_and(left, right, row),

        Expr::BinaryOp {
            op: BinaryOp::Or,
            left,
            right,
        } => eval_or(left, right, row),

        Expr::BinaryOp { op, left, right } => {
            let l = eval(left, row)?;
            let r = eval(right, row)?;
            eval_binary(*op, l, r)
        }

        Expr::IsNull { expr, negated } => {
            let v = eval(expr, row)?;
            let is_null = matches!(v, Value::Null);
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }

        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval(expr, row)?;
            let lo = eval(low, row)?;
            let hi = eval(high, row)?;
            // BETWEEN low AND high  ≡  v >= low AND v <= high
            let ge = eval_binary(BinaryOp::GtEq, v.clone(), lo)?;
            let le = eval_binary(BinaryOp::LtEq, v, hi)?;
            let result = apply_and_values(ge, le);
            Ok(if *negated { apply_not(result) } else { result })
        }

        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = eval(expr, row)?;
            let p = eval(pattern, row)?;
            match (v, p) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(text), Value::Text(pat)) => {
                    let matched = like_match(&text, &pat);
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                (v, p) => Err(DbError::TypeMismatch {
                    expected: "Text LIKE Text".into(),
                    got: format!("{} LIKE {}", v.variant_name(), p.variant_name()),
                }),
            }
        }

        Expr::In {
            expr,
            list,
            negated,
        } => {
            let v = eval(expr, row)?;
            let result = eval_in(v, list, row)?;
            Ok(if *negated { apply_not(result) } else { result })
        }

        Expr::Function { name, .. } => Err(DbError::NotImplemented {
            feature: format!("function '{name}' — implemented in Phase 4.19"),
        }),

        // ── CASE WHEN ─────────────────────────────────────────────────────────
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            match operand {
                // ── Searched CASE: conditions are boolean expressions ──────────
                None => {
                    for (when_expr, then_expr) in when_thens {
                        let condition = eval(when_expr, row)?;
                        if is_truthy(&condition) {
                            return eval(then_expr, row);
                        }
                    }
                }

                // ── Simple CASE: compare base value against WHEN values ────────
                Some(base_expr) => {
                    let base_val = eval(base_expr, row)?;
                    for (val_expr, then_expr) in when_thens {
                        let val = eval(val_expr, row)?;
                        // Use eval() for NULL-safe equality and type coercion.
                        // NULL base or NULL val → UNKNOWN → is_truthy = false → no match.
                        let eq = eval(
                            &Expr::BinaryOp {
                                op: crate::expr::BinaryOp::Eq,
                                left: Box::new(Expr::Literal(base_val.clone())),
                                right: Box::new(Expr::Literal(val)),
                            },
                            &[],
                        )?;
                        if is_truthy(&eq) {
                            return eval(then_expr, row);
                        }
                    }
                }
            }

            // No WHEN branch matched — return ELSE or NULL.
            match else_result {
                Some(else_expr) => eval(else_expr, row),
                None => Ok(Value::Null),
            }
        }
    }
}

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
fn apply_and_values(l: Value, r: Value) -> Value {
    match (&l, &r) {
        (Value::Bool(false), _) | (_, Value::Bool(false)) => Value::Bool(false),
        (Value::Bool(true), Value::Bool(true)) => Value::Bool(true),
        _ => Value::Null, // NULL AND TRUE = NULL, NULL AND NULL = NULL
    }
}

/// NOT applied to an already-evaluated value.
fn apply_not(v: Value) -> Value {
    match v {
        Value::Bool(b) => Value::Bool(!b),
        Value::Null => Value::Null,
        other => other, // unreachable in well-typed expressions
    }
}

// ── Short-circuit AND / OR ────────────────────────────────────────────────────

fn eval_and(left: &Expr, right: &Expr, row: &[Value]) -> Result<Value, DbError> {
    let l = eval(left, row)?;
    match l {
        // FALSE dominates: short-circuit — do NOT evaluate right.
        Value::Bool(false) => Ok(Value::Bool(false)),
        // TRUE: result is entirely determined by right.
        Value::Bool(true) => eval(right, row),
        // NULL (UNKNOWN): must evaluate right.
        Value::Null => {
            let r = eval(right, row)?;
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

fn eval_or(left: &Expr, right: &Expr, row: &[Value]) -> Result<Value, DbError> {
    let l = eval(left, row)?;
    match l {
        // TRUE dominates: short-circuit — do NOT evaluate right.
        Value::Bool(true) => Ok(Value::Bool(true)),
        // FALSE: result is entirely determined by right.
        Value::Bool(false) => eval(right, row),
        // NULL (UNKNOWN): must evaluate right.
        Value::Null => {
            let r = eval(right, row)?;
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

fn eval_unary(op: UnaryOp, v: Value) -> Result<Value, DbError> {
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
    }
}

// ── Binary evaluation ─────────────────────────────────────────────────────────

/// Evaluates a binary op on already-evaluated operands (non-AND/OR).
/// NULL propagates: if either operand is NULL, the result is NULL.
fn eval_binary(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    // NULL propagation for all binary ops except IS NULL.
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            eval_arithmetic(op, l, r)
        }

        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => eval_comparison(op, l, r),

        BinaryOp::Concat => eval_concat(l, r),

        // AND and OR are handled in `eval` before calling here.
        BinaryOp::And | BinaryOp::Or => unreachable!("AND/OR handled in eval"),
    }
}

// ── Arithmetic ────────────────────────────────────────────────────────────────

fn eval_arithmetic(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    let (l, r) = coerce_for_op(l, r)?;
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => int_arith(op, a, b),
        (Value::BigInt(a), Value::BigInt(b)) => bigint_arith(op, a, b),
        (Value::Real(a), Value::Real(b)) => Ok(Value::Real(real_arith(op, a, b)?)),
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
            // Integer division of mantissas — scale preserved as s1 (truncation).
            // Full precision division is Phase 4.18b.
            let result = m1.checked_div(m2).ok_or(DbError::Overflow)?;
            Ok(Value::Decimal(result, s1))
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
fn compare_values(l: &Value, r: &Value) -> Result<std::cmp::Ordering, DbError> {
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
        (Value::Text(a), Value::Text(b)) => Ok(a.cmp(b)),
        (Value::Bytes(a), Value::Bytes(b)) => Ok(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Ok(a.cmp(b)),
        (Value::Timestamp(a), Value::Timestamp(b)) => Ok(a.cmp(b)),
        (Value::Uuid(a), Value::Uuid(b)) => Ok(a.cmp(b)),
        _ => Err(DbError::TypeMismatch {
            expected: "comparable types".into(),
            got: format!("{} and {}", l.variant_name(), r.variant_name()),
        }),
    }
}

// ── String concat ─────────────────────────────────────────────────────────────

fn eval_concat(l: Value, r: Value) -> Result<Value, DbError> {
    match (l, r) {
        (Value::Text(a), Value::Text(b)) => Ok(Value::Text(a + &b)),
        (l, r) => Err(DbError::TypeMismatch {
            expected: "Text || Text".into(),
            got: format!("{} || {}", l.variant_name(), r.variant_name()),
        }),
    }
}

// ── IN list ───────────────────────────────────────────────────────────────────

fn eval_in(v: Value, list: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    // NULL expr → UNKNOWN.
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }

    let mut has_null_in_list = false;

    for item_expr in list {
        let item = eval(item_expr, row)?;
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
pub(crate) fn like_match(text: &str, pattern: &str) -> bool {
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── like_match ────────────────────────────────────────────────────────────

    #[test]
    fn test_like_exact_match() {
        assert!(like_match("hello", "hello"));
        assert!(!like_match("hello", "world"));
    }

    #[test]
    fn test_like_percent_any() {
        assert!(like_match("hello", "%"));
        assert!(like_match("", "%"));
        assert!(like_match("hello", "%ello"));
        assert!(like_match("hello", "hell%"));
        assert!(like_match("hello", "%ell%"));
        assert!(like_match("hello", "%"));
        assert!(!like_match("hello", "world%"));
    }

    #[test]
    fn test_like_underscore() {
        // _ matches exactly one char
        assert!(like_match("hello", "h_llo")); // h + _ + llo
        assert!(like_match("hello", "_____")); // 5 underscores = 5 chars ✓
        assert!(like_match("hello", "h___o")); // h + 3 underscores + o = h+e+l+l+o ✓
        assert!(!like_match("hello", "h____o")); // 6 pattern positions vs 5 text chars
        assert!(!like_match("hello", "____")); // 4 underscores vs 5 chars
        assert!(!like_match("hi", "___")); // 3 underscores vs 2 chars
    }

    #[test]
    fn test_like_multiple_percent() {
        assert!(like_match("abcdef", "%b%d%"));
        assert!(like_match("abcdef", "a%f"));
        assert!(!like_match("abcdef", "%z%"));
    }

    #[test]
    fn test_like_empty_string() {
        assert!(like_match("", "%"));
        assert!(like_match("", "%%"));
        assert!(!like_match("", "_"));
        assert!(like_match("", ""));
        assert!(!like_match("a", ""));
    }

    #[test]
    fn test_like_unicode() {
        // '_' must match one Unicode char, not one byte
        assert!(like_match("こんにちは", "_んにちは"));
        assert!(like_match("こんにちは", "%にちは"));
        assert!(like_match("🦀rust", "%rust"));
    }

    // ── coerce_for_op integration ─────────────────────────────────────────────

    #[test]
    fn test_eval_int_plus_bigint() {
        // Int(1) + BigInt(999) — coerce_for_op widens Int to BigInt.
        let expr = Expr::BinaryOp {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(Value::Int(1))),
            right: Box::new(Expr::Literal(Value::BigInt(999))),
        };
        assert_eq!(eval(&expr, &[]).unwrap(), Value::BigInt(1000));
    }

    #[test]
    fn test_eval_int_eq_real() {
        // Int(5) = Real(5.0) — coerce_for_op widens Int to Real for comparison.
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Literal(Value::Int(5))),
            right: Box::new(Expr::Literal(Value::Real(5.0))),
        };
        assert_eq!(eval(&expr, &[]).unwrap(), Value::Bool(true));
    }

    #[test]
    fn test_eval_int_lt_real() {
        let expr = Expr::BinaryOp {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Literal(Value::Int(3))),
            right: Box::new(Expr::Literal(Value::Real(3.5))),
        };
        assert_eq!(eval(&expr, &[]).unwrap(), Value::Bool(true));
    }

    #[test]
    fn test_eval_int_plus_decimal() {
        // Int(2) + Decimal(314, 2) = Decimal(514, 2) = 5.14
        let expr = Expr::BinaryOp {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(Value::Int(2))),
            right: Box::new(Expr::Literal(Value::Decimal(314, 2))),
        };
        assert_eq!(eval(&expr, &[]).unwrap(), Value::Decimal(514, 2));
    }

    #[test]
    fn test_eval_incompatible_types_error() {
        let expr = Expr::BinaryOp {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(Value::Int(1))),
            right: Box::new(Expr::Literal(Value::Text("a".into()))),
        };
        assert!(eval(&expr, &[]).is_err());
    }

    // ── is_truthy ────────────────────────────────────────────────────────────

    #[test]
    fn test_is_truthy() {
        assert!(is_truthy(&Value::Bool(true)));
        assert!(!is_truthy(&Value::Bool(false)));
        assert!(!is_truthy(&Value::Null));
        assert!(!is_truthy(&Value::Int(1)));
        assert!(!is_truthy(&Value::Text("true".into())));
    }
}
