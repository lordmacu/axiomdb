//! Phase 20.10 — `FROM GENERATE_SERIES(start, stop [, step])` table-valued function.
//!
//! Produces a sequence of integers or dates following PostgreSQL semantics.
//!
//! ## Integer variant
//! - `step` defaults to `1` when omitted.
//! - Ascending (`step > 0`): yields `start, start+step, ...` while `value <= stop`.
//! - Descending (`step < 0`): yields `start, start+step, ...` while `value >= stop`.
//! - Empty result (no error) when direction and range are inconsistent.
//! - `step = 0` → error.
//!
//! ## Date variant
//! - `start`/`stop` must both be `Value::Date` (days since 1970-01-01).
//! - `step` is a string literal `'N unit'` where unit ∈ day/week/month/year.
//! - Uses `chrono::NaiveDate` for correct calendar semantics.

use axiomdb_catalog::schema::{ColumnDef, ColumnType, TableId};
use axiomdb_core::error::DbError;
use axiomdb_types::{DataType, Value};
use chrono::{Datelike, NaiveDate};

use crate::ast::GenerateSeriesClause;
use crate::eval;
use crate::result::ColumnMeta;

const MAX_ROWS: usize = 10_000_000;
const EPOCH: fn() -> NaiveDate = || NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();

/// Column metadata for GENERATE_SERIES output projection.
pub fn column_metas_for_generate_series(gs: &GenerateSeriesClause) -> Vec<ColumnMeta> {
    let name = gs
        .column_name
        .clone()
        .unwrap_or_else(|| "generate_series".to_string());
    let table_name = gs
        .alias
        .clone()
        .unwrap_or_else(|| "generate_series".to_string());
    // Data type is resolved at runtime; use Text as placeholder.
    // The executor fills in the actual Value which carries the type.
    vec![ColumnMeta {
        name,
        data_type: DataType::Text,
        nullable: false,
        table_name: Some(table_name),
    }]
}

/// Catalog-shape column defs for analyzer binding.
pub fn column_defs_for_generate_series(gs: &GenerateSeriesClause) -> Vec<ColumnDef> {
    let metas = column_metas_for_generate_series(gs);
    metas
        .into_iter()
        .enumerate()
        .map(|(i, m)| ColumnDef {
            table_id: 0 as TableId,
            col_idx: i as u16,
            name: m.name,
            col_type: ColumnType::Text,
            nullable: false,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
            enum_type_name: None,
            array_element_type: None,
            array_ndims: None,
        })
        .collect()
}

/// Core materialization — produces rows from evaluated start/stop/step Values.
pub fn materialize_generate_series(
    start: &Value,
    stop: &Value,
    step: &Value,
) -> Result<Vec<Vec<Value>>, DbError> {
    match (start, stop) {
        (Value::Int(s), Value::Int(e)) => {
            let step_i = match step {
                Value::Int(v) => *v as i64,
                Value::BigInt(v) => *v,
                Value::Null => 1,
                other => {
                    return Err(DbError::InvalidValue {
                        reason: format!(
                            "GENERATE_SERIES: step must be an integer; got {}",
                            other.variant_name()
                        ),
                    })
                }
            };
            generate_int_series(*s as i64, *e as i64, step_i, false)
        }
        (Value::BigInt(s), Value::BigInt(e)) => {
            let step_i = match step {
                Value::Int(v) => *v as i64,
                Value::BigInt(v) => *v,
                Value::Null => 1,
                other => {
                    return Err(DbError::InvalidValue {
                        reason: format!(
                            "GENERATE_SERIES: step must be an integer; got {}",
                            other.variant_name()
                        ),
                    })
                }
            };
            generate_int_series(*s, *e, step_i, true)
        }
        // Mixed Int/BigInt — coerce to BigInt.
        (Value::Int(s), Value::BigInt(e)) | (Value::BigInt(e), Value::Int(s)) => {
            // Canonical: first is start, second is stop.
            let (start_i, stop_i) = if matches!(start, Value::Int(_)) {
                (*s as i64, *e)
            } else {
                (*e, *s as i64)
            };
            let step_i = match step {
                Value::Int(v) => *v as i64,
                Value::BigInt(v) => *v,
                Value::Null => 1,
                _ => {
                    return Err(DbError::InvalidValue {
                        reason: "GENERATE_SERIES: step must be an integer".into(),
                    })
                }
            };
            generate_int_series(start_i, stop_i, step_i, true)
        }
        (Value::Date(s), Value::Date(e)) => generate_date_series(*s, *e, step),
        (Value::Null, _) | (_, Value::Null) => Err(DbError::InvalidValue {
            reason: "GENERATE_SERIES: start and stop must not be NULL".into(),
        }),
        (s, e) if std::mem::discriminant(s) != std::mem::discriminant(e) => {
            Err(DbError::InvalidValue {
                reason: "GENERATE_SERIES: start and stop must have the same type".into(),
            })
        }
        (other, _) => Err(DbError::InvalidValue {
            reason: format!(
                "GENERATE_SERIES: unsupported type {}; expected Int, BigInt, or Date",
                other.variant_name()
            ),
        }),
    }
}

