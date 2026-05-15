# Plan: GENERATE_SERIES table-valued function

Phase: 20 — Types + import/export
Task: 20.10 — GENERATE_SERIES
Spec: specs/fase-20/spec-20.10-generate-series.md
Status: done

## Summary

Three steps following the established UNNEST/JsonbSRF pattern exactly.
Step 1 adds the AST variant and parser (compilable, parser-tests only).
Step 2 wires the generate_series.rs materialization module through all
14 match sites (analyzer × 4, executor × 6, plan_deps, lib.rs) and adds
the executor integration tests.
Step 3 closes with wire smoke, docs, and the workspace gate.

## Dependencies

Must be done first:
- [x] spec-20.10-generate-series.md approved

Blocks:
- nothing

## Affected files

New files:
- `crates/axiomdb-sql/src/generate_series.rs` — materialize_generate_series + column helpers
- `crates/axiomdb-sql/tests/integration_generate_series_parser.rs` — 8 parser tests
- `crates/axiomdb-sql/tests/integration_generate_series.rs` — 15 executor tests

Modified files:
- `crates/axiomdb-sql/src/ast.rs` — GenerateSeriesClause struct + FromClause::GenerateSeries
- `crates/axiomdb-sql/src/lib.rs` — pub mod generate_series
- `crates/axiomdb-sql/src/parser/dml.rs` — FROM GENERATE_SERIES parse + 3 match arms
- `crates/axiomdb-sql/src/analyzer_bind.rs` — virtual table binding
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — JOIN expr resolution arm
- `crates/axiomdb-sql/src/analyzer_ddl.rs` — NotImplemented arm
- `crates/axiomdb-sql/src/analyzer_pivot.rs` — alias extraction arm
- `crates/axiomdb-sql/src/plan_deps.rs` — plan deps arm
- `crates/axiomdb-sql/src/executor/select_core.rs` — dispatch + execute function
- `crates/axiomdb-sql/src/executor/select_ctx.rs` — matches! guard
- `crates/axiomdb-sql/src/executor/select_helpers.rs` — match arm
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` — JOIN materialization
- `crates/axiomdb-sql/src/executor/dml_join.rs` — DML join arm
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — explain arm
- `tools/wire-test.py` — 4 new GENERATE_SERIES assertions
- `docs-site/src/user-guide/sql-reference/dml.md` — GENERATE_SERIES section
- `docs/progreso.md` — mark 20.10 done

---

## Step 1 — AST + parser + parser tests

**Goal:** `FromClause::GenerateSeries` parses correctly; all existing match arms compile.
**Files:** `ast.rs`, `parser/dml.rs`, `tests/integration_generate_series_parser.rs`

### AST additions (ast.rs)

```rust
/// Phase 20.10 — `FROM GENERATE_SERIES(start, stop [, step]) [AS alias(col)]`
#[derive(Debug, Clone, PartialEq)]
pub struct GenerateSeriesClause {
    pub start: Expr,
    pub stop: Expr,
    pub step: Option<Expr>,
    pub alias: Option<String>,
    pub column_name: Option<String>,
}
```

Add to `FromClause` enum (after `Unnest`):
```rust
/// Phase 20.10 — `FROM GENERATE_SERIES(start, stop [, step])`.
GenerateSeries(Box<GenerateSeriesClause>),
```

### Parser addition (parser/dml.rs)

In `parse_from_item`, alongside the UNNEST branch, detect `GENERATE_SERIES`
as a case-insensitive identifier (not a reserved token — same as COPY):

```rust
// GENERATE_SERIES — case-insensitive identifier check
Token::Ident(s) | Token::QuotedIdent(s)
    if s.eq_ignore_ascii_case("generate_series") =>
{
    p.advance();
    p.expect(&Token::LParen)?;
    let start = parse_expr(p)?;
    p.expect(&Token::Comma)?;
    let stop = parse_expr(p)?;
    let step = if p.eat(&Token::Comma) {
        Some(parse_expr(p)?)
    } else {
        None
    };
    p.expect(&Token::RParen)?;

    // Optional: AS alias(col) or AS alias
    let (alias, column_name) = parse_srf_alias(p)?;

    FromClause::GenerateSeries(Box::new(GenerateSeriesClause {
        start, stop, step, alias, column_name,
    }))
}
```

Helper `parse_srf_alias` (reused from UNNEST pattern):
```rust
// Returns (table_alias, col_alias)
// AS g(n)  → (Some("g"), Some("n"))
// AS g     → (Some("g"), None)
// (none)   → (None, None)
fn parse_srf_alias(p: &mut Parser) -> Result<(Option<String>, Option<String>), DbError> {
    if !p.eat(&Token::As) { return Ok((None, None)); }
    let alias = p.parse_identifier()?;
    if p.eat(&Token::LParen) {
        let col = p.parse_identifier()?;
        p.expect(&Token::RParen)?;
        Ok((Some(alias), Some(col)))
    } else {
        Ok((Some(alias), None))
    }
}
```

Add `GenerateSeries` to the 3 match arms in `parser/dml.rs` that enumerate
`FromClause` variants (subquery alias extraction × 2, table-alias helpers × 1):
```rust
| FromClause::GenerateSeries(_) => { /* same as Unnest arm */ }
```

### Parser tests

```rust
// tests/integration_generate_series_parser.rs
use axiomdb_sql::{ast::{FromClause, GenerateSeriesClause, Stmt}, parse};

