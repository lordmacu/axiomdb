# Spec: 4.19d — MySQL Scalar Functions

## What to build (not how)

Implement three missing MySQL-compatible scalar functions plus fix the incomplete
date-component extractors (`year`, `month`, `day`, `hour`, `minute`, `second`).

`IF`, `IFNULL`, `NULLIF` are already implemented in `eval.rs` and are NOT in scope.

### Functions in scope

| Function | Signature | Returns |
|---|---|---|
| `DATE_FORMAT` | `(ts, fmt)` | TEXT |
| `STR_TO_DATE` | `(str, fmt)` | Date \| Timestamp \| NULL |
| `FIND_IN_SET` | `(needle, list)` | INT (1-indexed pos or 0) |
| `year` `month` `day` `hour` `minute` `second` | `(val)` | INT (fix existing stub) |

---

## Inputs / Outputs

### DATE_FORMAT(ts, fmt)

- **Input:** `ts` = any of `Value::Date(i32)`, `Value::Timestamp(i64)`, or coercible Text
- **Input:** `fmt` = `Value::Text` — MySQL-style format string
- **Output:** `Value::Text`
- **Errors:** `Value::Null` if ts is Null; `Value::Null` if fmt is Null/empty
- **NOT an error:** invalid format specifiers are passed through literally

Date epoch: `Value::Date(days)` = days since Unix epoch (1970-01-01 = 0).
Timestamp epoch: `Value::Timestamp(micros)` = microseconds since Unix epoch.

### STR_TO_DATE(str, fmt)

- **Input:** `str` = `Value::Text`, `fmt` = `Value::Text`
- **Output:** `Value::Timestamp` if format includes time components, `Value::Date` if date-only
- **Errors:** `Value::Null` on parse failure (MySQL behavior — never an error, always Null)
- **Errors:** `Value::Null` if either argument is Null

### FIND_IN_SET(needle, csv_list)

- **Input:** `needle` = `Value::Text`, `csv_list` = `Value::Text` (comma-separated)
- **Output:** `Value::Int` — 1-indexed position of first match; 0 if not found
- **Errors:** `Value::Null` if either argument is Null
- **Comparison:** case-insensitive (MySQL default), using `.eq_ignore_ascii_case()`
- **Separator:** always `,` (not configurable)

### year / month / day / hour / minute / second (fix)

- **Input:** `Value::Date(i32)` or `Value::Timestamp(i64)` or Text coercible to date
- **Output:** `Value::Int`
- **Errors:** `Value::Null` if input is Null or not a date/timestamp

---

## Format specifiers for DATE_FORMAT / STR_TO_DATE

Core MySQL subset to implement:

| Specifier | Description | Example |
|---|---|---|
| `%Y` | 4-digit year | `2025` |
| `%y` | 2-digit year | `25` |
| `%m` | Month 01–12 | `03` |
| `%c` | Month 1–12 (no pad) | `3` |
| `%M` | Full month name | `March` |
| `%b` | Abbreviated month name | `Mar` |
| `%d` | Day 01–31 | `05` |
| `%e` | Day 1–31 (no pad) | `5` |
| `%H` | Hour 00–23 | `14` |
| `%h` | Hour 01–12 | `02` |
| `%i` | Minute 00–59 | `30` |
| `%s` / `%S` | Second 00–59 | `45` |
| `%p` | AM / PM | `PM` |
| `%W` | Full weekday name | `Wednesday` |
| `%a` | Abbreviated weekday | `Wed` |
| `%j` | Day of year 001–366 | `064` |
| `%w` | Day of week 0=Sunday | `3` |
| `%T` | Time `HH:MM:SS` (24h) | `14:30:45` |
| `%r` | Time `HH:MM:SS AM/PM` | `02:30:45 PM` |
| `%%` | Literal `%` | `%` |

Unknown specifiers: pass through unchanged (MySQL behavior).

---

## Use cases

### DATE_FORMAT

1. **Happy path — timestamp to date string:**
   `DATE_FORMAT(NOW(), '%Y-%m-%d')` → `"2025-03-25"`

2. **Happy path — date only:**
   `DATE_FORMAT(CURRENT_DATE, '%d/%m/%Y')` → `"25/03/2025"`

3. **NULL input → NULL output:**
   `DATE_FORMAT(NULL, '%Y-%m-%d')` → `NULL`

4. **NULL format → NULL output:**
   `DATE_FORMAT(NOW(), NULL)` → `NULL`

