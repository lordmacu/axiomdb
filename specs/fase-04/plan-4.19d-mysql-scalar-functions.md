# Plan: 4.19d — MySQL Scalar Functions

## Files to create/modify

- `crates/axiomdb-sql/Cargo.toml` — add `chrono = { version = "0.4", default-features = false, features = ["std"] }`
- `crates/axiomdb-sql/src/eval.rs` — add `date_format`, `str_to_date`, `find_in_set`; fix `year/month/day/hour/minute/second`

No new files, no new crates — all fits in the existing `eval_function` dispatcher.

---

## Algorithm / Data structure

### 1. chrono helpers (private module top of eval.rs or inline)

```rust
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};

/// Value::Timestamp(micros) → NaiveDateTime (UTC)
fn micros_to_ndt(micros: i64) -> NaiveDateTime {
    let secs = micros / 1_000_000;
    let nanos = ((micros % 1_000_000) * 1000) as u32;
    DateTime::from_timestamp(secs, nanos)
        .unwrap_or_else(|| DateTime::UNIX_EPOCH)
        .naive_utc()
}

/// Value::Date(days) → NaiveDate (days since Unix epoch)
fn days_to_ndate(days: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()
        + chrono::Duration::days(days as i64)
}
```

### 2. DATE_FORMAT — format specifier processor

```
fn date_format_str(ndt: NaiveDateTime, fmt: &str) -> String:
  output = String::new()
  chars = fmt.chars().peekable()
  loop:
    c = chars.next()
    if c == '%':
      spec = chars.next()
      match spec:
        'Y' → push year 4-digit
        'y' → push year 2-digit (% 100)
        'm' → push month 01..12
        'c' → push month 1..12 (no pad)
        'M' → push MONTH_NAMES[month-1]
        'b' → push MONTH_ABBR[month-1]
        'd' → push day 01..31
        'e' → push day 1..31 (no pad)
        'H' → push hour 00..23
        'h' → push hour 01..12 (12h)
        'i' → push minute 00..59
        's'|'S' → push second 00..59
        'p' → push "AM"/"PM"
        'W' → push WEEKDAY_NAMES[weekday]
        'a' → push WEEKDAY_ABBR[weekday]
        'j' → push day-of-year 001..366
        'w' → push weekday 0=Sunday..6=Saturday
        'T' → push "HH:MM:SS"
        'r' → push "HH:MM:SS AM/PM"
        '%' → push '%'
        _   → push '%' then the char (unknown = literal passthrough)
    else:
      push c
  return output
```

For DATE-only values (NaiveDate), set time to 00:00:00.

### 3. STR_TO_DATE — reverse parser

```
fn str_to_date_inner(s: &str, fmt: &str) -> Option<NaiveDateTime>:
  cursor on s
  has_date = false; has_time = false
  year=1970, month=1, day=1, hour=0, minute=0, second=0

  for each char in fmt:
    if '%':
      spec = next char
      match spec:
        'Y' → parse 4 digits → year; has_date=true
        'y' → parse 2 digits → year; if < 70: year+=2000 else year+=1900
        'm'|'c' → parse 1-2 digits → month; has_date=true
        'd'|'e' → parse 1-2 digits → day; has_date=true
        'H'|'h' → parse 1-2 digits → hour; has_time=true
        'i' → parse 1-2 digits → minute; has_time=true
        's'|'S' → parse 1-2 digits → second; has_time=true
        _   → skip char in s
    else:
      expect same literal char in s; if mismatch → return None

  validate: month 1-12, day 1-31, hour 0-23, minute 0-59, second 0-59
  if NaiveDate::from_ymd_opt(year, month, day) fails → None

  if has_time:
    return Some(NaiveDateTime)
  else:
    return Some(NaiveDate → NaiveDateTime at midnight)
  → caller converts to Value::Date or Value::Timestamp
```

Result type:
- Format has time components (`%H`, `%i`, `%s`) → `Value::Timestamp(micros)`
- Format has only date components → `Value::Date(days)`

### 4. FIND_IN_SET — comma split + compare

```
fn find_in_set(needle: &str, list: &str) -> i32:
  if list.is_empty(): return 0
  for (i, item) in list.split(',').enumerate():
    if item.eq_ignore_ascii_case(needle):
      return (i + 1) as i32
  return 0
```

### 5. Fix year/month/day/hour/minute/second

Replace the existing stub in `eval_function` that returns `Null` for most components:

