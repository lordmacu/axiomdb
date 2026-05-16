// Business calendar runtime functions (Phase 20.16):
//   IS_BUSINESS_DAY(date, country_code)         → BOOLEAN (0 or 1)
//   NEXT_BUSINESS_DAY(date, country_code)        → DATE (days since epoch as INT)
//   BUSINESS_DAYS_BETWEEN(start_date, end_date, country_code) → INT
//
// Included into the executor module (mod.rs) so it shares all imports.
// Holiday sets are loaded from the catalog once per (session, country_code)
// and cached in `ctx.holiday_cache`.

fn eval_business_calendar_function(
    name: &str,
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Option<Value>, DbError> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "is_business_day" => is_business_day_fn(args, row, runner).map(Some),
        "next_business_day" => next_business_day_fn(args, row, runner).map(Some),
        "business_days_between" => business_days_between_fn(args, row, runner).map(Some),
        _ => Ok(None),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolves a holiday set for `country` from the session cache or catalog.
fn get_or_load_holidays(
    country: &str,
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<std::sync::Arc<std::collections::HashSet<i32>>, DbError> {
    let key = country.to_ascii_uppercase();
    if let Some(cached) = runner.ctx.holiday_cache.get(&key) {
        return Ok(std::sync::Arc::clone(cached));
    }
    let snap = runner.txn.snapshot();
    let mut reader = CatalogReader::new(runner.storage, snap)?;
    let def = reader.get_holiday_calendar(&key)?;
    let set: std::sync::Arc<std::collections::HashSet<i32>> = std::sync::Arc::new(
        def.map(|d| d.holidays.into_iter().collect())
            .unwrap_or_default(),
    );
    runner.ctx.holiday_cache.insert(key, std::sync::Arc::clone(&set));
    Ok(set)
}

/// Parses a function date argument — accepts:
/// - `Value::Int` / `Value::BigInt`: already days-since-epoch
/// - `Value::Text("YYYY-MM-DD")`: parsed via chrono
/// - `Value::Date(days)`: days-since-epoch
fn eval_date_arg(
    fn_name: &str,
    param: &str,
    arg: &Expr,
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<i32, DbError> {
    let v = crate::eval::eval_with(arg, row, runner)?;
    match v {
        Value::Int(d) => Ok(d),
        Value::BigInt(d) => i32::try_from(d).map_err(|_| DbError::InvalidValue {
            reason: format!("{fn_name}({param}): day value out of i32 range"),
        }),
        Value::Date(d) => Ok(d),
        Value::Text(s) => {
            use chrono::NaiveDate;
            let date =
                NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| DbError::InvalidValue {
                    reason: format!("{fn_name}({param}): expected YYYY-MM-DD, got '{s}'"),
                })?;
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let days = date.signed_duration_since(epoch).num_days();
            i32::try_from(days).map_err(|_| DbError::InvalidValue {
                reason: format!("{fn_name}({param}): date '{s}' out of representable range"),
            })
        }
        other => Err(DbError::TypeMismatch {
            expected: format!("{fn_name}({param}: DATE or INT or TEXT)"),
            got: other.to_string(),
        }),
    }
}

fn eval_text_arg_bc(
    fn_name: &str,
    param: &str,
    arg: &Expr,
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<String, DbError> {
    let v = crate::eval::eval_with(arg, row, runner)?;
    match v {
        Value::Text(s) => Ok(s),
        other => Err(DbError::TypeMismatch {
            expected: format!("{fn_name}({param}: TEXT)"),
            got: other.to_string(),
        }),
    }
}

/// Returns true iff `day` is Mon–Fri (weekday).
///
/// Formula: `(day + 3) % 7` maps days to Mon=0 … Sun=6.
/// Epoch 1970-01-01 (day 0) is Thursday → `(0 + 3) % 7 = 3` (Thu) ✓.
/// Weekday iff the result is < 5.
#[inline]
fn is_weekday(day: i32) -> bool {
    ((day + 3).rem_euclid(7) as u32) < 5
}

// ── IS_BUSINESS_DAY ───────────────────────────────────────────────────────────

fn is_business_day_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 2 {
        return Err(DbError::TypeMismatch {
            expected: "IS_BUSINESS_DAY(date, country_code)".into(),
            got: format!("{} args", args.len()),
        });
    }
    let day = eval_date_arg("IS_BUSINESS_DAY", "date", &args[0], row, runner)?;
    let country = eval_text_arg_bc("IS_BUSINESS_DAY", "country_code", &args[1], row, runner)?;
    let holidays = get_or_load_holidays(&country, runner)?;

    let result = is_weekday(day) && !holidays.contains(&day);
    Ok(Value::Int(i32::from(result)))
}

// ── NEXT_BUSINESS_DAY ─────────────────────────────────────────────────────────

fn next_business_day_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 2 {
        return Err(DbError::TypeMismatch {
            expected: "NEXT_BUSINESS_DAY(date, country_code)".into(),
            got: format!("{} args", args.len()),
        });
    }
    let day = eval_date_arg("NEXT_BUSINESS_DAY", "date", &args[0], row, runner)?;
    let country = eval_text_arg_bc("NEXT_BUSINESS_DAY", "country_code", &args[1], row, runner)?;
    let holidays = get_or_load_holidays(&country, runner)?;

    // Start from next calendar day.
    let mut candidate = day.checked_add(1).ok_or_else(|| DbError::InvalidValue {
        reason: "NEXT_BUSINESS_DAY: date overflow".into(),
    })?;
    // Cap iteration to avoid infinite loops (e.g. calendar has every day as holiday).
    for _ in 0..3650 {
        if is_weekday(candidate) && !holidays.contains(&candidate) {
            return Ok(Value::Int(candidate));
        }
        candidate = candidate.checked_add(1).ok_or_else(|| DbError::InvalidValue {
            reason: "NEXT_BUSINESS_DAY: date overflow".into(),
        })?;
    }
    Err(DbError::InvalidValue {
        reason: "NEXT_BUSINESS_DAY: no business day found within 10 years".into(),
    })
}

// ── BUSINESS_DAYS_BETWEEN ─────────────────────────────────────────────────────

fn business_days_between_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 3 {
        return Err(DbError::TypeMismatch {
            expected: "BUSINESS_DAYS_BETWEEN(start_date, end_date, country_code)".into(),
            got: format!("{} args", args.len()),
        });
    }
    let start = eval_date_arg("BUSINESS_DAYS_BETWEEN", "start_date", &args[0], row, runner)?;
    let end = eval_date_arg("BUSINESS_DAYS_BETWEEN", "end_date", &args[1], row, runner)?;
    let country =
        eval_text_arg_bc("BUSINESS_DAYS_BETWEEN", "country_code", &args[2], row, runner)?;
    let holidays = get_or_load_holidays(&country, runner)?;

    if start >= end {
        return Ok(Value::Int(0));
    }

    // Count weekdays in [start, end) using the full-week shortcut.
    // start_day_of_week: 0=Mon … 4=Fri, 5=Sat, 6=Sun (same as is_weekday formula)
    let span = (end - start) as i64;
    let full_weeks = span / 7;
    let remainder = span % 7;

    // Day-of-week index for `start` (0=Mon … 6=Sun, same as is_weekday formula)
    let start_dow = ((start + 3).rem_euclid(7)) as i64;

    // Count weekdays in the remainder [start, start+remainder)
    let remainder_weekdays: i64 = (0..remainder)
        .filter(|i| ((start_dow + i) % 7) < 5)
        .count() as i64;

    let total_weekdays = full_weeks * 5 + remainder_weekdays;

    // Subtract holidays that fall strictly in [start, end) on weekdays.
    let holiday_count = holidays
        .iter()
        .filter(|&&h| h >= start && h < end && is_weekday(h))
        .count() as i64;

    let result = (total_weekdays - holiday_count) as i32;
    Ok(Value::Int(result))
}