fn parse_gs(sql: &str) -> GenerateSeriesClause { ... }

#[test] fn parse_gs_basic_two_args() { /* (1, 10) → start=1,stop=10,step=None */ }
#[test] fn parse_gs_with_step() { /* (1, 10, 2) */ }
#[test] fn parse_gs_with_alias_and_col() { /* AS g(n) */ }
#[test] fn parse_gs_alias_only() { /* AS g */ }
#[test] fn parse_gs_no_alias() { /* no AS → alias=None, col=None */ }
#[test] fn parse_gs_negative_step() { /* (10, 1, -1) */ }
#[test] fn parse_gs_date_args_with_string_step() { /* ('2024-01-01'::date, ..., '1 month') */ }
#[test] fn parse_gs_in_select_from() { /* SELECT * FROM GENERATE_SERIES(1,5) */ }
```

### Verification

```bash
cargo nextest run -p axiomdb-sql --test integration_generate_series_parser
cargo clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-20): AST GenerateSeriesClause + parser GENERATE_SERIES (20.10 step 1)
```

---

## Step 2 — generate_series.rs module + full wiring + executor tests

**Goal:** `SELECT * FROM GENERATE_SERIES(...)` executes correctly end-to-end.
**Files:** `generate_series.rs` (new), `lib.rs`, all 14 match sites, executor tests.

### generate_series.rs

```rust
// crates/axiomdb-sql/src/generate_series.rs

pub fn column_metas_for_generate_series(gs: &GenerateSeriesClause) -> Vec<ColumnMeta> {
    let name = gs.column_name
        .clone()
        .unwrap_or_else(|| "generate_series".to_string());
    vec![ColumnMeta {
        name,
        data_type: DataType::BigInt, // placeholder; executor emits actual types
        nullable: false,
        table_name: Some(gs.alias.clone().unwrap_or_else(|| "generate_series".into())),
    }]
}

pub fn column_defs_for_generate_series(gs: &GenerateSeriesClause) -> Vec<ColumnDef> { ... }

pub fn materialize_generate_series(
    start: &Value,
    stop: &Value,
    step: &Value,
) -> Result<Vec<Vec<Value>>, DbError> {
    match (start, stop) {
        (Value::Int(_) | Value::BigInt(_), Value::Int(_) | Value::BigInt(_)) => {
            materialize_int_series(to_i64(start), to_i64(stop), to_i64(step)?)
        }
        (Value::Date(s), Value::Date(e)) => {
            materialize_date_series(*s, *e, step)
        }
        _ => Err(DbError::InvalidValue { reason: "...".into() })
    }
}