/// Generate integer series. `bigint` controls whether output is BigInt or Int.
fn generate_int_series(
    start: i64,
    stop: i64,
    step: i64,
    bigint: bool,
) -> Result<Vec<Vec<Value>>, DbError> {
    if step == 0 {
        return Err(DbError::InvalidValue {
            reason: "GENERATE_SERIES: step must not be zero".into(),
        });
    }

    let mut rows = Vec::new();
    let mut cur = start;

    loop {
        if step > 0 {
            if cur > stop {
                break;
            }
        } else if cur < stop {
            break;
        }

        let val = if bigint {
            Value::BigInt(cur)
        } else {
            Value::Int(cur as i32)
        };
        rows.push(vec![val]);

        if rows.len() > MAX_ROWS {
            return Err(DbError::InvalidValue {
                reason: "GENERATE_SERIES: result exceeds 10,000,000 rows".into(),
            });
        }

        cur = match cur.checked_add(step) {
            Some(v) => v,
            None => break, // overflow → series ends
        };
    }

    Ok(rows)
}

/// Parse `'N unit'` interval string for date series step.
///
/// Returns `(n, unit)` where unit is one of: `"day"`, `"week"`, `"month"`, `"year"`.
fn parse_date_step(s: &str) -> Result<(u32, &str), DbError> {
    let s = s.trim();
    let (n_str, rest) = s
        .split_once(char::is_whitespace)
        .ok_or_else(|| DbError::InvalidValue {
            reason: format!(
                "GENERATE_SERIES: unrecognized step '{}'; expected 'N day(s|week(s|month(s|year(s)'",
                s
            ),
        })?;
    let n: u32 = n_str.parse().map_err(|_| DbError::InvalidValue {
        reason: format!(
            "GENERATE_SERIES: unrecognized step '{}'; expected 'N day(s|week(s|month(s|year(s)'",
            s
        ),
    })?;
    let unit = rest.trim();
    let canonical = match unit {
        u if u.eq_ignore_ascii_case("day") || u.eq_ignore_ascii_case("days") => "day",
        u if u.eq_ignore_ascii_case("week") || u.eq_ignore_ascii_case("weeks") => "week",
        u if u.eq_ignore_ascii_case("month") || u.eq_ignore_ascii_case("months") => "month",
        u if u.eq_ignore_ascii_case("year") || u.eq_ignore_ascii_case("years") => "year",
        _ => {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "GENERATE_SERIES: unrecognized step '{}'; expected 'N day(s|week(s|month(s|year(s)'",
                    s
                ),
            })
        }
    };
    Ok((n, canonical))
}

/// Convert days-since-epoch (i32) to NaiveDate.
fn days_to_naive(days: i32) -> Option<NaiveDate> {
    EPOCH().checked_add_signed(chrono::Duration::days(days as i64))
}