5. **Unknown specifier passes through:**
   `DATE_FORMAT(NOW(), '%Y-%X-%d')` → `"2025-%X-25"` (% + unknown char = literal)

### STR_TO_DATE

1. **Happy path — date string:**
   `STR_TO_DATE('25/03/2025', '%d/%m/%Y')` → `Value::Date(...)`

2. **Happy path — datetime string:**
   `STR_TO_DATE('2025-03-25 14:30:00', '%Y-%m-%d %H:%i:%s')` → `Value::Timestamp(...)`

3. **Parse failure → NULL (not error):**
   `STR_TO_DATE('not-a-date', '%Y-%m-%d')` → `NULL`

4. **NULL input → NULL:**
   `STR_TO_DATE(NULL, '%Y-%m-%d')` → `NULL`

### FIND_IN_SET

1. **Happy path — found:**
   `FIND_IN_SET('b', 'a,b,c')` → `2`

2. **Happy path — not found:**
   `FIND_IN_SET('z', 'a,b,c')` → `0`

3. **NULL argument → NULL:**
   `FIND_IN_SET(NULL, 'a,b,c')` → `NULL`
   `FIND_IN_SET('a', NULL)` → `NULL`

4. **Case-insensitive:**
   `FIND_IN_SET('B', 'a,b,c')` → `2`

5. **Empty needle:**
   `FIND_IN_SET('', 'a,b,c')` → `0`

6. **Empty list:**
   `FIND_IN_SET('a', '')` → `0`

---

## Acceptance criteria

- [ ] `DATE_FORMAT(NOW(), '%Y-%m-%d')` returns current date as `"YYYY-MM-DD"`
- [ ] `DATE_FORMAT(CURRENT_DATE, '%d/%m/%Y')` returns `"DD/MM/YYYY"`
- [ ] `DATE_FORMAT(NULL, '%Y')` returns `NULL`
- [ ] `STR_TO_DATE('2025-03-25', '%Y-%m-%d')` returns a Date value representing 2025-03-25
- [ ] `STR_TO_DATE('2025-03-25 14:30:00', '%Y-%m-%d %H:%i:%s')` returns Timestamp
- [ ] `STR_TO_DATE('bad', '%Y-%m-%d')` returns `NULL` (not an error)
- [ ] `FIND_IN_SET('b', 'a,b,c')` returns `2`
- [ ] `FIND_IN_SET('z', 'a,b,c')` returns `0`
- [ ] `FIND_IN_SET(NULL, 'x')` returns `NULL`
- [ ] `FIND_IN_SET('B', 'a,b,c')` returns `2` (case-insensitive)
- [ ] `year(NOW())` returns current year as INT
- [ ] `month(NOW())` returns current month 1-12 as INT
- [ ] `day(NOW())` returns current day 1-31 as INT
- [ ] `hour(NOW())` returns current hour 0-23 as INT
- [ ] `minute(NOW())` returns current minute 0-59 as INT
- [ ] `second(NOW())` returns current second 0-59 as INT
- [ ] All functions: `NULL` input → `NULL` output (except `year/month/day/hour/minute/second` with valid Date/Timestamp)
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy -- -D warnings` clean
- [ ] Wire protocol smoke test passes

---

## Out of scope

- `DATE_FORMAT` with locale-specific names (month/weekday names are English only)
- `DATE_FORMAT` with timezone conversion (always UTC)
- `STR_TO_DATE` with microseconds (`%f`)
- `FIND_IN_SET` with non-ASCII separators
- `DATE_ADD` / `DATE_SUB` / `TIMESTAMPDIFF` (separate subfase if needed)
- 3-argument `DATE_FORMAT(ts, fmt, locale)` — only 2-arg form

---

## Dependencies

- `chrono` crate — add to `axiomdb-sql/Cargo.toml` for correct date arithmetic
  - `Value::Timestamp(micros)` → `DateTime<Utc>` via `DateTime::from_timestamp(s, us*1000)`
  - `Value::Date(days)` → `NaiveDate` via `NaiveDate::from_ymd(1970,1,1) + Duration::days(days)`
- No other new dependencies required

## Research sources

- MariaDB: `sql/item_timefunc.cc` — `Item_func_date_format::val_str`, `Item_func_str_to_date::get_date_common`
- MariaDB: `sql/item_func.cc` — `Item_func_find_in_set::val_int`
- SQLite: `func.c` — `strftimeFunc` for format specifier reference
- DuckDB: `strftime_format.cpp` — format specifier table
