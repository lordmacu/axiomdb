use axiomdb_core::error::DbError;
use axiomdb_types::Value;
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

use crate::expr::Expr;

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        // ── Date/Time functions (4.19) ───────────────────────────────────────
        "now" | "current_timestamp" | "getdate" | "sysdate" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let micros = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as i64;
            Ok(Value::Timestamp(micros))
        }
        "current_date" | "curdate" | "today" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let days = (secs / 86400) as i32;
            Ok(Value::Date(days))
        }
        "unix_timestamp" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Ok(Value::BigInt(secs as i64))
        }
        "year" | "month" | "day" | "hour" | "minute" | "second" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            let ndt = match v {
                Value::Null => return Ok(Value::Null),
                Value::Timestamp(micros) => micros_to_ndt(micros),
                Value::Date(days) => days_to_ndate(days).and_time(NaiveTime::MIN),
                _ => return Ok(Value::Null),
            };
            let result = match name {
                "year" => ndt.year(),
                "month" => ndt.month() as i32,
                "day" => ndt.day() as i32,
                "hour" => ndt.hour() as i32,
                "minute" => ndt.minute() as i32,
                "second" => ndt.second() as i32,
                _ => unreachable!(),
            };
            Ok(Value::Int(result))
        }
        "datediff" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let a = crate::eval::eval(&args[0], row)?;
            let b = crate::eval::eval(&args[1], row)?;
            let days_a = match a {
                Value::Date(d) => d as i64,
                Value::Timestamp(t) => t / 86_400_000_000,
                _ => return Ok(Value::Null),
            };
            let days_b = match b {
                Value::Date(d) => d as i64,
                Value::Timestamp(t) => t / 86_400_000_000,
                _ => return Ok(Value::Null),
            };
            Ok(Value::Int((days_a - days_b) as i32))
        }

        // ── DATE_FORMAT(ts, fmt) ──────────────────────────────────────────────
        //
        // DATE_FORMAT(ts, fmt_str) → TEXT
        // Formats a date/timestamp value using MySQL-style format specifiers.
        // Returns NULL if ts or fmt is NULL or fmt is empty.
        "date_format" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let ts_val = crate::eval::eval(&args[0], row)?;
            let fmt_val = crate::eval::eval(&args[1], row)?;
            if matches!(ts_val, Value::Null) || matches!(fmt_val, Value::Null) {
                return Ok(Value::Null);
            }
            let fmt_str = match fmt_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            if fmt_str.is_empty() {
                return Ok(Value::Null);
            }
            let ndt = match ts_val {
                Value::Timestamp(micros) => micros_to_ndt(micros),
                Value::Date(days) => days_to_ndate(days).and_time(NaiveTime::MIN),
                Value::Text(ref s) => {
                    if let Some((ndt, _)) = str_to_date_inner(s, "%Y-%m-%d %H:%i:%s") {
                        ndt
                    } else if let Some((ndt, _)) = str_to_date_inner(s, "%Y-%m-%d") {
                        ndt
                    } else {
                        return Ok(Value::Null);
                    }
                }
                _ => return Ok(Value::Null),
            };
            Ok(Value::Text(date_format_str(ndt, &fmt_str)))
        }

        // ── STR_TO_DATE(str, fmt) ─────────────────────────────────────────────
        //
        // STR_TO_DATE(str, fmt) → Date | Timestamp | NULL
        // Parses a string using MySQL-style format specifiers.
        // Returns NULL on parse failure (never raises an error — MySQL behavior).
        // Returns Timestamp if the format contains time components (%H/%i/%s),
        // otherwise returns Date.
        "str_to_date" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s_val = crate::eval::eval(&args[0], row)?;
            let fmt_val = crate::eval::eval(&args[1], row)?;
            if matches!(s_val, Value::Null) || matches!(fmt_val, Value::Null) {
                return Ok(Value::Null);
            }
            let s = match s_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            let fmt_str = match fmt_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            match str_to_date_inner(&s, &fmt_str) {
                None => Ok(Value::Null),
                Some((ndt, has_time)) => {
                    // SAFETY: 1970-01-01 00:00:00 is always valid.
                    let epoch_ndt = NaiveDate::from_ymd_opt(1970, 1, 1)
                        .unwrap()
                        .and_hms_opt(0, 0, 0)
                        .unwrap();
                    if has_time {
                        let micros = (ndt - epoch_ndt).num_microseconds().unwrap_or(0);
                        Ok(Value::Timestamp(micros))
                    } else {
                        let epoch_date = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                        let days = (ndt.date() - epoch_date).num_days() as i32;
                        Ok(Value::Date(days))
                    }
                }
            }
        }

        // ── FIND_IN_SET(needle, csv_list) ─────────────────────────────────────
        //
        // FIND_IN_SET(needle, list) → INT
        // Returns the 1-indexed position of needle in the comma-separated list,
        // or 0 if not found. Comparison is case-insensitive (MySQL default).
        // Returns NULL if either argument is NULL.
        "find_in_set" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let needle_val = crate::eval::eval(&args[0], row)?;
            let list_val = crate::eval::eval(&args[1], row)?;
            if matches!(needle_val, Value::Null) || matches!(list_val, Value::Null) {
                return Ok(Value::Null);
            }
            let needle = match needle_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            let list = match list_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            Ok(Value::Int(find_in_set_inner(&needle, &list)))
        }

        _ => unreachable!("dispatcher routed unsupported datetime function"),
    }
}

