# Spec: GENERATE_SERIES table-valued function

Phase: 20 — Types + import/export
Task: 20.10 — GENERATE_SERIES
Status: approved

## Context

Phase 20.4 added UNNEST as a set-returning function in FROM, establishing the
`FromClause::Unnest` → `unnest.rs` → `select_core.rs` dispatch pattern. Phase
11.25 added JsonbSRF following the same pattern. GENERATE_SERIES is the next
SRF in the same family: a table-valued function that produces a sequence of
integer or date values without needing a real table. It is one of the most
commonly used PostgreSQL built-ins for reporting, test-data generation, and
calendar arithmetic.

## Goal

Implement `GENERATE_SERIES(start, stop [, step])` as a FROM-position
table-valued function that produces a row-per-value sequence of integers
or dates, following PostgreSQL semantics.

## Non-goals

- `GENERATE_SERIES` as a scalar expression in SELECT (not a TVF there) — error
- `timestamp` / `timestamptz` variant — deferred until `Interval` is a native type
- `numeric` / `float` variant — deferred; integers + dates cover 95% of use cases
- Lazy/cursor-based streaming — all series are materialized in memory (20.8 scope)
- `GENERATE_SUBSCRIPTS` — separate SRF, not in this spec

## Behavior

### SQL syntax

```sql
-- Integer series, step defaults to 1
SELECT * FROM GENERATE_SERIES(1, 10) AS g(n);

-- Integer series with explicit step
SELECT * FROM GENERATE_SERIES(1, 10, 2) AS g(n);   -- 1,3,5,7,9
SELECT * FROM GENERATE_SERIES(10, 1, -1) AS g(n);  -- 10,9,...,1

-- Without alias — default column name is "generate_series"
SELECT * FROM GENERATE_SERIES(1, 5);

-- Date series: step is a string interval literal
SELECT * FROM GENERATE_SERIES('2024-01-01'::date, '2024-12-31'::date, '1 month');
SELECT * FROM GENERATE_SERIES('2024-01-01'::date, '2024-06-30'::date, '1 week');

-- In JOIN
SELECT t.id, g.n
FROM t
JOIN GENERATE_SERIES(1, 5) AS g(n) ON t.id = g.n;

-- In subquery / CTE
WITH nums AS (SELECT * FROM GENERATE_SERIES(1, 100) AS g(n))
SELECT SUM(n) FROM nums;
```

### AST

```rust
/// Phase 20.10 — `FROM GENERATE_SERIES(start, stop [, step]) [AS alias(col)]`
pub struct GenerateSeriesClause {
    pub start: Expr,
    pub stop: Expr,
    pub step: Option<Expr>,         // None → default step 1 (int) or 1 day (date)
    pub alias: Option<String>,
    pub column_name: Option<String>, // explicit col name from AS alias(col)
}
```

Added to `FromClause`:
```rust
pub enum FromClause {
    // ... existing variants ...
    /// Phase 20.10 — `FROM GENERATE_SERIES(start, stop [, step])`.
    GenerateSeries(Box<GenerateSeriesClause>),
}
```

### Materialization API

```rust
// crates/axiomdb-sql/src/generate_series.rs

pub fn materialize_generate_series(
    start: &Value,
    stop: &Value,
    step: &Value,
) -> Result<Vec<Vec<Value>>, DbError>;

pub fn column_metas_for_generate_series(gs: &GenerateSeriesClause) -> Vec<ColumnMeta>;

pub fn column_defs_for_generate_series(gs: &GenerateSeriesClause) -> Vec<ColumnDef>;
```

### Semantics — integer variant

- Types: `Int` or `BigInt`. If start/stop/step are `Int`, output is `Int`.
  If any is `BigInt`, all coerce to `BigInt`.
- `step` must not be zero → `DbError::InvalidValue`.
- `step` defaults to `1` when omitted.
- **Ascending** (`step > 0`): yields values `start, start+step, ...` while `value <= stop`.
- **Descending** (`step < 0`): yields values `start, start+step, ...` while `value >= stop`.
- Empty result (no error) when: ascending and `start > stop`, or descending and `start < stop`.
- Max rows: 10,000,000 — returns `DbError::InvalidValue` if exceeded (guard against runaway).

Examples:
| Call | Result |
|------|--------|
| `(1, 5)` | 1,2,3,4,5 |
| `(1, 5, 2)` | 1,3,5 |
| `(5, 1, -1)` | 5,4,3,2,1 |
| `(5, 1)` | *(empty)* |
| `(1, 5, 0)` | error |
| `(1, 5, -1)` | *(empty)* |

### Semantics — date variant

- Recognized when `start` and `stop` evaluate to `Value::Date`.
- `step` is required for date series and must be a `Value::Text` string parsed as
  a simple interval: `'N unit'` where unit is one of:
  `day`/`days`, `week`/`weeks`, `month`/`months`, `year`/`years`.
  Example: `'1 month'`, `'7 days'`, `'2 weeks'`, `'1 year'`.
- Arithmetic uses `chrono::NaiveDate` for correct calendar semantics (months
  respect varying lengths, years handle leap years).