```rust
"year" | "month" | "day" | "hour" | "minute" | "second" => {
  let v = eval(arg, row)?;
  let ndt = match v {
    Value::Null => return Ok(Value::Null),
    Value::Timestamp(micros) => micros_to_ndt(micros),
    Value::Date(days) => days_to_ndate(days).and_time(NaiveTime::MIN),
    _ => return Ok(Value::Null),
  };
  let result = match name {
    "year"   => ndt.year(),
    "month"  => ndt.month() as i32,
    "day"    => ndt.day() as i32,
    "hour"   => ndt.hour() as i32,
    "minute" => ndt.minute() as i32,
    "second" => ndt.second() as i32,
    _ => unreachable!(),
  };
  Ok(Value::Int(result))
}
```

---

## Implementation phases

1. **Add chrono dependency** to `Cargo.toml` — build check
2. **Add chrono helpers** (`micros_to_ndt`, `days_to_ndate`) at top of eval.rs
3. **Fix year/month/day/hour/minute/second** — replace stub with chrono impl
4. **Implement `FIND_IN_SET`** — add arm in `eval_function` match
5. **Implement `DATE_FORMAT`** — format specifier table + processor function
6. **Implement `STR_TO_DATE`** — parser function
7. **Write tests** in `integration_eval.rs` (or new `integration_date_functions.rs`)

---

## Tests to write

### Unit (in eval.rs or test module)
- `date_format_str` with each specifier independently
- `str_to_date_inner` round-trip with `date_format_str`
- `find_in_set` edge cases (empty, not found, NULL, case)

### Integration (integration_eval.rs or new file)
```sql
SELECT DATE_FORMAT(NOW(), '%Y-%m-%d')        → "YYYY-MM-DD"
SELECT DATE_FORMAT(CURRENT_DATE, '%d/%m/%Y') → "DD/MM/YYYY"
SELECT DATE_FORMAT(NULL, '%Y')               → NULL
SELECT STR_TO_DATE('2025-03-25', '%Y-%m-%d') -- returns Date
SELECT STR_TO_DATE('bad', '%Y')              → NULL (not error)
SELECT FIND_IN_SET('b', 'a,b,c')            → 2
SELECT FIND_IN_SET('B', 'a,b,c')            → 2  (case insensitive)
SELECT FIND_IN_SET('z', 'a,b,c')            → 0
SELECT FIND_IN_SET(NULL, 'a,b,c')           → NULL
SELECT year(NOW())                           → 2025 (or current)
SELECT month(NOW()), day(NOW())              → current month/day
SELECT hour(NOW()), minute(NOW())            → current hour/minute
```

### Wire protocol smoke test additions
```python
# DATE_FORMAT
cur.execute("SELECT DATE_FORMAT(NOW(), '%Y-%m-%d')")
val = cur.fetchone()[0]
assert len(val) == 10 and val[4] == '-'  # "YYYY-MM-DD"

# STR_TO_DATE round-trip
cur.execute("SELECT STR_TO_DATE('2025-03-25', '%Y-%m-%d')")
# should not raise

# FIND_IN_SET
cur.execute("SELECT FIND_IN_SET('b', 'a,b,c')")
assert cur.fetchone()[0] == 2

# year/month/day
cur.execute("SELECT year(NOW()), month(NOW()), day(NOW())")
row = cur.fetchone()
assert 2020 <= row[0] <= 2100
assert 1 <= row[1] <= 12
assert 1 <= row[2] <= 31
```

---

## Anti-patterns to avoid

- **DO NOT** use `unwrap()` on chrono date construction — some dates are invalid (Feb 30), use `_opt()` variants and return `Null`
- **DO NOT** use `%Y-%m-%d` via chrono's `format()` — map specifiers manually so MySQL semantics are exact (chrono uses different specifiers)
- **DO NOT** make `date_format_str` and `str_to_date_inner` public — they're internal helpers, keep them `fn` (not `pub fn`)
- **DO NOT** panic on unknown format specifiers — pass them through literally (`%X` → `"%X"`)

---

## Risks

- **chrono `DateTime::from_timestamp`** deprecated in newer versions → use `DateTime::from_timestamp_opt(s, ns).unwrap_or(DateTime::UNIX_EPOCH)`
- **2-digit year in STR_TO_DATE** (`%y`) — MySQL rule: 00-69 → 2000-2069, 70-99 → 1970-1999. Implement correctly.
- **`Value::Date(days)` epoch** — confirmed in code: days since Unix epoch (1970-01-01 = 0). chrono: `NaiveDate::from_ymd(1970,1,1) + Duration::days(days)`