// ── Date / time helpers (4.19d) ───────────────────────────────────────────────

/// Converts `Value::Timestamp(micros)` to a `NaiveDateTime` (UTC).
///
/// Uses pure NaiveDateTime arithmetic (no timezone conversion needed) so that
/// results are stable across all chrono 0.4.x versions.
fn micros_to_ndt(micros: i64) -> NaiveDateTime {
    // SAFETY: 1970-01-01 00:00:00 is always a valid NaiveDateTime.
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let secs = micros.div_euclid(1_000_000);
    let nanos = micros.rem_euclid(1_000_000) * 1_000;
    epoch
        .checked_add_signed(chrono::Duration::seconds(secs))
        .and_then(|dt| dt.checked_add_signed(chrono::Duration::nanoseconds(nanos)))
        .unwrap_or(epoch)
}

/// Converts `Value::Date(days)` (days since 1970-01-01) to `NaiveDate`.
fn days_to_ndate(days: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1)
        .unwrap()
        .checked_add_signed(chrono::Duration::days(days as i64))
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
}

/// Formats `ndt` according to MySQL-compatible format specifiers in `fmt`.
///
/// Unknown specifiers are passed through literally (`%X` → `"%X"`), matching
/// MySQL behavior. English-only month/weekday names (out-of-scope: locale).
fn date_format_str(ndt: NaiveDateTime, fmt: &str) -> String {
    const MONTH_NAMES: &[&str] = &[
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    const MONTH_ABBR: &[&str] = &[
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const WEEKDAY_NAMES: &[&str] = &[
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const WEEKDAY_ABBR: &[&str] = &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    let mut out = String::with_capacity(fmt.len() + 8);
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => out.push('%'),
            Some('Y') => out.push_str(&format!("{:04}", ndt.year())),
            Some('y') => out.push_str(&format!("{:02}", ndt.year().abs() % 100)),
            Some('m') => out.push_str(&format!("{:02}", ndt.month())),
            Some('c') => out.push_str(&format!("{}", ndt.month())),
            Some('M') => out.push_str(MONTH_NAMES[(ndt.month() - 1) as usize]),
            Some('b') => out.push_str(MONTH_ABBR[(ndt.month() - 1) as usize]),
            Some('d') => out.push_str(&format!("{:02}", ndt.day())),
            Some('e') => out.push_str(&format!("{}", ndt.day())),
            Some('H') => out.push_str(&format!("{:02}", ndt.hour())),
            Some('h') => {
                let h = ndt.hour() % 12;
                out.push_str(&format!("{:02}", if h == 0 { 12 } else { h }));
            }
            Some('i') => out.push_str(&format!("{:02}", ndt.minute())),
            Some('s') | Some('S') => out.push_str(&format!("{:02}", ndt.second())),
            Some('p') => out.push_str(if ndt.hour() < 12 { "AM" } else { "PM" }),
            Some('W') => {
                let wd = ndt.weekday().num_days_from_sunday() as usize;
                out.push_str(WEEKDAY_NAMES[wd]);
            }
            Some('a') => {
                let wd = ndt.weekday().num_days_from_sunday() as usize;
                out.push_str(WEEKDAY_ABBR[wd]);
            }
            Some('j') => out.push_str(&format!("{:03}", ndt.ordinal())),
            Some('w') => out.push_str(&format!("{}", ndt.weekday().num_days_from_sunday())),
            Some('T') => out.push_str(&format!(
                "{:02}:{:02}:{:02}",
                ndt.hour(),
                ndt.minute(),
                ndt.second()
            )),
            Some('r') => {
                let h = ndt.hour() % 12;
                let h = if h == 0 { 12 } else { h };
                let am_pm = if ndt.hour() < 12 { "AM" } else { "PM" };
                out.push_str(&format!(
                    "{:02}:{:02}:{:02} {am_pm}",
                    h,
                    ndt.minute(),
                    ndt.second()
                ));
            }
            Some('%') => out.push('%'),
            Some(x) => {
                out.push('%');
                out.push(x);
            }
        }
    }
    out
}

/// Parses string `s` according to MySQL-compatible format `fmt`.
///
/// Returns `Some((NaiveDateTime, has_time))` on success.
/// - `has_time = true` → format contained `%H`, `%h`, `%i`, or `%s`/`%S`
///   (caller should return `Value::Timestamp`)
/// - `has_time = false` → date-only format (caller should return `Value::Date`)
///
/// Returns `None` on any parse failure, matching MySQL's NULL-on-bad-input
/// behavior for STR_TO_DATE.
fn str_to_date_inner(s: &str, fmt: &str) -> Option<(NaiveDateTime, bool)> {
    let mut has_date = false;
    let mut has_time = false;
    let mut year: i32 = 1970;
    let mut month: u32 = 1;
    let mut day: u32 = 1;
    let mut hour: u32 = 0;
    let mut minute: u32 = 0;
    let mut second: u32 = 0;

    let mut rem = s;
    let mut fmt_iter = fmt.chars().peekable();

    while let Some(fc) = fmt_iter.next() {
        if fc != '%' {
            // Literal character must match the corresponding char in rem.
            let mut rem_chars = rem.chars();
            match rem_chars.next() {
                Some(sc) if sc == fc => rem = rem_chars.as_str(),
                _ => return None,
            }
            continue;
        }
        let spec = fmt_iter.next()?;
        match spec {
            'Y' => {
                let (val, rest) = take_digits(rem, 4)?;
                year = val as i32;
                rem = rest;
                has_date = true;
            }
            'y' => {
                let (val, rest) = take_digits(rem, 2)?;
                year = if val < 70 {
                    2000 + val as i32
                } else {
                    1900 + val as i32
                };
                rem = rest;
                has_date = true;
            }
            'm' | 'c' => {
                let (val, rest) = take_digits(rem, 2)?;
                month = val;
                rem = rest;
                has_date = true;
            }
            'd' | 'e' => {
                let (val, rest) = take_digits(rem, 2)?;
                day = val;
                rem = rest;
                has_date = true;
            }
            'H' | 'h' => {
                let (val, rest) = take_digits(rem, 2)?;
                hour = val;
                rem = rest;
                has_time = true;
            }
            'i' => {
                let (val, rest) = take_digits(rem, 2)?;
                minute = val;
                rem = rest;
                has_time = true;
            }
            's' | 'S' => {
                let (val, rest) = take_digits(rem, 2)?;
                second = val;
                rem = rest;
                has_time = true;
            }
            _ => {
                // Unknown specifier: skip one character in rem.
                let mut rem_chars = rem.chars();
                rem_chars.next();
                rem = rem_chars.as_str();
            }
        }
    }

    // Validate component ranges.
    if month == 0 || month > 12 {
        return None;
    }
    if day == 0 || day > 31 {
        return None;
    }
    if hour > 23 {
        return None;
    }
    if minute > 59 {
        return None;
    }
    if second > 59 {
        return None;
    }

    // chrono validates day-in-month (e.g. Feb 30 → None).
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let time = NaiveTime::from_hms_opt(hour, minute, second)?;
    let _ = has_date; // used above; suppress lint
    Some((NaiveDateTime::new(date, time), has_time))
}

/// Take up to `max` ASCII decimal digits from the start of `s`.
/// Returns `(value, remainder)` or `None` if no digit is found.
fn take_digits(s: &str, max: usize) -> Option<(u32, &str)> {
    let n = s
        .bytes()
        .take(max)
        .take_while(|b| b.is_ascii_digit())
        .count();
    if n == 0 {
        return None;
    }
    let val: u32 = s[..n].parse().ok()?;
    Some((val, &s[n..]))
}

/// Returns the 1-indexed position of `needle` in the comma-separated `list`,
/// or 0 if not found. Comparison is case-insensitive (ASCII).
fn find_in_set_inner(needle: &str, list: &str) -> i32 {
    if list.is_empty() {
        return 0;
    }
    for (i, item) in list.split(',').enumerate() {
        if item.eq_ignore_ascii_case(needle) {
            return (i + 1) as i32;
        }
    }
    0
}
