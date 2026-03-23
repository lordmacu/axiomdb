//! Type coercion — converts a [`Value`] to a target [`DataType`].
//!
//! ## Two entry points
//!
//! - [`coerce`] — converts a single `Value` to a specific `DataType` target,
//!   used by the executor on INSERT/UPDATE column assignment.
//! - [`coerce_for_op`] — widens two operands to a common numeric type for use
//!   in binary arithmetic and comparisons inside the expression evaluator.
//!
//! ## Coercion modes
//!
//! [`CoercionMode::Strict`] (AxiomDB default) rejects any `Text` value that
//! contains non-numeric characters when converting to a numeric type:
//! `'42abc'` → `INT` = [`DbError::InvalidCoercion`].
//!
//! [`CoercionMode::Permissive`] applies MySQL-compatible lenient rules:
//! `'42abc'` → `INT` = `42` (stops at first non-digit).
//! `'abc'` → `INT` = `0` (no digits consumed).
//!
//! ## NULL semantics
//!
//! [`Value::Null`] always passes through unchanged regardless of target type
//! or mode. `coerce(Null, any_target, any_mode)` always returns `Ok(Null)`.
//!
//! ## What is NOT in scope
//!
//! - `Text → Date / Timestamp` parsing (requires chrono — Phase 4.19)
//! - `Text → UUID` parsing (Phase 4.19)
//! - `Text → Bool` conversion
//! - `Real / Decimal → Int / BigInt` narrowing (requires explicit CAST — Phase 4.12b)
//! - `Real ↔ Decimal` (different precision models; explicit CAST only)

use axiomdb_core::error::DbError;

use crate::{types::DataType, value::Value};

// ── Public types ──────────────────────────────────────────────────────────────

/// Controls how [`coerce`] handles ambiguous conversions.
///
/// Use [`CoercionMode::Strict`] (the AxiomDB default) for correct behavior.
/// Use [`CoercionMode::Permissive`] when the session is in MySQL-compat mode
/// (`SET AXIOM_COMPAT = 'mysql'` — Phase 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoercionMode {
    /// AxiomDB default.
    ///
    /// `Text → numeric` requires the entire string (after trimming whitespace)
    /// to be a valid number. `'42abc'` → `INT` is an error. `'42'` → `INT` is
    /// `Int(42)`. `Bool → numeric` is always an error.
    Strict,

    /// MySQL-compatible lenient mode.
    ///
    /// `Text → numeric` parses as many leading numeric characters as possible
    /// and discards the rest. `'42abc'` → `Int(42)`. `'abc'` → `Int(0)`.
    /// `Bool → Int/BigInt/Real` succeeds: `true` → `1`, `false` → `0`.
    Permissive,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Coerce `value` to the given `target` type using `mode` rules.
