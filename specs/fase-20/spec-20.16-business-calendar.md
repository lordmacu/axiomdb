# Spec: 20.16 Business Calendar Functions

Phase: 20 — Types + import/export  
Task: Business calendar DDL + scalar functions  
Status: approved

## Context

Phase 20 delivers date/time extensions. Subphase 20.16 adds business-calendar
awareness: a catalog-backed holiday registry per country code plus three scalar
functions that are the surface API. Functions need DB access at call time (to
load the calendar), so they follow the same hook used by `NEXTVAL`/`CURRVAL` —
intercepted in `ExecSubqueryRunner::eval_function` rather than implemented as
pure scalar functions in `eval/functions/datetime.rs`.

Predecessor: 20.15 (Regex operators). Successor: 20.17 (MONEY type).

## Goal

Implement `IS_BUSINESS_DAY`, `NEXT_BUSINESS_DAY`, and `BUSINESS_DAYS_BETWEEN`
scalar functions backed by a persistent per-country holiday catalog that users
populate with `CREATE HOLIDAY CALENDAR` DDL.

## Non-goals

- No built-in holiday data for any country — users must create calendars
  explicitly (or accept weekend-only behaviour when no calendar exists).
- No `ALTER HOLIDAY CALENDAR` — replace by issuing `CREATE HOLIDAY CALENDAR`
  again (create-or-replace semantics).
- No time-zone awareness in the functions — all inputs are `DATE` values
  (days since epoch), not timestamps.
