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
