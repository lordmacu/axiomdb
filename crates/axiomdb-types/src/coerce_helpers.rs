// ── Private helpers ───────────────────────────────────────────────────────────

/// Returns true if `value`'s variant already matches `target`.
fn value_matches_type(value: &Value, target: DataType) -> bool {
    matches!(
        (value, target),
        (Value::Bool(_), DataType::Bool)
            | (Value::Int(_), DataType::Int)
            | (Value::BigInt(_), DataType::BigInt)
            | (Value::Real(_), DataType::Float)
            | (Value::Real(_), DataType::Real)
            | (Value::Decimal(..), DataType::Decimal)
            | (Value::Text(_), DataType::Text)
            | (Value::Json(_), DataType::Json)
            | (Value::Bytes(_), DataType::Bytes)
            | (Value::Date(_), DataType::Date)
            | (Value::Timestamp(_), DataType::Timestamp)
            | (Value::TimestampTz(_), DataType::TimestampTz)
            | (Value::Uuid(_), DataType::Uuid)
            | (Value::Array(_), DataType::Array(_))
            | (Value::Range(_), DataType::Range(_))
            | (Value::Composite(_), DataType::Composite(_))
            | (Value::Ltree(_), DataType::Ltree)
            | (Value::Xml(_), DataType::Xml)
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

fn parse_text_to_date_days(s: &str, mode: CoercionMode) -> Result<i32, DbError> {
    let trimmed = s.trim();
    let make_err = |reason: String| DbError::InvalidCoercion {
        from: "Text".into(),
        to: "DATE".into(),
        value: format!("'{s}'"),
        reason,
    };

    // Accept ISO date prefix: YYYY-MM-DD.
    // If a time part exists ("YYYY-MM-DD HH:MM:SS"), ignore it.
    let bytes = trimmed.as_bytes();
    if bytes.len() < 10 {
        return Err(make_err("expected YYYY-MM-DD".into()));
    }
    let prefix = &trimmed[..10];
    let b = prefix.as_bytes();
    if !(b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[7] == b'-'
        && b[8].is_ascii_digit()
        && b[9].is_ascii_digit())
    {
        return Err(make_err("expected YYYY-MM-DD".into()));
    }

    // Strict mode: after the 10-byte prefix, allow optional whitespace + time.
    // We don't validate the time payload here since Date truncates it.
    if mode == CoercionMode::Strict && bytes.len() > 10 {
        let rest = &trimmed[10..];
        if !rest.is_empty() {
            let first = rest.as_bytes()[0];
            if !first.is_ascii_whitespace() && first != b'T' {
                return Err(make_err("unexpected trailing characters after date".into()));
            }
        }
    }

    let year: i32 = prefix[0..4]
        .parse()
        .map_err(|_| make_err("invalid year".into()))?;
    let month: i32 = prefix[5..7]
        .parse()
        .map_err(|_| make_err("invalid month".into()))?;
    let day: i32 = prefix[8..10]
        .parse()
        .map_err(|_| make_err("invalid day".into()))?;

    ymd_to_days_checked(year, month, day).ok_or_else(|| make_err("invalid calendar date".into()))
}

fn ymd_to_days_checked(year: i32, month: i32, day: i32) -> Option<i32> {
    if !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 {
        return None;
    }
    let dim = days_in_month(year, month) as i32;
    if day > dim {
        return None;
    }

    // Howard Hinnant's civil calendar algorithm (same math as mysql/prepared.rs).
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * m as u32 + 2) / 5 + day as u32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as i64 * 146_097_i64 + doe as i64 - 719_468_i64;
    if days < i32::MIN as i64 || days > i32::MAX as i64 {
        return None;
    }
    Some(days as i32)
}

fn days_in_month(year: i32, month: i32) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            if leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

/// Parse a text string as a TIMESTAMPTZ value (microseconds since Unix epoch, UTC).
///
/// Accepted formats (same as PostgreSQL):
/// - `YYYY-MM-DD HH:MM:SS` — treated as UTC, no offset
/// - `YYYY-MM-DD HH:MM:SS.ffffff` — with fractional seconds (up to 6 digits)
/// - `YYYY-MM-DD HH:MM:SS+HH:MM` — explicit positive offset: subtract from UTC
/// - `YYYY-MM-DD HH:MM:SS-HH:MM` — explicit negative offset: add to UTC
/// - `YYYY-MM-DD HH:MM:SSZ` — Z = UTC (+00:00)
/// - `YYYY-MM-DDTHH:MM:SS...` — T separator (ISO 8601)
///
/// In Permissive mode, a bare date `YYYY-MM-DD` is accepted and treated as midnight UTC.
pub(crate) fn parse_text_to_timestamptz_micros(
    s: &str,
    mode: CoercionMode,
) -> Result<i64, DbError> {
    let trimmed = s.trim();
    let make_err = |reason: String| DbError::InvalidCoercion {
        from: "Text".into(),
        to: "TIMESTAMPTZ".into(),
        value: format!("'{s}'"),
        reason,
    };

    let bytes = trimmed.as_bytes();
    // Require at least "YYYY-MM-DD"
    if bytes.len() < 10 {
        return Err(make_err("expected YYYY-MM-DD [HH:MM:SS[.ffffff][±HH:MM|Z]]".into()));
    }

    // Parse date part: YYYY-MM-DD
    if !(bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes[4] == b'-'
        && bytes[5].is_ascii_digit()
        && bytes[6].is_ascii_digit()
        && bytes[7] == b'-'
        && bytes[8].is_ascii_digit()
        && bytes[9].is_ascii_digit())
    {
        return Err(make_err("expected YYYY-MM-DD date prefix".into()));
    }

    let year: i32 = trimmed[0..4].parse().map_err(|_| make_err("invalid year".into()))?;
    let month: i32 = trimmed[5..7].parse().map_err(|_| make_err("invalid month".into()))?;
    let day: i32 = trimmed[8..10].parse().map_err(|_| make_err("invalid day".into()))?;

    let date_days =
        ymd_to_days_checked(year, month, day).ok_or_else(|| make_err("invalid calendar date".into()))?;

    // If only date provided
    let rest = &trimmed[10..];
    if rest.is_empty() {
        if mode == CoercionMode::Strict {
            return Err(make_err("missing time part".into()));
        }
        // Permissive: midnight UTC
        return Ok(date_days as i64 * 86_400_000_000_i64);
    }

    // Separator: space or T
    let rest_bytes = rest.as_bytes();
    if rest_bytes[0] != b' ' && rest_bytes[0] != b'T' {
        return Err(make_err("expected space or T before time part".into()));
    }
    let rest = &rest[1..];
    let rest_bytes = rest.as_bytes();

    // Parse time part: HH:MM:SS
    if rest_bytes.len() < 8 {
        return Err(make_err("expected HH:MM:SS time part".into()));
    }
    if !(rest_bytes[0].is_ascii_digit()
        && rest_bytes[1].is_ascii_digit()
        && rest_bytes[2] == b':'
        && rest_bytes[3].is_ascii_digit()
        && rest_bytes[4].is_ascii_digit()
        && rest_bytes[5] == b':'
        && rest_bytes[6].is_ascii_digit()
        && rest_bytes[7].is_ascii_digit())
    {
        return Err(make_err("expected HH:MM:SS".into()));
    }

    let hour: i64 = rest[0..2].parse().map_err(|_| make_err("invalid hour".into()))?;
    let minute: i64 = rest[3..5].parse().map_err(|_| make_err("invalid minute".into()))?;
    let second: i64 = rest[6..8].parse().map_err(|_| make_err("invalid second".into()))?;

    if hour > 23 {
        return Err(make_err(format!("hour {hour} out of range 0..23")));
    }
    if minute > 59 {
        return Err(make_err(format!("minute {minute} out of range 0..59")));
    }
    if second > 59 {
        return Err(make_err(format!("second {second} out of range 0..59")));
    }

    let mut pos = 8; // current position in rest

    // Optional fractional seconds: .ffffff (up to 6 digits)
    let mut frac_micros: i64 = 0;
    if pos < rest.len() && rest_bytes[pos] == b'.' {
        pos += 1;
        let frac_start = pos;
        while pos < rest.len() && rest_bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        let frac_str = &rest[frac_start..pos];
        let frac_len = frac_str.len();
        if frac_len > 6 {
            return Err(make_err("fractional seconds > 6 digits not supported".into()));
        }
        let frac_raw: i64 = frac_str.parse().unwrap_or(0);
        // Pad to 6 digits (microseconds)
        frac_micros = frac_raw * 10i64.pow((6 - frac_len) as u32);
    }

    // Optional timezone offset: Z, +HH:MM, -HH:MM, +HHMM, -HHMM
    let mut offset_secs: i64 = 0; // seconds to subtract from local to get UTC
    if pos < rest.len() {
        let tz_byte = rest_bytes[pos];
        if tz_byte == b'Z' || tz_byte == b'z' {
            pos += 1; // UTC, offset = 0
        } else if tz_byte == b'+' || tz_byte == b'-' {
            let sign: i64 = if tz_byte == b'+' { 1 } else { -1 };
            pos += 1;
            // Remaining must be HH:MM or HHMM (4 digits mandatory)
            let tz_rest = &rest[pos..];
            let tz_bytes = tz_rest.as_bytes();
            if tz_bytes.len() < 4 || !tz_bytes[0].is_ascii_digit() || !tz_bytes[1].is_ascii_digit()
            {
                return Err(make_err("expected HH:MM or HHMM after timezone sign".into()));
            }
            let tz_h: i64 = tz_rest[0..2].parse().map_err(|_| make_err("invalid tz hour".into()))?;
            // Skip optional colon
            let colon_offset = if tz_bytes.len() > 2 && tz_bytes[2] == b':' { 1 } else { 0 };
            let min_start = 2 + colon_offset;
            if tz_bytes.len() < min_start + 2 {
                return Err(make_err("expected MM in timezone offset".into()));
            }
            let tz_m: i64 = tz_rest[min_start..min_start + 2]
                .parse()
                .map_err(|_| make_err("invalid tz minute".into()))?;
            offset_secs = sign * (tz_h * 3600 + tz_m * 60);
            pos += min_start + 2;
        }
    }

    if mode == CoercionMode::Strict && pos < rest.len() {
        return Err(make_err(format!(
            "unexpected trailing characters: '{}'",
            &rest[pos..]
        )));
    }

    // Compute UTC microseconds
    let time_micros = hour * 3_600_000_000_i64
        + minute * 60_000_000_i64
        + second * 1_000_000_i64
        + frac_micros;

    let day_micros = date_days as i64 * 86_400_000_000_i64;

    // local_micros = day_micros + time_micros
    // utc_micros = local_micros - offset_secs * 1_000_000
    day_micros
        .checked_add(time_micros)
        .and_then(|m| m.checked_sub(offset_secs * 1_000_000))
        .ok_or_else(|| make_err("timestamp value overflows i64".into()))
}