- No `SHOW HOLIDAY CALENDARS` / `information_schema` exposure — deferred.
- No UNNEST or range-based expansion of calendar entries — just scalar lookups.
- No holiday inheritance (country inheriting another country's holidays).

## Behaviour

### DDL — CREATE HOLIDAY CALENDAR

```sql
CREATE HOLIDAY CALENDAR 'country_code'
  ('date_literal' [, 'date_literal' ...]);

-- Examples
CREATE HOLIDAY CALENDAR 'CO'
  ('2026-01-01', '2026-03-23', '2026-05-01', '2026-12-25');

CREATE HOLIDAY CALENDAR 'US'
  ('2026-01-01', '2026-07-04', '2026-11-26', '2026-12-25');
```

- `country_code` is a bare string literal (no schema qualifier).
  Stored and matched case-insensitively (normalised to upper-case).
- Date strings must be `'YYYY-MM-DD'`; other formats produce
  `DbError::ParseError` at DDL execution time.
- Semantics: **create or replace** — if a calendar already exists for this
  country code, it is atomically overwritten. No error when it already exists.
- Empty date list is valid: `CREATE HOLIDAY CALENDAR 'XX' ()` — creates a
  calendar with no holidays (weekend-only behaviour, but explicitly registered).
- Up to 65 535 dates per calendar (u16 count field).

### DDL — DROP HOLIDAY CALENDAR

```sql
DROP HOLIDAY CALENDAR [IF EXISTS] 'country_code';
```

- Drops the named calendar from the catalog.
- Without `IF EXISTS`: `DbError::ObjectNotFound` if the calendar does not
  exist.
- With `IF EXISTS`: no error; returns `QueryResult::Affected(0)`.
- After drop, functions that referenced this country code fall back to
  weekend-only behaviour (no error).

### Scalar Functions

All three functions are intercepted in `ExecSubqueryRunner::eval_function`.
Pure `eval()` (i.e. the `NoSubquery` path) returns `DbError::NotImplemented`
with message: `"business calendar functions require a database context —
use eval_with instead of eval"`.

#### IS_BUSINESS_DAY(date, country_code) → BOOL

Returns `TRUE` if `date` is:
1. a weekday (Monday–Friday), AND
2. not listed in the holiday calendar for `country_code` (if one exists).

```
IS_BUSINESS_DAY('2026-01-01', 'CO')  -- Thursday AND holiday → FALSE
IS_BUSINESS_DAY('2026-01-02', 'CO')  -- Friday, not holiday → TRUE
IS_BUSINESS_DAY('2026-01-03', 'CO')  -- Saturday → FALSE
IS_BUSINESS_DAY('2026-01-02', 'XX')  -- 'XX' has no calendar → weekend-only → TRUE
IS_BUSINESS_DAY(NULL, 'CO')          -- → NULL
IS_BUSINESS_DAY('2026-01-02', NULL)  -- → NULL
```

#### NEXT_BUSINESS_DAY(date, country_code) → DATE

Returns the first business day that is **strictly after** `date`.

- If `date` itself is a business day, it is **not** returned; the search
  starts from `date + 1`.
- Skips weekends and calendar holidays.
- Maximum search window: 14 days. If no business day is found within 14
  days (e.g. a user-created calendar marks every day as a holiday), returns
  `DbError::InvalidValue`.

```
NEXT_BUSINESS_DAY('2026-01-02', 'CO')  -- Friday → Monday 2026-01-05 (skip weekend)
NEXT_BUSINESS_DAY('2026-01-01', 'CO')  -- Thursday holiday → 2026-01-02 Friday
NEXT_BUSINESS_DAY('2026-01-03', 'CO')  -- Saturday → 2026-01-05 Monday
NEXT_BUSINESS_DAY(NULL, 'CO')          -- → NULL
NEXT_BUSINESS_DAY('2026-01-02', NULL)  -- → NULL
```

Return type: `Value::Date(i32)` — days since Unix epoch, same as `CURRENT_DATE`.

#### BUSINESS_DAYS_BETWEEN(date1, date2, country_code) → INT

Returns the number of business days in the half-open interval `[date1, date2)`.

- If `date1 == date2` → 0.
- If `date1 < date2` → positive count.
- If `date1 > date2` → negative count (counts `[date2, date1)` then negates).
- `date1` is included in the count if it is a business day; `date2` is not.

```
BUSINESS_DAYS_BETWEEN('2026-01-01', '2026-01-08', 'CO')
  -- Week: Thu(holiday), Fri, Sat, Sun, Mon, Tue, Wed → 4 business days (Fri+Mon+Tue+Wed)
BUSINESS_DAYS_BETWEEN('2026-01-08', '2026-01-01', 'CO')  -- → -4
BUSINESS_DAYS_BETWEEN('2026-01-05', '2026-01-05', 'CO')  -- → 0
BUSINESS_DAYS_BETWEEN(NULL, '2026-01-08', 'CO')           -- → NULL
BUSINESS_DAYS_BETWEEN('2026-01-01', NULL, 'CO')           -- → NULL
BUSINESS_DAYS_BETWEEN('2026-01-01', '2026-01-08', NULL)   -- → NULL
```

Return type: `Value::Int(i32)`.

For performance over large ranges, use the mathematical shortcut:
- Total calendar days = |d2 - d1|
- Full weeks in range → 5 business days each
- Remaining days → check weekday by `(epoch_day + 4) % 7` (0=Mon)
- Subtract holidays in `[date1, date2)` from the `HashSet<i32>`

### Session-level calendar cache

The calendar is loaded from the catalog on first use within a session and
cached in `SessionContext.holiday_cache: HashMap<String, Arc<HashSet<i32>>>`.
The key is the upper-cased country code.

Cache invalidation:
- On `CREATE HOLIDAY CALENDAR 'XX' (...)`: remove `"XX"` from cache.
- On `DROP HOLIDAY CALENDAR 'XX'`: remove `"XX"` from cache.
- Cache is session-scoped; no cross-session invalidation needed (each session
  reads its own snapshot).

### Error cases

| Situation | Error | Message |
|---|---|---|
| `CREATE HOLIDAY CALENDAR 'CO' ('not-a-date')` | `DbError::ParseError` | `"invalid date literal in HOLIDAY CALENDAR: 'not-a-date'"` |
| `DROP HOLIDAY CALENDAR 'XX'` (not found) | `DbError::ObjectNotFound` | `"holiday calendar 'XX' does not exist"` |
| `NEXT_BUSINESS_DAY` — no business day in 14 days | `DbError::InvalidValue` | `"NEXT_BUSINESS_DAY: no business day found within 14 days after <date>"` |
| Any function called without DB context | `DbError::NotImplemented` | `"business calendar functions require a database context — use eval_with instead of eval"` |

## On-disk format

### Meta page — new constant

```
CATALOG_HOLIDAY_CALENDARS_ROOT_BODY_OFFSET: usize = 184
```

8-byte u64 at body offset 184 of the meta page (page 0).
Value 0 = not yet allocated (lazy-init on first `CREATE HOLIDAY CALENDAR`).

### HolidayCalendarDef binary encoding

```
Byte layout:
  offset  size  field            description
  0       1     code_len         u8 — country code byte length (≤ 16)
  1       N     country_code     UTF-8 bytes, upper-cased
  N+1     2     holiday_count    u16 LE — number of holiday dates
  N+3     4*H   holidays[]       i32 LE each — days since Unix epoch, sorted ascending
```

- `code_len ≤ 16` enforced at DDL time (`DbError::InvalidValue` otherwise).
- Holidays stored sorted ascending for efficient range queries.
- A single heap row = one `HolidayCalendarDef`.
- Multiple calendars coexist in the same heap; full scan to find by country
  code (expected count: ≤ hundreds).

Compatibility: field is lazy-init (offset=0 on legacy DBs). Existing databases
open without error and have no holiday calendars registered.

## Edge cases

- [ ] Empty date list: `CREATE HOLIDAY CALENDAR 'XX' ()` — valid; `holiday_count=0`
- [ ] Single date: `CREATE HOLIDAY CALENDAR 'XX' ('2026-01-01')` — valid
- [ ] Duplicate dates in CREATE list — deduplicated silently (stored once, sorted)
- [ ] Country code with spaces / special chars — rejected: `DbError::ParseError`
- [ ] Country code > 16 bytes — rejected: `DbError::InvalidValue`
- [ ] NULL first arg to IS_BUSINESS_DAY — → NULL (not DbError)
- [ ] NULL second arg (country_code) — → NULL
- [ ] NEXT_BUSINESS_DAY on last representable date — `DbError::InvalidValue`
  ("date out of range")
- [ ] BUSINESS_DAYS_BETWEEN where date2 < date1 — negative result, not error
- [ ] Calendar created mid-session — functions see it immediately (cache miss
  on next call loads fresh from catalog)
- [ ] Calendar dropped mid-session — `country_code` falls back to weekend-only
  (cache entry removed on DROP)
- [ ] Re-creating same calendar — old data replaced atomically; cache invalidated

## Performance budget

| Operation | Target | Max acceptable |
|---|---|---|
| IS_BUSINESS_DAY (cache warm) | < 1 µs | 5 µs |
| IS_BUSINESS_DAY (cache cold, first call) | < 1 ms | 5 ms |
| NEXT_BUSINESS_DAY (worst case, 14-day scan) | < 15 µs | 50 µs |
| BUSINESS_DAYS_BETWEEN (1-year range) | < 10 µs | 50 µs |
| CREATE HOLIDAY CALENDAR (365 dates) | < 10 ms | 50 ms |

## Dependencies

- Depends on: axiomdb-storage (meta page offsets), axiomdb-catalog (heap
  machinery, `CatalogPageIds`, `CatalogReader`, `CatalogWriter`),
  axiomdb-sql (AST, parser, executor, `ExecSubqueryRunner`)
- Blocks: nothing in the critical path (20.17 is independent)

## Done criteria

- [ ] `CREATE HOLIDAY CALENDAR` parses and persists correctly; roundtrip test
      via `HolidayCalendarDef::to_bytes` / `from_bytes`
- [ ] `DROP HOLIDAY CALENDAR` and `DROP HOLIDAY CALENDAR IF EXISTS` work
- [ ] `IS_BUSINESS_DAY` returns correct BOOL for weekday, weekend, holiday,
      missing-calendar (weekend-only), and NULL inputs
- [ ] `NEXT_BUSINESS_DAY` returns correct DATE, skips weekends and holidays,
      raises `InvalidValue` after 14-day scan, propagates NULL
- [ ] `BUSINESS_DAYS_BETWEEN` returns correct signed INT for forward, backward,
      same-day, cross-holiday ranges; propagates NULL for any NULL argument
- [ ] Session cache: warm path used on repeated calls within same session
- [ ] Cache invalidated on CREATE and DROP within same session
- [ ] `cargo nextest run -p axiomdb-sql` — all new + regression tests pass
- [ ] `cargo nextest run -p axiomdb-catalog` — roundtrip unit test passes
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `tools/wire-test.py` — at least 4 new assertions covering CREATE, IS_BUSINESS_DAY, NEXT_BUSINESS_DAY, BUSINESS_DAYS_BETWEEN
- [ ] `docs/fase-20.md` updated with 20.16 section
- [ ] `docs/progreso.md` marks 20.16 ✅

## References

- Existing pattern: `crates/axiomdb-catalog/src/schema_sequence.rs` (catalog heap + serde)
- Existing pattern: `crates/axiomdb-catalog/src/schema_enum.rs` (catalog heap + serde)
- Existing pattern: `crates/axiomdb-sql/src/executor/sequence_runtime.rs`
  (`eval_function` hook for DB-context functions)
- Meta offsets: `crates/axiomdb-storage/src/meta.rs` — last used offset 176;
  new offset 184
- `eval/functions/datetime.rs`: `days_to_ndate`, `coerce_to_ndate`, chrono usage
- PostgreSQL: no built-in equivalent; MariaDB: `DAYOFWEEK` + user-defined tables