fn materialize_int_series(start: i64, stop: i64, step: i64)
    -> Result<Vec<Vec<Value>>, DbError>
{
    if step == 0 { return Err(...); }
    let mut out = Vec::new();
    let mut cur = start;
    loop {
        if step > 0 && cur > stop { break; }
        if step < 0 && cur < stop { break; }
        if out.len() >= 10_000_000 { return Err(...); }
        // Use Int if all values fit in i32, else BigInt
        out.push(vec![if fits_i32(cur) { Value::Int(cur as i32) }
                       else { Value::BigInt(cur) }]);
        cur = cur.wrapping_add(step);
    }
    Ok(out)
}

fn materialize_date_series(start: i32, stop: i32, step: &Value)
    -> Result<Vec<Vec<Value>>, DbError>
{
    // parse step string e.g. "1 month", "7 days", "1 year", "2 weeks"
    let (n, unit) = parse_interval_str(step)?;
    let mut out = Vec::new();
    let mut cur = days_to_naive(start)?;
    let end = days_to_naive(stop)?;
    loop {
        if cur > end { break; }
        if out.len() >= 10_000_000 { return Err(...); }
        out.push(vec![Value::Date(naive_to_days(cur))]);
        cur = advance_date(cur, n, unit)?;
    }
    Ok(out)
}
```

### Wiring: all 14 sites (pattern identical to UNNEST)

| File | What to add |
|---|---|
| `lib.rs` | `pub mod generate_series;` |
| `analyzer_bind.rs:470` | `FromClause::GenerateSeries(gs) => { use column_defs_for_generate_series }` |
| `analyzer_stmt.rs:478` | `FromClause::GenerateSeries(mut gs) => { resolve start/stop/step exprs; join.table = ... }` |
| `analyzer_ddl.rs:578` | `FromClause::GenerateSeries(_) => Err(NotImplemented)` |
| `analyzer_pivot.rs:33` | `FromClause::GenerateSeries(gs) => gs.alias.clone().unwrap_or("generate_series")` |
| `plan_deps.rs:392` | `FromClause::GenerateSeries(gs) => { collect_expr_deps(start/stop/step) }` |
| `select_core.rs:97` | add `|| matches!(stmt.from, Some(FromClause::GenerateSeries(_)))` to the guard |
| `select_core.rs:104` | update unreachable message |
| `select_core.rs` | add `execute_select_generate_series_source` function |
| `select_ctx.rs:52` | add `|| matches!(stmt.from, Some(FromClause::GenerateSeries(_)))` |
| `select_helpers.rs:98` | `FromClause::GenerateSeries(_) => { /* same as Unnest */ }` |
| `select_joins_ctx.rs:278` | `FromClause::GenerateSeries(gs) => { materialize + build source }` |
| `dml_join.rs:501` | `FromClause::GenerateSeries(_) => Err(NotImplemented for DML)` |
| `exec_explain.rs:343` | `FromClause::GenerateSeries(_) => { write "GENERATE_SERIES" }` |

### execute_select_generate_series_source (select_core.rs)

Mirrors `execute_select_unnest_source` exactly:
1. Extract `GenerateSeriesClause` from `stmt.from`
2. Evaluate `start`, `stop`, `step` via `eval()`
3. Call `materialize_generate_series(&start_val, &stop_val, &step_val)`
4. Get `derived_cols` from `column_metas_for_generate_series`
5. If joins: route through `execute_select_with_joins_first_materialized`
6. Otherwise: apply WHERE, GROUP BY, ORDER BY, LIMIT

### Executor integration tests

```rust
// tests/integration_generate_series.rs
#[test] fn gs_basic_1_to_5()           // 5 rows: 1,2,3,4,5
#[test] fn gs_with_step_2()            // (1,10,2) → 1,3,5,7,9
#[test] fn gs_descending()             // (5,1,-1) → 5,4,3,2,1
#[test] fn gs_empty_asc_inverted()     // (5,1) → 0 rows
#[test] fn gs_empty_desc_inverted()    // (1,5,-1) → 0 rows
#[test] fn gs_single_row()             // (3,3) → 1 row: 3
#[test] fn gs_step_zero_errors()       // step=0 → error
#[test] fn gs_with_where_filter()      // SELECT n FROM gs(1,10) WHERE n > 5
#[test] fn gs_with_alias_and_col()     // AS g(n) → col name is "n"
#[test] fn gs_default_col_name()       // no alias → col name is "generate_series"
#[test] fn gs_in_join()                // FROM t JOIN GENERATE_SERIES(1,5) AS g(n) ON t.id=g.n
#[test] fn gs_in_cte()                 // WITH s AS (SELECT * FROM gs(1,3)) SELECT SUM(n) FROM s
#[test] fn gs_date_monthly()           // ('2024-01-01','2024-03-01','1 month') → 3 rows
#[test] fn gs_date_daily()             // ('2024-01-01','2024-01-07','1 day') → 7 rows
#[test] fn gs_date_missing_step_errors() // date series without step → error
```

### Verification

```bash
cargo nextest run -p axiomdb-sql --test integration_generate_series
cargo nextest run -p axiomdb-sql --test integration_generate_series_parser
cargo clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-20): generate_series.rs + full executor wiring (20.10 step 2)
```

---

## Step 3 — Wire test + docs + workspace close

**Goal:** 542/542 wire test; full workspace clean; subphase marked done.

### Wire test additions (tools/wire-test.py)

```python
# ── Phase 20.10: GENERATE_SERIES ─────────────────────────────────────────────
cur.execute("SELECT COUNT(*) FROM (SELECT * FROM GENERATE_SERIES(1,100)) AS g")
cnt = cur.fetchone()[0]
ok("[20.10 gs_int] GENERATE_SERIES(1,100) produces 100 rows", int(cnt)==100, cnt)