- Ascending only when step is positive (negative intervals not supported in
  this subphase → `DbError::NotImplemented`).
- `Value::Date` is stored as days since 1970-01-01 (i32).
- Max rows: 10,000,000 (same guard).

Examples:
| Call | Result |
|------|--------|
| `('2024-01-01', '2024-03-01', '1 month')` | 2024-01-01, 2024-02-01, 2024-03-01 |
| `('2024-01-01', '2024-01-07', '1 day')` | 7 rows |
| `('2024-01-01', '2024-12-31', '1 year')` | 2024-01-01 |

### Column naming

| Situation | Column name |
|---|---|
| `AS g(n)` explicit | `n` |
| `AS g` alias only | `generate_series` |
| No alias at all | `generate_series` |

### Error cases

| Situation | Error | Message |
|---|---|---|
| step = 0 | `DbError::InvalidValue` | `"GENERATE_SERIES: step must not be zero"` |
| > 10M rows | `DbError::InvalidValue` | `"GENERATE_SERIES: result exceeds 10,000,000 rows"` |
| date step missing | `DbError::InvalidValue` | `"GENERATE_SERIES: step is required for date series"` |
| date step unparseable | `DbError::InvalidValue` | `"GENERATE_SERIES: unrecognized step '...'; expected 'N day(s|week(s|month(s|year(s)'"` |
| date negative step | `DbError::NotImplemented` | `"GENERATE_SERIES: negative interval step not yet supported"` |
| mixed types (int start, date stop) | `DbError::InvalidValue` | `"GENERATE_SERIES: start and stop must have the same type"` |
| non-int/date start | `DbError::InvalidValue` | `"GENERATE_SERIES: unsupported type ...; expected Int, BigInt, or Date"` |

## Edge cases

- [ ] `GENERATE_SERIES(n, n)` → exactly 1 row
- [ ] `GENERATE_SERIES(5, 1)` → 0 rows (no error)
- [ ] `GENERATE_SERIES(1, 5, 0)` → error
- [ ] `GENERATE_SERIES(1, 5, -1)` → 0 rows
- [ ] `GENERATE_SERIES(5, 1, -1)` → 5,4,3,2,1
- [ ] BigInt args: `GENERATE_SERIES(0, 9999999999)` handled (hits row limit)
- [ ] Int args that overflow when coerced → BigInt path
- [ ] Date series end-of-month: `('2024-01-31', '2024-03-31', '1 month')` → 3 rows using chrono
- [ ] JOIN: `FROM t JOIN GENERATE_SERIES(1,5) AS g(n) ON t.id = g.n`
- [ ] CTE: `WITH s AS (SELECT * FROM GENERATE_SERIES(1,3) AS g(n)) SELECT * FROM s`
- [ ] NULL start/stop → `DbError::InvalidValue`

## Performance budget

| Operation | Target | Max acceptable |
|---|---|---|
| `GENERATE_SERIES(1, 100000)` | < 5 ms | 20 ms |
| `GENERATE_SERIES(date, date, '1 day')` 365 rows | < 1 ms | 5 ms |

No storage I/O — pure in-memory generation. Budget is generous.

## Dependencies

- Depends on: `chrono` (workspace dep, already present), Phase 20.4 UNNEST pattern
- Blocks: nothing

## Open questions

*(all resolved)*

- Date support: **yes** (chrono already available)
- JOIN support: **yes** (free via UNNEST pattern)
- Default column name: **`generate_series`** (PostgreSQL parity)
- Timestamp support: **deferred** (no Interval type)

## Done criteria

- [ ] `FROM GENERATE_SERIES(int, int)` works with default step 1
- [ ] `FROM GENERATE_SERIES(int, int, int)` works with explicit step (pos + neg)
- [ ] `FROM GENERATE_SERIES(date, date, 'N unit')` works for day/week/month/year
- [ ] Empty result cases return 0 rows, no error
- [ ] step=0 returns `DbError::InvalidValue`
- [ ] Row limit guard (>10M) returns `DbError::InvalidValue`
- [ ] JOIN with GENERATE_SERIES works
- [ ] CTE with GENERATE_SERIES works
- [ ] Default column name is `generate_series`, overridable via `AS alias(col)`
- [ ] 8+ parser tests pass
- [ ] 15+ executor integration tests pass
- [ ] Wire test: 4+ assertions, total passes
- [ ] `cargo nextest run --workspace` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `docs-site/src/user-guide/sql-reference/dml.md` updated with GENERATE_SERIES section

## References

- PostgreSQL: `research/postgres/src/backend/utils/adt/int8.c:1385` (generate_series_step_int8)
- PostgreSQL: `research/postgres/src/backend/utils/adt/timestamp.c:56` (timestamp variant, FYI)
- Phase 20.4 UNNEST pattern: `crates/axiomdb-sql/src/unnest.rs`
- Phase 11.25 JsonbSRF pattern: `crates/axiomdb-sql/src/jsonb_srf.rs`
- PG docs: https://www.postgresql.org/docs/current/functions-srf.html