/// Convert NaiveDate to days-since-epoch (i32).
fn naive_to_days(d: NaiveDate) -> i32 {
    d.signed_duration_since(EPOCH()).num_days() as i32
}

/// Generate date series.
fn generate_date_series(
    start_days: i32,
    stop_days: i32,
    step: &Value,
) -> Result<Vec<Vec<Value>>, DbError> {
    let step_str = match step {
        Value::Text(s) => s.as_str(),
        Value::Null => {
            return Err(DbError::InvalidValue {
                reason: "GENERATE_SERIES: step is required for date series".into(),
            })
        }
        other => {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "GENERATE_SERIES: date step must be a string; got {}",
                    other.variant_name()
                ),
            })
        }
    };

    let (n, unit) = parse_date_step(step_str)?;

    if n == 0 {
        return Err(DbError::InvalidValue {
            reason: "GENERATE_SERIES: step must not be zero".into(),
        });
    }

    let start_date = days_to_naive(start_days).ok_or_else(|| DbError::InvalidValue {
        reason: "GENERATE_SERIES: start date out of range".into(),
    })?;
    let stop_date = days_to_naive(stop_days).ok_or_else(|| DbError::InvalidValue {
        reason: "GENERATE_SERIES: stop date out of range".into(),
    })?;

    let mut rows = Vec::new();
    let mut cur = start_date;

    while cur <= stop_date {
        rows.push(vec![Value::Date(naive_to_days(cur))]);

        if rows.len() > MAX_ROWS {
            return Err(DbError::InvalidValue {
                reason: "GENERATE_SERIES: result exceeds 10,000,000 rows".into(),
            });
        }

        cur = advance_date(cur, n, unit)?;
    }

    Ok(rows)
}

/// Advance a date by `n` units. Uses chrono calendar arithmetic.
fn advance_date(d: NaiveDate, n: u32, unit: &str) -> Result<NaiveDate, DbError> {
    let next = match unit {
        "day" => d.checked_add_signed(chrono::Duration::days(n as i64)),
        "week" => d.checked_add_signed(chrono::Duration::weeks(n as i64)),
        "month" => {
            let total_months = d.month() as i64 + n as i64 - 1;
            let year = d.year() + (total_months / 12) as i32;
            let month = (total_months % 12) as u32 + 1;
            // Clamp day to last day of the target month.
            let max_day = days_in_month(year, month);
            let day = d.day().min(max_day);
            NaiveDate::from_ymd_opt(year, month, day)
        }
        "year" => {
            let year = d.year() + n as i32;
            let max_day = days_in_month(year, d.month());
            let day = d.day().min(max_day);
            NaiveDate::from_ymd_opt(year, d.month(), day)
        }
        _ => unreachable!("unit validated in parse_date_step"),
    };
    next.ok_or_else(|| DbError::InvalidValue {
        reason: "GENERATE_SERIES: date arithmetic overflow".into(),
    })
}

/// Return the number of days in a given month/year.
fn days_in_month(year: i32, month: u32) -> u32 {
    // Day 1 of the next month minus 1 day.
    let (next_y, next_m) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_y, next_m, 1)
        .and_then(|d| d.checked_sub_signed(chrono::Duration::days(1)))
        .map(|d| d.day())
        .unwrap_or(28)
}

