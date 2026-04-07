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

include!("coerce_types.rs");
include!("coerce_api.rs");
include!("coerce_helpers.rs");

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{types::DataType, value::Value};

    fn parse_real(s: &str) -> f64 {
        s.parse().expect("valid test f64")
    }

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
            Value::Real(parse_real("3.14"))
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
        assert_eq!(v, Value::Real(parse_real("3.14")));
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
        let err = coerce(
            Value::Real(parse_real("3.14")),
            DataType::Int,
            CoercionMode::Strict,
        )
        .unwrap_err();
        assert!(matches!(err, DbError::InvalidCoercion { .. }));
    }

    #[test]
    fn test_coerce_int_to_text() {
        // Int → Text is lossless (just stringification): always allowed.
        assert_eq!(
            coerce(Value::Int(42), DataType::Text, CoercionMode::Strict).unwrap(),
            Value::Text("42".into())
        );
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
