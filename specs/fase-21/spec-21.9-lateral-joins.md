# Spec: 21.9 LATERAL joins

Phase: 21 — Advanced SQL
Task: LATERAL subquery joins
Status: approved

## Context

LATERAL joins allow a subquery on the right side of a FROM/JOIN clause to
reference columns from tables to its left. They are a standard SQL:1999
feature, widely used in PostgreSQL and MySQL 8.0+ for per-row correlated
sub-selects, function-table joins, and computed columns. AxiomDB already has
the lexer token, AST field, parser dispatch, and SELECT executor path. This
spec closes the phase by cleaning up debug artifacts, fixing the DML join path,
and formalising the documented surface.

## Goal

Deliver a production-clean LATERAL join implementation across SELECT, UPDATE,
and DELETE joins, with full test coverage and docs-site update.

## Non-goals

- RIGHT LATERAL / FULL LATERAL — rejected at parse/execute time (PG-compatible)
- LATERAL on the first FROM source as correlated (no left context to bind to;
  a non-correlated LATERAL first-FROM already works)
- INSERT … LATERAL — not a valid SQL construct
- Planner push-down / index use for LATERAL predicates — Phase 27

## Behavior

### SQL surface

```sql
-- Comma = implicit CROSS JOIN LATERAL
SELECT t.id, sub.val
FROM t, LATERAL (SELECT t.id + 10 AS val FROM other o WHERE o.t_id = t.id) sub

-- Explicit INNER JOIN LATERAL
SELECT t.id, sub.val
FROM t JOIN LATERAL (...) sub ON true

-- LEFT JOIN LATERAL — null-pads when subquery returns no rows
SELECT t.id, sub.val
FROM t LEFT JOIN LATERAL (...) sub ON true

-- CROSS JOIN LATERAL — cartesian with subquery output
SELECT t.id, sub.x
FROM t CROSS JOIN LATERAL (SELECT 1 AS x UNION ALL SELECT 2) sub

-- Chained LATERAL — second LATERAL sees first's output columns
SELECT * FROM (VALUES (1)) t(v),
  LATERAL (SELECT t.v + 10 AS a) s1,
  LATERAL (SELECT s1.a * 2 AS b) s2

-- LATERAL as first FROM (no outer context, non-correlated)
SELECT * FROM LATERAL (SELECT 1 AS x) init

-- LATERAL in UPDATE JOIN
UPDATE target t
  JOIN LATERAL (SELECT ...) sub ON sub.id = t.id
SET t.col = sub.val

-- LATERAL in DELETE JOIN
DELETE t FROM target t
  JOIN LATERAL (SELECT ...) sub ON sub.id = t.id
```

### Semantics

**Correlation detection** (`select_joins_ctx.rs`)
The subquery is considered correlated when `lateral=true` AND the subquery
body references any `OuterColumn` node whose index is within the current
`effective_left_cols` count (all materialized left sources + accumulated
LATERAL columns from earlier lateral subqueries in the same FROM).

**Materialization strategy**
- Non-correlated subquery (lateral=false OR correlated=false): execute once,
  cache result, use for every outer row — same as a regular derived table.
- Correlated LATERAL subquery: store AST, per outer row substitute
  `OuterColumn(i)` nodes with the actual value from the outer row, execute,
  collect result rows.

**Placeholder columns** (correlated path)
Because a correlated subquery cannot be run at setup time, the schema of its
output is inferred from the SELECT list at parse/analysis time (names from
aliases or fallback `colN`). This schema is used for `running_offset`,
null-padding in LEFT JOIN, and column name resolution by downstream projections.

**JOIN type semantics**
| Join type | No subquery rows | One+ subquery rows |
|-----------|------------------|--------------------|
| INNER / CROSS | outer row dropped | joined rows emitted |
| LEFT | outer row with NULL right | joined rows emitted |
| RIGHT | NotImplemented | NotImplemented |
| FULL | NotImplemented | NotImplemented |