///
/// Returns `value` unchanged if it is already the correct variant.
/// Returns `Ok(Value::Null)` if `value` is `Value::Null` (NULL propagates).
///
/// # Errors
///
/// - [`DbError::InvalidCoercion`] (SQLSTATE 22018) — the value cannot be
///   converted to the target type under the given mode (e.g., non-numeric text
///   in strict mode, type pairs that have no implicit conversion).
pub fn coerce(value: Value, target: DataType, mode: CoercionMode) -> Result<Value, DbError> {
    // NULL always passes through.
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }

    // Identity — already the correct type.
    if value_matches_type(&value, target) {
        return Ok(value);
    }

    match (value, target) {
        // ── Numeric widening (lossless) ───────────────────────────────────────
        (Value::Int(n), DataType::BigInt) => Ok(Value::BigInt(n as i64)),
        (Value::Int(n), DataType::Real) => Ok(Value::Real(n as f64)),
        (Value::Int(n), DataType::Decimal) => Ok(Value::Decimal(n as i128, 0)),

        (Value::BigInt(n), DataType::Real) => Ok(Value::Real(n as f64)),
        (Value::BigInt(n), DataType::Decimal) => Ok(Value::Decimal(n as i128, 0)),

        // ── Numeric narrowing (may fail) ──────────────────────────────────────
        (Value::BigInt(n), DataType::Int) => {
            if n < i32::MIN as i64 || n > i32::MAX as i64 {
                Err(DbError::InvalidCoercion {
                    from: "BigInt".into(),
                    to: "INT".into(),
                    value: n.to_string(),
                    reason: format!("value {n} overflows INT range [{}, {}]", i32::MIN, i32::MAX),
                })
            } else {
                Ok(Value::Int(n as i32))
            }
        }

        // ── Text → numeric ────────────────────────────────────────────────────
        (Value::Text(s), DataType::Int) => {
            let n = parse_text_to_bigint(&s, mode, "INT")?;
            if n < i32::MIN as i64 || n > i32::MAX as i64 {
                Err(DbError::InvalidCoercion {
                    from: "Text".into(),
                    to: "INT".into(),
                    value: format!("'{s}'"),
                    reason: format!("parsed value {n} overflows INT range"),
                })
            } else {
                Ok(Value::Int(n as i32))
            }
        }
        (Value::Text(s), DataType::BigInt) => {
            let n = parse_text_to_bigint(&s, mode, "BIGINT")?;
            Ok(Value::BigInt(n))
        }
        (Value::Text(s), DataType::Real) => {
            let f = parse_text_to_float(&s, mode, "REAL")?;
            Ok(Value::Real(f))
        }
        (Value::Text(s), DataType::Decimal) => {
            let (mantissa, scale) = parse_text_to_decimal(&s, mode)?;
            Ok(Value::Decimal(mantissa, scale))
        }

        // ── Temporal ──────────────────────────────────────────────────────────
        (Value::Date(days), DataType::Timestamp) => {
            // Convert days since epoch to microseconds since epoch (midnight UTC).
            // 86_400_000_000 µs = 86400 s × 1_000_000 µs/s
            let micros = (days as i64)
                .checked_mul(86_400_000_000_i64)
                .ok_or_else(|| DbError::InvalidCoercion {
                    from: "Date".into(),
                    to: "TIMESTAMP".into(),
                    value: days.to_string(),
                    reason: "days × 86400000000 overflows i64".into(),
                })?;
            Ok(Value::Timestamp(micros))
        }

        // ── Bool → numeric (permissive only) ──────────────────────────────────
        (Value::Bool(b), DataType::Int) if mode == CoercionMode::Permissive => {
            Ok(Value::Int(b as i32))
        }
        (Value::Bool(b), DataType::BigInt) if mode == CoercionMode::Permissive => {
            Ok(Value::BigInt(b as i64))
        }
        (Value::Bool(b), DataType::Real) if mode == CoercionMode::Permissive => {
            Ok(Value::Real(if b { 1.0 } else { 0.0 }))
        }

        // ── Everything else is an error ───────────────────────────────────────
        (value, target) => Err(DbError::InvalidCoercion {
            from: value.variant_name().into(),
            to: target.name().into(),
            value: value.to_string(),
            reason: "no implicit conversion exists between these types".into(),
        }),
    }
}

