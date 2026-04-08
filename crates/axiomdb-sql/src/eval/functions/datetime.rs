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
            if args.is_empty() {
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                Ok(Value::BigInt(secs as i64))
            } else {
                // UNIX_TIMESTAMP(date_expr) → seconds since epoch
                let v = crate::eval::eval(&args[0], row)?;
                match v {
                    Value::Null => Ok(Value::Null),
                    Value::Timestamp(micros) => Ok(Value::BigInt(micros / 1_000_000)),
                    Value::Date(days) => Ok(Value::BigInt(days as i64 * 86_400)),
                    _ => Ok(Value::Null),
                }
            }
        }

        // ── UTC date/time functions (4.19i) ──────────────────────────────────
        "utc_timestamp" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let micros = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as i64;
            Ok(Value::Timestamp(micros))
        }
        "utc_date" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let days = (secs / 86400) as i32;
            Ok(Value::Date(days))
        }
        "utc_time" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let time_secs = secs % 86400;
            let hh = time_secs / 3600;
            let mm = (time_secs % 3600) / 60;
            let ss = time_secs % 60;
            Ok(Value::Text(format!("{:02}:{:02}:{:02}", hh, mm, ss)))
        }

        // ── FROM_UNIXTIME(epoch [, fmt]) (4.19i) ────────────────────────────
        "from_unixtime" => {
            if args.is_empty() {
                return Err(DbError::TypeMismatch {
                    expected: "1+ args".into(),
                    got: "0".into(),
                });
            }
            let epoch_val = crate::eval::eval(&args[0], row)?;
            let epoch_secs = match epoch_val {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n as i64,
                Value::BigInt(n) => n,
                Value::Real(f) => f as i64,
                _ => return Ok(Value::Null),
            };
            let micros = epoch_secs * 1_000_000;
            if args.len() == 1 {
                Ok(Value::Timestamp(micros))
            } else {
                // FROM_UNIXTIME(epoch, fmt) → formatted string
                let fmt_val = crate::eval::eval(&args[1], row)?;
                let fmt_str = match fmt_val {
                    Value::Text(s) => s,
                    _ => return Ok(Value::Null),
                };
                let ndt = micros_to_ndt(micros);
                Ok(Value::Text(date_format_str(ndt, &fmt_str)))
            }
        }

        // ── CONVERT_TZ(ts, from_tz, to_tz) — stub: returns ts unchanged ────
        "convert_tz" => {
            if args.is_empty() {
                return Ok(Value::Null);
            }
            crate::eval::eval(&args[0], row)
        }

        // ── ADDDATE / DATE_ADD / SUBDATE / DATE_SUB (4.19i) ─────────────────
        "adddate" | "date_add" | "subdate" | "date_sub" => {
            if args.len() < 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let ts_val = crate::eval::eval(&args[0], row)?;
            let delta_val = crate::eval::eval(&args[1], row)?;
            let delta_days = match delta_val {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n as i64,
                Value::BigInt(n) => n,
                _ => return Ok(Value::Null),
            };
            let sign: i64 = if name.starts_with("sub") || name == "date_sub" {
                -1
            } else {
                1
            };
            match ts_val {
                Value::Null => Ok(Value::Null),
                Value::Date(d) => Ok(Value::Date((d as i64 + sign * delta_days) as i32)),
                Value::Timestamp(us) => {
                    Ok(Value::Timestamp(us + sign * delta_days * 86_400_000_000))
                }
                _ => Ok(Value::Null),
            }
        }

        // ── TIMESTAMPDIFF(unit, ts1, ts2) — integer difference in unit ──────
        "timestampdiff" => {
            // MySQL: TIMESTAMPDIFF(SECOND, ts1, ts2) → ts2 - ts1 in seconds
            // We receive unit as a column-name expression (Ident), not a string.
            // Eval all 3 args; first arg is the unit identifier, returned as Text.
            if args.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let unit_val = crate::eval::eval(&args[0], row)?;
            let ts1_val = crate::eval::eval(&args[1], row)?;
            let ts2_val = crate::eval::eval(&args[2], row)?;
            let unit_str = match &unit_val {
                Value::Text(s) => s.to_ascii_uppercase(),
                _ => return Ok(Value::Null),
            };
            fn to_micros(v: &Value) -> Option<i64> {
                match v {
                    Value::Timestamp(us) => Some(*us),
                    Value::Date(d) => Some(*d as i64 * 86_400_000_000),
                    Value::Text(s) => {
                        if let Some((ndt, _)) = str_to_date_inner(s, "%Y-%m-%d %H:%i:%s") {
                            Some(ndt.and_utc().timestamp_micros())
                        } else if let Some((ndt, _)) = str_to_date_inner(s, "%Y-%m-%d") {
                            Some(ndt.and_utc().timestamp_micros())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            match (to_micros(&ts1_val), to_micros(&ts2_val)) {
                (Some(a), Some(b)) => {
                    let diff_us = b - a;
                    let result = match unit_str.as_str() {
                        "MICROSECOND" => diff_us,
                        "SECOND" => diff_us / 1_000_000,
                        "MINUTE" => diff_us / 60_000_000,
                        "HOUR" => diff_us / 3_600_000_000,
                        "DAY" => diff_us / 86_400_000_000,
                        "WEEK" => diff_us / (7 * 86_400_000_000),
                        "MONTH" => {
                            let ndt1 = micros_to_ndt(a);
                            let ndt2 = micros_to_ndt(b);
                            ((ndt2.year() - ndt1.year()) * 12
                                + (ndt2.month() as i32 - ndt1.month() as i32))
                                as i64
                        }
                        "YEAR" => {
                            let ndt1 = micros_to_ndt(a);
                            let ndt2 = micros_to_ndt(b);
                            (ndt2.year() - ndt1.year()) as i64
                        }
                        _ => diff_us / 1_000_000,
                    };
                    Ok(Value::BigInt(result))
                }
                _ => Ok(Value::Null),
            }
        }

        // ── MAKEDATE(year, dayofyear) ────────────────────────────────────────
        "makedate" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let y = crate::eval::eval(&args[0], row)?;
            let d = crate::eval::eval(&args[1], row)?;
            match (y, d) {
                (Value::Int(yr), Value::Int(doy)) => {
                    if let Some(date) = NaiveDate::from_yo_opt(yr, doy as u32) {
                        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                        Ok(Value::Date((date - epoch).num_days() as i32))
                    } else {
                        Ok(Value::Null)
                    }
                }
                _ => Ok(Value::Null),
            }
        }

        // ── DAYOFWEEK / DAYOFMONTH / DAYOFYEAR / WEEKDAY / WEEK / QUARTER ───
        "dayofweek" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match coerce_to_ndate(&v) {
                Some(d) => Ok(Value::Int(d.weekday().num_days_from_sunday() as i32 + 1)),
                None => Ok(Value::Null),
            }
        }
        "dayofmonth" | "day_of_month" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Date(d) => Ok(Value::Int(days_to_ndate(d).day() as i32)),
                Value::Timestamp(us) => Ok(Value::Int(micros_to_ndt(us).day() as i32)),
                _ => Ok(Value::Null),
            }
        }
        "dayofyear" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Date(d) => Ok(Value::Int(days_to_ndate(d).ordinal() as i32)),
                Value::Timestamp(us) => Ok(Value::Int(micros_to_ndt(us).ordinal() as i32)),
                _ => Ok(Value::Null),
            }
        }
        "weekday" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Date(d) => Ok(Value::Int(
                    days_to_ndate(d).weekday().num_days_from_monday() as i32,
                )),
                Value::Timestamp(us) => Ok(Value::Int(
                    micros_to_ndt(us).weekday().num_days_from_monday() as i32,
                )),
                _ => Ok(Value::Null),
            }
        }
        "quarter" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match coerce_to_ndate(&v) {
                Some(d) => Ok(Value::Int(((d.month() as i32 - 1) / 3) + 1)),
                None => Ok(Value::Null),
            }
        }
        "week" | "weekofyear" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Date(d) => Ok(Value::Int(days_to_ndate(d).iso_week().week() as i32)),
                Value::Timestamp(us) => Ok(Value::Int(micros_to_ndt(us).iso_week().week() as i32)),
                _ => Ok(Value::Null),
            }
        }
        "yearweek" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Date(d) => {
                    let nd = days_to_ndate(d);
                    Ok(Value::Int(
                        nd.iso_week().year() * 100 + nd.iso_week().week() as i32,
                    ))
                }
                Value::Timestamp(us) => {
                    let nd = micros_to_ndt(us);
                    Ok(Value::Int(
                        nd.iso_week().year() * 100 + nd.iso_week().week() as i32,
                    ))
                }
                _ => Ok(Value::Null),
            }
        }
        "last_day" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            let nd = match v {
                Value::Null => return Ok(Value::Null),
                Value::Date(d) => days_to_ndate(d),
                Value::Timestamp(us) => micros_to_ndt(us).date(),
                _ => return Ok(Value::Null),
            };
            // last day of month: go to first day of next month, subtract 1
            let next_month = if nd.month() == 12 {
                NaiveDate::from_ymd_opt(nd.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(nd.year(), nd.month() + 1, 1)
            };
            match next_month {
                Some(nm) => {
                    let last = nm - chrono::Duration::days(1);
                    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                    Ok(Value::Date((last - epoch).num_days() as i32))
                }
                None => Ok(Value::Null),
            }
        }
        "date" => {
            // DATE(ts) → extract Date portion
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Date(_) => Ok(v),
                Value::Timestamp(us) => {
                    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                    let nd = micros_to_ndt(us).date();
                    Ok(Value::Date((nd - epoch).num_days() as i32))
                }
                _ => Ok(Value::Null),
            }
        }
        "time" => {
            // TIME(ts) → 'HH:MM:SS' string
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Timestamp(us) => {
                    let ndt = micros_to_ndt(us);
                    Ok(Value::Text(format!(
                        "{:02}:{:02}:{:02}",
                        ndt.hour(),
                        ndt.minute(),
                        ndt.second()
                    )))
                }
                _ => Ok(Value::Null),
            }
        }
        "timediff" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let a = crate::eval::eval(&args[0], row)?;
            let b = crate::eval::eval(&args[1], row)?;
            fn to_secs(v: &Value) -> Option<i64> {
                match v {
                    Value::Timestamp(us) => Some(us / 1_000_000),
                    Value::Date(d) => Some(*d as i64 * 86_400),
                    _ => None,
                }
            }
            match (to_secs(&a), to_secs(&b)) {
                (Some(sa), Some(sb)) => {
                    let diff = sa - sb;
                    let sign = if diff < 0 { "-" } else { "" };
                    let abs = diff.unsigned_abs();
                    Ok(Value::Text(format!(
                        "{}{:02}:{:02}:{:02}",
                        sign,
                        abs / 3600,
                        (abs % 3600) / 60,
                        abs % 60
                    )))
                }
                _ => Ok(Value::Null),
            }
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
pub(crate) fn micros_to_ndt(micros: i64) -> NaiveDateTime {
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

/// Coerces a `Value` to a `NaiveDate` for use in date functions.
///
/// Handles `Date`, `Timestamp`, and `Text` (string parsing via common formats).
/// Returns `None` for Null or unconvertible values.
pub(crate) fn coerce_to_ndate(v: &Value) -> Option<NaiveDate> {
    match v {
        Value::Date(d) => Some(days_to_ndate(*d)),
        Value::Timestamp(us) => Some(micros_to_ndt(*us).date()),
        Value::Text(s) => {
            if let Some((ndt, _)) = str_to_date_inner(s, "%Y-%m-%d %H:%i:%s") {
                Some(ndt.date())
            } else if let Some((ndt, _)) = str_to_date_inner(s, "%Y-%m-%d") {
                Some(ndt.date())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Converts `Value::Date(days)` (days since 1970-01-01) to `NaiveDate`.
pub(crate) fn days_to_ndate(days: i32) -> NaiveDate {
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
pub(crate) fn str_to_date_inner(s: &str, fmt: &str) -> Option<(NaiveDateTime, bool)> {
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
