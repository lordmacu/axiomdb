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
        // ── Int / BigInt → Bool (MySQL: any non-zero = true) ─────────────────
        (Value::Int(n), DataType::Bool) => Ok(Value::Bool(n != 0)),
        (Value::BigInt(n), DataType::Bool) => Ok(Value::Bool(n != 0)),

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

        // ── Numeric/bool → Text (always lossless: stringification) ──────────
        (Value::Int(n), DataType::Text) => Ok(Value::Text(n.to_string())),
        (Value::BigInt(n), DataType::Text) => Ok(Value::Text(n.to_string())),
        (Value::Real(f), DataType::Text) => Ok(Value::Text(f.to_string())),
        (Value::Bool(b), DataType::Text) if mode == CoercionMode::Permissive => {
            Ok(Value::Text(if b { "1" } else { "0" }.to_string()))
        }

        // ── Text → JSON (Phase 11.4) — validate JSON syntax ─────────────────
        (Value::Text(s), DataType::Json) => {
            // Validate JSON syntax before storing.
            if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
                Ok(Value::Json(s))
            } else {
                Err(DbError::InvalidValue {
                    reason: format!("invalid JSON: {}", &s[..s.len().min(80)]),
                })
            }
        }
        // ── JSON → Text ─────────────────────────────────────────────────────
        (Value::Json(s), DataType::Text) => Ok(Value::Text(s)),

        // ── Text/Json → JSONB — encode to binary format (Phase 11.16) ───────
        (Value::Text(s) | Value::Json(s), DataType::Jsonb) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&s).map_err(|e| DbError::InvalidValue {
                    reason: format!("invalid JSON: {e}"),
                })?;
            let blob = crate::JsonbEncoder::encode(&parsed)?;
            Ok(Value::Jsonb(std::sync::Arc::new(blob)))
        }
        // ── JSONB → Text/Json — decode to JSON text ──────────────────────────
        (Value::Jsonb(blob), DataType::Text | DataType::Json) => {
            let s = crate::JsonbDecoder::to_string(&blob).map_err(|e| DbError::InvalidValue {
                reason: format!("JSONB decode: {e}"),
            })?;
            Ok(match target {
                DataType::Json => Value::Json(s),
                _ => Value::Text(s),
            })
        }
        // ── JSONB identity ───────────────────────────────────────────────────
        (v @ Value::Jsonb(_), DataType::Jsonb) => Ok(v),

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

        // Bool ↔ Int — MySQL: TRUE=1, FALSE=0 for comparisons.
        // e.g. `active = 1` works in MySQL; we follow the same rule.
        (Value::Bool(a), Value::Int(_)) => Ok((Value::Int(if *a { 1 } else { 0 }), r)),
        (Value::Int(_), Value::Bool(b)) => Ok((l, Value::Int(if *b { 1 } else { 0 }))),
        (Value::Bool(a), Value::BigInt(_)) => Ok((Value::BigInt(if *a { 1 } else { 0 }), r)),
        (Value::BigInt(_), Value::Bool(b)) => Ok((l, Value::BigInt(if *b { 1 } else { 0 }))),

        // All other pairs are not implicitly promotable.
        _ => Err(DbError::InvalidCoercion {
            from: l.variant_name().into(),
            to: r.variant_name().into(),
            value: l.to_string(),
            reason: "no implicit numeric promotion between these types; use explicit CAST".into(),
        }),
    }
}