**ON condition**
Evaluated against the combined row after the subquery yields each result.
`ON true` is the canonical form for LATERAL (condition is satisfied if the
subquery produced the row).

**Chained LATERAL**
Each successive LATERAL in the same FROM clause can reference all columns
accumulated from prior sources (tables + earlier LATERALs). The
`lateral_accum_cols` counter advances by the column count of each correlated
LATERAL added so that `subquery_is_correlated` computes the correct
`effective_left_cols` for the next subquery.

**DML LATERAL**
In `dml_join.rs`, LATERAL subqueries must use the same placeholder-column
strategy as SELECT joins. Executing the subquery with an empty scope to derive
column names is incorrect for correlated queries (the outer columns are not
yet substituted) and must be replaced with AST-driven placeholder inference.

### Error cases

| Condition | Error | Message |
|-----------|-------|---------|
| `RIGHT JOIN LATERAL (...)` | `DbError::NotImplemented` | contains "RIGHT" |
| `FULL JOIN LATERAL (...)` | `DbError::NotImplemented` | contains "FULL" |
| LATERAL first-FROM with outer column ref | `DbError::ParseError` | correlated first-FROM not allowed |

## Edge cases

- [ ] LATERAL inner join — outer row with no match is dropped
- [ ] LATERAL left join — outer row with no match is null-padded
- [ ] LATERAL cross join — cartesian with multi-row subquery
- [ ] LATERAL as first FROM, non-correlated (no left context)
- [ ] Chained LATERALs (second references first's columns)
- [ ] LATERAL referencing multiple outer columns
- [ ] RIGHT JOIN LATERAL → NotImplemented
- [ ] FULL JOIN LATERAL → NotImplemented
- [ ] Non-LATERAL subquery JOIN still works (regression)
- [ ] LATERAL in UPDATE JOIN (correlated)
- [ ] LATERAL in DELETE JOIN (correlated)
- [ ] LATERAL with UNION ALL subquery body

## Performance budget

LATERAL execution is inherently O(left_rows × subquery_cost). No budget target
beyond "no unnecessary re-materialization for non-correlated LATERAL".

## Dependencies

- Depends on: Phase 11.20d3 (correlated JSON_TABLE — same `substitute_outer`
  pattern), Phase 21.22 (VALUES inline — `select_joins_ctx.rs` already has the
  per-join-type dispatch structure)
- Blocks: nothing (standalone feature)

## Done criteria

- [ ] No `eprintln!` / debug prints in `src/` (select_joins_ctx.rs cleaned)
- [ ] `dml_join.rs` LATERAL path uses placeholder columns, not empty-scope execution
- [ ] All 8 existing integration tests in `integration_lateral_join.rs` pass
- [ ] New tests: `lateral_update_join`, `lateral_delete_join` pass
- [ ] `cargo nextest run -p axiomdb-sql` passes (in Lima)
- [ ] `cargo clippy --workspace -- -D warnings` clean (in Lima)
- [ ] `cargo fmt --check` clean (in Lima)
- [ ] Wire smoke: 2+ new `[21.9 LATERAL]` assertions pass
- [ ] `docs-site/src/user-guide/sql-reference/dml.md` updated with LATERAL syntax
- [ ] `docs-site/src/internals/sql-parser.md` updated with LATERAL join design notes
- [ ] `docs/progreso.md` marks 21.9 ✅
- [ ] Commit pushed to origin/main

## References

- PostgreSQL: `src/backend/parser/gram.y` — `joined_table` → `LATERAL opt_alias`
- PostgreSQL: `src/backend/executor/nodeNestloop.c` — per-outer-row inner re-scan
- MySQL 8.0: "Lateral Derived Tables" in reference manual
- Related impl: `crates/axiomdb-sql/src/executor/select_joins_ctx.rs`
- Related impl: `crates/axiomdb-sql/src/executor/joins.rs::apply_correlated_subquery_join`
- Related impl: `crates/axiomdb-sql/src/executor/dml_join.rs`
- Similar pattern: Phase 11.20d3 LATERAL JSON_TABLE (`apply_correlated_jt_join`)