/// Promote two operands to a common numeric type for arithmetic and comparison.
///
/// This function does **not** accept a [`CoercionMode`] because operator
/// widening is always deterministic — `Text` values are never implicitly parsed
/// at expression evaluation time (only at column-assignment time via [`coerce`]).
///
/// Returns both values in the same promoted type. If the pair cannot be
/// promoted (e.g., `Text` + `Int`, `Real` + `Decimal`), returns
/// [`DbError::InvalidCoercion`].
///
/// ## Widening lattice
///
/// ```text
/// Int < BigInt < Real
///              < Decimal
/// ```
///
/// `Int` widens to `BigInt`, `Real`, or `Decimal` depending on the other
/// operand. `BigInt` widens to `Real` or `Decimal`. `Real` and `Decimal` are
/// never implicitly mixed (use explicit CAST).
pub fn coerce_for_op(l: Value, r: Value) -> Result<(Value, Value), DbError> {
    match (&l, &r) {
        // Same type — identity.
        (Value::Int(_), Value::Int(_))
        | (Value::BigInt(_), Value::BigInt(_))
        | (Value::Real(_), Value::Real(_))
        | (Value::Decimal(..), Value::Decimal(..)) => Ok((l, r)),

        // Int ↔ BigInt — both become BigInt.
        (Value::Int(a), Value::BigInt(_)) => Ok((Value::BigInt(*a as i64), r)),
        (Value::BigInt(_), Value::Int(b)) => Ok((l, Value::BigInt(*b as i64))),

        // Int / BigInt ↔ Real — both become Real.
        (Value::Int(a), Value::Real(_)) => Ok((Value::Real(*a as f64), r)),
        (Value::Real(_), Value::Int(b)) => Ok((l, Value::Real(*b as f64))),
        (Value::BigInt(a), Value::Real(_)) => Ok((Value::Real(*a as f64), r)),
        (Value::Real(_), Value::BigInt(b)) => Ok((l, Value::Real(*b as f64))),

        // Int / BigInt ↔ Decimal — integer adopts the Decimal's scale.
        //
        // Example: Int(5) + Decimal(314, 2)
        //   → Decimal(500, 2) + Decimal(314, 2) = Decimal(814, 2) = 8.14
        //
        // The integer value is multiplied by 10^scale so its magnitude is
        // expressed in the same unit as the Decimal mantissa.
        (Value::Int(a), Value::Decimal(_, s)) => {
            let scale = *s;
            let factor = 10i128.pow(scale as u32);
            let mantissa =
                (*a as i128)
                    .checked_mul(factor)
                    .ok_or_else(|| DbError::InvalidCoercion {
                        from: "Int".into(),
                        to: "DECIMAL".into(),
                        value: a.to_string(),
                        reason: "scaling Int to match Decimal precision overflows i128".into(),
                    })?;
            Ok((Value::Decimal(mantissa, scale), r))
        }
        (Value::Decimal(_, s), Value::Int(b)) => {
            let scale = *s;
            let factor = 10i128.pow(scale as u32);
            let mantissa =
                (*b as i128)
                    .checked_mul(factor)
                    .ok_or_else(|| DbError::InvalidCoercion {
                        from: "Int".into(),
                        to: "DECIMAL".into(),
                        value: b.to_string(),
                        reason: "scaling Int to match Decimal precision overflows i128".into(),
                    })?;
            Ok((l, Value::Decimal(mantissa, scale)))
        }
        (Value::BigInt(a), Value::Decimal(_, s)) => {
            let scale = *s;
            let factor = 10i128.pow(scale as u32);
            let mantissa =
                (*a as i128)
                    .checked_mul(factor)
                    .ok_or_else(|| DbError::InvalidCoercion {
                        from: "BigInt".into(),
                        to: "DECIMAL".into(),
                        value: a.to_string(),
                        reason: "scaling BigInt to match Decimal precision overflows i128".into(),
                    })?;
            Ok((Value::Decimal(mantissa, scale), r))
        }
        (Value::Decimal(_, s), Value::BigInt(b)) => {
            let scale = *s;
            let factor = 10i128.pow(scale as u32);
            let mantissa =
                (*b as i128)
                    .checked_mul(factor)
                    .ok_or_else(|| DbError::InvalidCoercion {
                        from: "BigInt".into(),
                        to: "DECIMAL".into(),
                        value: b.to_string(),
                        reason: "scaling BigInt to match Decimal precision overflows i128".into(),
                    })?;
            Ok((l, Value::Decimal(mantissa, scale)))
        }

        // All other pairs are not implicitly promotable.
        _ => Err(DbError::InvalidCoercion {
            from: l.variant_name().into(),
            to: r.variant_name().into(),
            value: l.to_string(),
            reason: "no implicit numeric promotion between these types; use explicit CAST".into(),
        }),
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Returns true if `value`'s variant already matches `target`.
fn value_matches_type(value: &Value, target: DataType) -> bool {
    matches!(
        (value, target),
        (Value::Bool(_), DataType::Bool)
            | (Value::Int(_), DataType::Int)
            | (Value::BigInt(_), DataType::BigInt)
            | (Value::Real(_), DataType::Real)
            | (Value::Decimal(..), DataType::Decimal)
            | (Value::Text(_), DataType::Text)
            | (Value::Bytes(_), DataType::Bytes)
            | (Value::Date(_), DataType::Date)
            | (Value::Timestamp(_), DataType::Timestamp)
            | (Value::Uuid(_), DataType::Uuid)
    )
}

/// Parse a text string as an integer (i64) with strict or permissive rules.
///
/// `target_type_name` is used only in error messages (e.g., `"INT"`, `"BIGINT"`).
///
/// ## Strict mode
/// The entire string (after trimming ASCII whitespace) must form a valid
/// decimal integer. Any trailing non-digit character causes an error.
///
/// ## Permissive mode (MySQL behavior)
/// Parse as many leading digit characters as possible (with optional leading
/// sign). Stop at the first non-digit. If no digits are found, return `0`.
/// Overflow still causes an error even in permissive mode.
fn parse_text_to_bigint(
    s: &str,
    mode: CoercionMode,
    target_type_name: &str,
) -> Result<i64, DbError> {
    let trimmed = s.trim();

    let make_err = |reason: String| DbError::InvalidCoercion {
        from: "Text".into(),
        to: target_type_name.into(),
        value: format!("'{s}'"),
        reason,
    };

    match mode {
        CoercionMode::Strict => trimmed
            .parse::<i64>()
            .map_err(|_| make_err(format!("'{trimmed}' is not a valid integer"))),

        CoercionMode::Permissive => {
            let bytes = trimmed.as_bytes();
            if bytes.is_empty() {
                return Ok(0);
            }

            let (negative, start) = match bytes[0] {
                b'-' => (true, 1),
                b'+' => (false, 1),
                _ => (false, 0),
            };

            // Accumulate leading digit characters.
            let digit_end = bytes[start..]
                .iter()
                .position(|b| !b.is_ascii_digit())
                .map(|p| start + p)
                .unwrap_or(bytes.len());

            let digit_slice = &trimmed[start..digit_end];

            if digit_slice.is_empty() {
                // No digits at all (e.g., "abc" or "-abc") — MySQL returns 0.
                return Ok(0);
            }

            // Parse as u64 first to detect overflow before applying sign.
            let unsigned: u64 = digit_slice
                .parse::<u64>()
                .map_err(|_| make_err(format!("numeric value in '{trimmed}' overflows i64")))?;

            if negative {
                // i64::MIN in absolute value is 9223372036854775808, which fits u64.
                if unsigned > (i64::MAX as u64) + 1 {
                    return Err(make_err(format!("value -{unsigned} overflows i64")));
                }
                // Safe: we know unsigned ≤ 2^63.
                Ok(-(unsigned as i64))
            } else {
                if unsigned > i64::MAX as u64 {
                    return Err(make_err(format!("value {unsigned} overflows i64")));
                }
                Ok(unsigned as i64)
            }
        }
    }
}

/// Parse a text string as an IEEE 754 double.
///
/// `"NaN"` and `"inf"` are rejected in both modes because `Value::Real(NaN)`
/// is forbidden (see [`Value`] docs).
///
/// Permissive mode attempts to parse a leading float prefix; returns `0.0` if
/// no valid prefix is found.
fn parse_text_to_float(
    s: &str,
    mode: CoercionMode,
    target_type_name: &str,
) -> Result<f64, DbError> {
    let trimmed = s.trim();

    let make_err = |reason: String| DbError::InvalidCoercion {
        from: "Text".into(),
        to: target_type_name.into(),
        value: format!("'{s}'"),
        reason,
    };

    let reject_special = |f: f64, src: &str| -> Result<f64, DbError> {
        if f.is_nan() {
            Err(make_err(format!(
                "'{src}' evaluates to NaN which is not allowed"
            )))
        } else if f.is_infinite() {
            Err(make_err(format!(
                "'{src}' evaluates to infinity which is not allowed"
            )))
        } else {
            Ok(f)
        }
    };

    match mode {
        CoercionMode::Strict => {
            let f: f64 = trimmed
                .parse()
                .map_err(|_| make_err(format!("'{trimmed}' is not a valid float")))?;
            reject_special(f, trimmed)
        }

        CoercionMode::Permissive => {
            // Find the longest valid float prefix.
            // A valid float matches: [-+]? [0-9]* [.]? [0-9]+ ([eE] [-+]? [0-9]+)?
            let prefix = longest_float_prefix(trimmed);
            if prefix.is_empty() {
                return Ok(0.0);
            }
            let f: f64 = prefix
                .parse()
                .map_err(|_| make_err(format!("'{prefix}' is not a valid float")))?;
            reject_special(f, prefix)
        }
    }
}

/// Find the longest prefix of `s` that forms a valid f64 literal.
///
/// Handles: optional sign, digits, optional decimal point with digits,
/// optional exponent (`e`/`E` with optional sign and digits).
/// Returns an empty string if the string does not start with a numeric prefix.
fn longest_float_prefix(s: &str) -> &str {
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut i = 0;

    // Optional sign.
    if i < n && (bytes[i] == b'-' || bytes[i] == b'+') {
        i += 1;
    }

    let digits_start = i;
    // Integer part.
    while i < n && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // Decimal point and fractional digits.
    if i < n && bytes[i] == b'.' {
        i += 1;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    // Exponent.
    if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
        let exp_start = i;
        i += 1;
        if i < n && (bytes[i] == b'-' || bytes[i] == b'+') {
            i += 1;
        }
        let exp_digits_start = i;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // If no exponent digits, backtrack before 'e'.
        if i == exp_digits_start {
            i = exp_start;
        }
    }

    // Must have consumed at least one digit.
    if i == digits_start {
        return "";
    }
    &s[..i]
}

/// Parse a text string as a `(mantissa, scale)` pair for `Value::Decimal`.
///
/// Format: `[-][integer_digits][.][fraction_digits]`
///
/// `scale` = number of fraction digits (0 if no decimal point).
/// `mantissa` = integer_part × 10^scale + fraction_part (with sign applied).
///
/// Scale is capped at 38 (maximum that fits in `Value::Decimal`'s `u8` scale
/// field). Values with more than 38 fractional digits are rejected.
///
/// Strict mode: the entire string must be consumed. Permissive mode: trailing
/// non-numeric characters are ignored.
fn parse_text_to_decimal(s: &str, mode: CoercionMode) -> Result<(i128, u8), DbError> {
    let trimmed = s.trim();

    let make_err = |reason: String| DbError::InvalidCoercion {
        from: "Text".into(),
        to: "DECIMAL".into(),
        value: format!("'{s}'"),
        reason,
    };

    let bytes = trimmed.as_bytes();
    let n = bytes.len();
    let mut i = 0;

    // Optional sign.
    let negative = if i < n && bytes[i] == b'-' {
        i += 1;
        true
    } else {
        if i < n && bytes[i] == b'+' {
            i += 1;
        }
        false
    };

    // Integer part.
    let int_start = i;
    while i < n && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = &trimmed[int_start..i];

    // Optional decimal point + fraction.
    let (frac_digits, scale) = if i < n && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &trimmed[frac_start..i];
        let scale = frac.len();
        if scale > 38 {
            return Err(make_err(format!(
                "fractional part has {scale} digits; maximum scale is 38"
            )));
        }
        (frac, scale as u8)
    } else {
        ("", 0u8)
    };

    // Strict mode: the entire string must have been consumed.
    if mode == CoercionMode::Strict && i < n {
        return Err(make_err(format!(
            "unexpected character '{}' at position {i}",
            trimmed.chars().nth(i).unwrap_or('?')
        )));
    }

    // Must have at least one digit somewhere.
    if int_digits.is_empty() && frac_digits.is_empty() {
        return Err(make_err("no numeric digits found".into()));
    }

    // Compute mantissa = int_part × 10^scale + frac_part.
    let factor = 10i128.pow(scale as u32);

    let int_part: i128 = if int_digits.is_empty() {
        0
    } else {
        int_digits
            .parse::<i128>()
            .map_err(|_| make_err(format!("integer part '{int_digits}' overflows i128")))?
    };

    let frac_part: i128 = if frac_digits.is_empty() {
        0
    } else {
        frac_digits
            .parse::<i128>()
            .map_err(|_| make_err(format!("fractional part '{frac_digits}' overflows i128")))?
    };

    let mantissa = int_part
        .checked_mul(factor)
        .and_then(|m| m.checked_add(frac_part))
        .ok_or_else(|| make_err("decimal value overflows i128".into()))?;

    Ok((if negative { -mantissa } else { mantissa }, scale))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{types::DataType, value::Value};

    // ── coerce() identity ────────────────────────────────────────────────────

    #[test]
    fn test_coerce_identity_all_types() {
        let cases: &[(Value, DataType)] = &[
            (Value::Bool(true), DataType::Bool),
            (Value::Int(42), DataType::Int),
            (Value::BigInt(99), DataType::BigInt),
            (Value::Real(1.5), DataType::Real),
            (Value::Decimal(314, 2), DataType::Decimal),
            (Value::Text("hi".into()), DataType::Text),
            (Value::Bytes(vec![1, 2]), DataType::Bytes),
            (Value::Date(100), DataType::Date),
            (Value::Timestamp(1_000), DataType::Timestamp),
            (Value::Uuid([0u8; 16]), DataType::Uuid),
        ];
        for (v, dt) in cases {
            let result = coerce(v.clone(), *dt, CoercionMode::Strict).unwrap();
            assert_eq!(result, *v, "identity failed for {dt:?}");
        }
    }

    #[test]
    fn test_coerce_null_to_any_target() {
        for target in [
            DataType::Bool,
            DataType::Int,
            DataType::BigInt,
            DataType::Real,
            DataType::Decimal,
            DataType::Text,
            DataType::Bytes,
            DataType::Date,
            DataType::Timestamp,
            DataType::Uuid,
        ] {
            let result = coerce(Value::Null, target, CoercionMode::Strict).unwrap();
            assert_eq!(
                result,
                Value::Null,
                "Null should pass through for {target:?}"
            );
        }
    }

    // ── Numeric widening ─────────────────────────────────────────────────────

    #[test]
    fn test_coerce_int_to_bigint() {
        assert_eq!(
            coerce(Value::Int(42), DataType::BigInt, CoercionMode::Strict).unwrap(),
            Value::BigInt(42)
        );
    }

    #[test]
    fn test_coerce_int_to_real() {
        assert_eq!(
            coerce(Value::Int(5), DataType::Real, CoercionMode::Strict).unwrap(),
            Value::Real(5.0)
        );
    }

    #[test]
    fn test_coerce_int_to_decimal() {
        assert_eq!(
            coerce(Value::Int(7), DataType::Decimal, CoercionMode::Strict).unwrap(),
            Value::Decimal(7, 0)
        );
    }

    #[test]
    fn test_coerce_bigint_to_real() {
        let result = coerce(
            Value::BigInt(1_000_000),
            DataType::Real,
            CoercionMode::Strict,
        )
        .unwrap();
        assert_eq!(result, Value::Real(1_000_000.0));
    }

    #[test]
    fn test_coerce_bigint_to_decimal() {
        assert_eq!(
            coerce(Value::BigInt(100), DataType::Decimal, CoercionMode::Strict).unwrap(),
            Value::Decimal(100, 0)
        );
    }

    // ── Numeric narrowing ────────────────────────────────────────────────────

    #[test]
    fn test_coerce_bigint_to_int_ok() {
        assert_eq!(
            coerce(Value::BigInt(42), DataType::Int, CoercionMode::Strict).unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn test_coerce_bigint_to_int_min() {
        assert_eq!(
            coerce(
                Value::BigInt(i32::MIN as i64),
                DataType::Int,
                CoercionMode::Strict
            )
            .unwrap(),
            Value::Int(i32::MIN)
        );
    }

    #[test]
    fn test_coerce_bigint_to_int_max() {
        assert_eq!(
            coerce(
                Value::BigInt(i32::MAX as i64),
                DataType::Int,
                CoercionMode::Strict
            )
            .unwrap(),
            Value::Int(i32::MAX)
        );
    }

    #[test]
    fn test_coerce_bigint_to_int_overflow_hi() {
        let err = coerce(
            Value::BigInt(i32::MAX as i64 + 1),
            DataType::Int,
            CoercionMode::Strict,
        )
        .unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_bigint_to_int_overflow_lo() {
        let err = coerce(
            Value::BigInt(i32::MIN as i64 - 1),
            DataType::Int,
            CoercionMode::Strict,
        )
        .unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    // ── Text → Int ───────────────────────────────────────────────────────────

    #[test]
    fn test_coerce_text_int_clean() {
        assert_eq!(
            coerce(
                Value::Text("42".into()),
                DataType::Int,
                CoercionMode::Strict
            )
            .unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn test_coerce_text_int_whitespace() {
        assert_eq!(
            coerce(
                Value::Text("  42  ".into()),
                DataType::Int,
                CoercionMode::Strict
            )
            .unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn test_coerce_text_int_negative() {
        assert_eq!(
            coerce(
                Value::Text("-7".into()),
                DataType::Int,
                CoercionMode::Strict
            )
            .unwrap(),
            Value::Int(-7)
        );
    }

    #[test]
    fn test_coerce_text_int_strict_garbage() {
        let err = coerce(
            Value::Text("42abc".into()),
            DataType::Int,
            CoercionMode::Strict,
        )
        .unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_text_int_permissive_trailing() {
        assert_eq!(
            coerce(
                Value::Text("42abc".into()),
                DataType::Int,
                CoercionMode::Permissive
            )
            .unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn test_coerce_text_int_permissive_all_garbage() {
        // MySQL behavior: no leading digits → 0.
        assert_eq!(
            coerce(
                Value::Text("abc".into()),
                DataType::Int,
                CoercionMode::Permissive
            )
            .unwrap(),
            Value::Int(0)
        );
    }

    #[test]
    fn test_coerce_text_int_overflow_into_int() {
        // "99999999999" parses to a valid i64 but overflows i32.
        let err = coerce(
            Value::Text("99999999999".into()),
            DataType::Int,
            CoercionMode::Strict,
        )
        .unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_text_bigint_large() {
        assert_eq!(
            coerce(
                Value::Text("99999999999".into()),
                DataType::BigInt,
                CoercionMode::Strict,
            )
            .unwrap(),
            Value::BigInt(99_999_999_999)
        );
    }

    // ── Text → Real ──────────────────────────────────────────────────────────

    #[test]
    fn test_coerce_text_real_ok() {
        assert_eq!(
            coerce(
                Value::Text("3.14".into()),
                DataType::Real,
                CoercionMode::Strict
            )
            .unwrap(),
            Value::Real(3.14)
        );
    }

    #[test]
    fn test_coerce_text_real_negative_exponent() {
        let v = coerce(
            Value::Text("-1.5e2".into()),
            DataType::Real,
            CoercionMode::Strict,
        )
        .unwrap();
        assert_eq!(v, Value::Real(-150.0));
    }

    #[test]
    fn test_coerce_text_real_nan_rejected() {
        let err = coerce(
            Value::Text("NaN".into()),
            DataType::Real,
            CoercionMode::Strict,
        )
        .unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_text_real_inf_rejected() {
        let err = coerce(
            Value::Text("inf".into()),
            DataType::Real,
            CoercionMode::Strict,
        )
        .unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_text_real_garbage_strict() {
        let err = coerce(
            Value::Text("3.14xyz".into()),
            DataType::Real,
            CoercionMode::Strict,
        )
        .unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_text_real_permissive() {
        let v = coerce(
            Value::Text("3.14xyz".into()),
            DataType::Real,
            CoercionMode::Permissive,
        )
        .unwrap();
        assert_eq!(v, Value::Real(3.14));
    }

    // ── Text → Decimal ───────────────────────────────────────────────────────

    #[test]
    fn test_coerce_text_decimal_fraction() {
        assert_eq!(
            coerce(
                Value::Text("3.14".into()),
                DataType::Decimal,
                CoercionMode::Strict
            )
            .unwrap(),
            Value::Decimal(314, 2)
        );
    }

    #[test]
    fn test_coerce_text_decimal_integer() {
        assert_eq!(
            coerce(
                Value::Text("100".into()),
                DataType::Decimal,
                CoercionMode::Strict
            )
            .unwrap(),
            Value::Decimal(100, 0)
        );
    }

    #[test]
    fn test_coerce_text_decimal_negative() {
        assert_eq!(
            coerce(
                Value::Text("-0.5".into()),
                DataType::Decimal,
                CoercionMode::Strict
            )
            .unwrap(),
            Value::Decimal(-5, 1)
        );
    }

    #[test]
    fn test_coerce_text_decimal_scale_too_large() {
        // 39-digit fraction: scale > 38 → error.
        let s = format!("0.{}", "1".repeat(39));
        let err = coerce(Value::Text(s), DataType::Decimal, CoercionMode::Strict).unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    // ── Date → Timestamp ─────────────────────────────────────────────────────

    #[test]
    fn test_coerce_date_epoch() {
        assert_eq!(
            coerce(Value::Date(0), DataType::Timestamp, CoercionMode::Strict).unwrap(),
            Value::Timestamp(0)
        );
    }

    #[test]
    fn test_coerce_date_one_day() {
        assert_eq!(
            coerce(Value::Date(1), DataType::Timestamp, CoercionMode::Strict).unwrap(),
            Value::Timestamp(86_400_000_000)
        );
    }

    #[test]
    fn test_coerce_date_negative() {
        assert_eq!(
            coerce(Value::Date(-1), DataType::Timestamp, CoercionMode::Strict).unwrap(),
            Value::Timestamp(-86_400_000_000)
        );
    }

    // ── Bool → numeric (permissive only) ─────────────────────────────────────

    #[test]
    fn test_coerce_bool_int_permissive() {
        assert_eq!(
            coerce(Value::Bool(true), DataType::Int, CoercionMode::Permissive).unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            coerce(Value::Bool(false), DataType::Int, CoercionMode::Permissive).unwrap(),
            Value::Int(0)
        );
    }

    #[test]
    fn test_coerce_bool_int_strict_error() {
        let err = coerce(Value::Bool(true), DataType::Int, CoercionMode::Strict).unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_bool_bigint_permissive() {
        assert_eq!(
            coerce(
                Value::Bool(true),
                DataType::BigInt,
                CoercionMode::Permissive
            )
            .unwrap(),
            Value::BigInt(1)
        );
    }

    #[test]
    fn test_coerce_bool_real_permissive() {
        assert_eq!(
            coerce(Value::Bool(false), DataType::Real, CoercionMode::Permissive).unwrap(),
            Value::Real(0.0)
        );
    }

    // ── Invalid combinations ─────────────────────────────────────────────────

    #[test]
    fn test_coerce_text_to_date_is_error() {
        // Phase 4.19 — not implemented yet.
        let err = coerce(
            Value::Text("2026-01-01".into()),
            DataType::Date,
            CoercionMode::Strict,
        )
        .unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_real_to_int_is_error() {
        // Narrowing Real→Int requires explicit CAST.
        let err = coerce(Value::Real(3.14), DataType::Int, CoercionMode::Strict).unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_int_to_text_is_error() {
        let err = coerce(Value::Int(42), DataType::Text, CoercionMode::Strict).unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    // ── coerce_for_op ────────────────────────────────────────────────────────

    #[test]
    fn test_op_same_types() {
        let cases: &[(Value, Value)] = &[
            (Value::Int(1), Value::Int(2)),
            (Value::BigInt(1), Value::BigInt(2)),
            (Value::Real(1.0), Value::Real(2.0)),
            (Value::Decimal(1, 0), Value::Decimal(2, 0)),
        ];
        for (l, r) in cases {
            let (ol, or_) = coerce_for_op(l.clone(), r.clone()).unwrap();
            assert_eq!(ol, *l);
            assert_eq!(or_, *r);
        }
    }

    #[test]
    fn test_op_int_bigint() {
        let (l, r) = coerce_for_op(Value::Int(3), Value::BigInt(10)).unwrap();
        assert_eq!(l, Value::BigInt(3));
        assert_eq!(r, Value::BigInt(10));
    }

    #[test]
    fn test_op_bigint_int_symmetric() {
        let (l, r) = coerce_for_op(Value::BigInt(10), Value::Int(3)).unwrap();
        assert_eq!(l, Value::BigInt(10));
        assert_eq!(r, Value::BigInt(3));
    }

    #[test]
    fn test_op_int_real() {
        let (l, r) = coerce_for_op(Value::Int(5), Value::Real(2.0)).unwrap();
        assert_eq!(l, Value::Real(5.0));
        assert_eq!(r, Value::Real(2.0));
    }

    #[test]
    fn test_op_bigint_real() {
        let (l, r) = coerce_for_op(Value::BigInt(100), Value::Real(1.5)).unwrap();
        assert_eq!(l, Value::Real(100.0));
        assert_eq!(r, Value::Real(1.5));
    }

    #[test]
    fn test_op_int_decimal_uses_decimal_scale() {
        // Int(5) + Decimal(314, 2): Int promoted to Decimal(500, 2).
        // 5 × 10^2 = 500
        let (l, r) = coerce_for_op(Value::Int(5), Value::Decimal(314, 2)).unwrap();
        assert_eq!(l, Value::Decimal(500, 2));
        assert_eq!(r, Value::Decimal(314, 2));
    }

    #[test]
    fn test_op_decimal_int_symmetric() {
        let (l, r) = coerce_for_op(Value::Decimal(314, 2), Value::Int(5)).unwrap();
        assert_eq!(l, Value::Decimal(314, 2));
        assert_eq!(r, Value::Decimal(500, 2));
    }

    #[test]
    fn test_op_bigint_decimal() {
        // BigInt(2) + Decimal(314, 2): BigInt(2) → Decimal(200, 2).
        let (l, r) = coerce_for_op(Value::BigInt(2), Value::Decimal(314, 2)).unwrap();
        assert_eq!(l, Value::Decimal(200, 2));
        assert_eq!(r, Value::Decimal(314, 2));
    }

    #[test]
    fn test_op_real_decimal_error() {
        // Real + Decimal: never implicit.
        let err = coerce_for_op(Value::Real(1.0), Value::Decimal(100, 0)).unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_op_text_int_error() {
        let err = coerce_for_op(Value::Text("42".into()), Value::Int(1)).unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }
}