cur.execute("SELECT COUNT(*) FROM (SELECT * FROM GENERATE_SERIES(1,10,2)) AS g")
cnt = cur.fetchone()[0]
ok("[20.10 gs_step] GENERATE_SERIES(1,10,2) produces 5 rows", int(cnt)==5, cnt)

cur.execute("SELECT COUNT(*) FROM (SELECT * FROM GENERATE_SERIES(5,1,-1)) AS g")
cnt = cur.fetchone()[0]
ok("[20.10 gs_desc] GENERATE_SERIES(5,1,-1) produces 5 rows", int(cnt)==5, cnt)

cur.execute("""
SELECT COUNT(*) FROM (
  SELECT * FROM GENERATE_SERIES('2024-01-01', '2024-12-01', '1 month')
) AS g
""")
cnt = cur.fetchone()[0]
ok("[20.10 gs_date] GENERATE_SERIES monthly date series produces 12 rows",
   int(cnt)==12, cnt)
```

Note: wire test uses untyped string dates — server coerces via existing Date
coercion path. If wire driver doesn't support `::date` cast syntax, use
`CAST('2024-01-01' AS DATE)` instead.

### Docs update

Add GENERATE_SERIES section to `docs-site/src/user-guide/sql-reference/dml.md`:
- Syntax, integer examples, date examples, JOIN example, column naming, errors.

### Closing protocol

```bash
cargo nextest run --workspace           # 3906+ tests, 0 failures
cargo clippy --workspace -- -D warnings # clean
cargo fmt --check                       # clean
python3 tools/wire-test.py              # 542/542
```

### Commit

```
feat(fase-20): complete GENERATE_SERIES SRF — int, date, JOIN, CTE (20.10)

Implements specs/fase-20/spec-20.10-generate-series.md
Tests: 8 parser + 15 executor + 4 wire assertions → 542/542
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Forgotten match arm (won't compile) | low | compiler enforces exhaustiveness; `-D warnings` catches it |
| Date string cast in wire test rejected | medium | use `CAST(... AS DATE)` fallback if `::date` syntax fails over wire |
| `chrono::NaiveDate::checked_add_months` absent on older chrono | low | verify chrono version in Cargo.toml; use `months_since` API |
| Integer overflow wrapping_add for very large ranges | low | row-limit guard at 10M catches it before overflow matters |

## Rollback plan

1. `git reset --hard <commit before step 1>`
2. Branch `abandoned/plan-20.10-generate-series-<date>` if partial
3. Set spec status back to `draft`

## Estimated effort

Total: ~2 hours
- Step 1: 30 min (AST + parser + 8 tests)
- Step 2: 60 min (module + 14 match arms + 15 tests)
- Step 3: 30 min (wire + docs + close)