/// Evaluate start/stop/step from a GenerateSeriesClause and materialize rows.
/// Called by the executor (both non-ctx and ctx paths).
pub fn materialize_from_clause(
    gs: &GenerateSeriesClause,
    row: &[Value],
) -> Result<Vec<Vec<Value>>, DbError> {
    let start = eval(&gs.start, row)?;
    let stop = eval(&gs.stop, row)?;
    let step = match &gs.step {
        Some(expr) => eval(expr, row)?,
        None => Value::Null,
    };
    materialize_generate_series(&start, &stop, &step)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_int_series_ascending() {
        let rows =
            materialize_generate_series(&Value::Int(1), &Value::Int(5), &Value::Null).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0], vec![Value::Int(1)]);
        assert_eq!(rows[4], vec![Value::Int(5)]);
    }

    #[test]
    fn test_int_series_explicit_step() {
        let rows =
            materialize_generate_series(&Value::Int(1), &Value::Int(9), &Value::Int(2)).unwrap();
        assert_eq!(rows.len(), 5); // 1,3,5,7,9
        assert_eq!(rows[2], vec![Value::Int(5)]);
    }

    #[test]
    fn test_int_series_descending() {
        let rows =
            materialize_generate_series(&Value::Int(5), &Value::Int(1), &Value::Int(-1)).unwrap();
        assert_eq!(rows.len(), 5); // 5,4,3,2,1
        assert_eq!(rows[0], vec![Value::Int(5)]);
        assert_eq!(rows[4], vec![Value::Int(1)]);
    }

    #[test]
    fn test_int_series_empty_ascending_wrong_direction() {
        let rows =
            materialize_generate_series(&Value::Int(5), &Value::Int(1), &Value::Null).unwrap();
        assert!(rows.is_empty()); // 5 > 1, step=1 → empty
    }

    #[test]
    fn test_int_series_empty_descending_wrong_direction() {
        let rows =
            materialize_generate_series(&Value::Int(1), &Value::Int(5), &Value::Int(-1)).unwrap();
        assert!(rows.is_empty()); // 1 < 5, step=-1 → empty
    }

    #[test]
    fn test_int_series_single_row() {
        let rows =
            materialize_generate_series(&Value::Int(7), &Value::Int(7), &Value::Null).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Int(7)]);
    }

    #[test]
    fn test_int_series_step_zero_error() {
        let err = materialize_generate_series(&Value::Int(1), &Value::Int(5), &Value::Int(0))
            .unwrap_err();
        assert!(err.to_string().contains("step must not be zero"));
    }

    #[test]
    fn test_null_start_error() {
        let err =
            materialize_generate_series(&Value::Null, &Value::Int(5), &Value::Null).unwrap_err();
        assert!(err.to_string().contains("NULL"));
    }

    #[test]
    fn test_date_series_monthly() {
        // 2024-01-01 to 2024-03-01 step 1 month → 3 rows
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let jan = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let mar = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        let s = Value::Date(jan.signed_duration_since(epoch).num_days() as i32);
        let e = Value::Date(mar.signed_duration_since(epoch).num_days() as i32);
        let step = Value::Text("1 month".into());
        let rows = materialize_generate_series(&s, &e, &step).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn test_date_series_daily() {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let d1 = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let d7 = NaiveDate::from_ymd_opt(2024, 1, 7).unwrap();
        let s = Value::Date(d1.signed_duration_since(epoch).num_days() as i32);
        let e = Value::Date(d7.signed_duration_since(epoch).num_days() as i32);
        let step = Value::Text("1 day".into());
        let rows = materialize_generate_series(&s, &e, &step).unwrap();
        assert_eq!(rows.len(), 7);
    }

    #[test]
    fn test_date_series_missing_step_error() {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let d = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let days = Value::Date(d.signed_duration_since(epoch).num_days() as i32);
        let err = materialize_generate_series(&days, &days, &Value::Null).unwrap_err();
        assert!(err.to_string().contains("step is required"));
    }

    #[test]
    fn test_date_series_end_of_month() {
        // 2024-01-31 + 1 month → 2024-02-29 (2024 is leap), + 1 month → 2024-03-29
        // PostgreSQL: month arithmetic clamps to last day.
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let jan31 = NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
        let mar31 = NaiveDate::from_ymd_opt(2024, 3, 31).unwrap();
        let s = Value::Date(jan31.signed_duration_since(epoch).num_days() as i32);
        let e = Value::Date(mar31.signed_duration_since(epoch).num_days() as i32);
        let step = Value::Text("1 month".into());
        let rows = materialize_generate_series(&s, &e, &step).unwrap();
        assert_eq!(rows.len(), 3); // 2024-01-31, 2024-02-29, 2024-03-29
    }
}
