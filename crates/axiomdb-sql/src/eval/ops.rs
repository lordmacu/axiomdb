use axiomdb_core::error::DbError;
use axiomdb_types::{coerce::coerce_for_op, Value};

use crate::{
    expr::{BinaryOp, Expr, UnaryOp},
    text_semantics::compare_text,
};

use super::current_eval_collation;

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

// ── Binary evaluation ─────────────────────────────────────────────────────────

/// Evaluates a binary op on already-evaluated operands (non-AND/OR/XOR).
/// NULL propagates: if either operand is NULL, the result is NULL
/// (except `<=>` which handles NULL explicitly).
pub(super) fn eval_binary(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
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

    // NULL propagation for all other binary ops.
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            eval_arithmetic(op, l, r)
        }
        BinaryOp::IntDiv => eval_int_div(l, r),

        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => eval_comparison(op, l, r),

        BinaryOp::Concat => eval_concat(l, r),

        BinaryOp::BitAnd => Ok(Value::BigInt(value_to_i64_bits(&l) & value_to_i64_bits(&r))),
        BinaryOp::BitOr => Ok(Value::BigInt(value_to_i64_bits(&l) | value_to_i64_bits(&r))),
        BinaryOp::BitXor => Ok(Value::BigInt(value_to_i64_bits(&l) ^ value_to_i64_bits(&r))),
        BinaryOp::ShiftLeft => {
            let n = value_to_i64_bits(&l);
            let s = value_to_i64_bits(&r);
            if s < 0 || s >= 64 {
                Ok(Value::BigInt(0))
            } else {
                Ok(Value::BigInt(n << s))
            }
        }
        BinaryOp::ShiftRight => {
            let n = value_to_i64_bits(&l) as u64;
            let s = value_to_i64_bits(&r);
            if s < 0 || s >= 64 {
                Ok(Value::BigInt(0))
            } else {
                Ok(Value::BigInt((n >> s) as i64))
            }
        }

        BinaryOp::Regexp => eval_regexp(l, r),

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
        (Value::Uuid(a), Value::Uuid(b)) => Ok(a.cmp(b)),
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
            if pi < m && pi < m && pat[pi] == text[ti] {
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
