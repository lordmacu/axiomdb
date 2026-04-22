# Spec: 21.25 — PIVOT dynamic

Phase: 21 — Advanced SQL
Task: 21.25 PIVOT dynamic
Status: implemented

## Context

Phase 21 already added several high-value SQL reshaping features around
derived tables and grouped execution: CTEs, LATERAL joins, VALUES in FROM,
GROUPING SETS, cursors, and advanced acceptance coverage. The roadmap entry
for `21.25` says "PIVOT dynamic" and gives this example:

```sql
SELECT * FROM sales
PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb', 'Mar', 'Apr'))
```

The example already uses an explicit `IN (...)` list. That is the correct
architectural cut for AxiomDB today. A truly runtime-discovered pivot that
creates columns based on scanned data would conflict with the current
analyzer/binder model, where a `FROM` item must publish a stable schema
before execution (`BindContext`, virtual columns from subqueries, prepared
statement metadata, wire result metadata).

## Goal

Implement a real SQL `PIVOT` operator with explicit pivot values in
`IN (...)`, producing row-to-column reshaping through a validated rewrite to
standard grouped aggregation.

## Non-goals

- Auto-discovering pivot columns from source data at execution time.
- `UNPIVOT` support.
- Multiple aggregate expressions inside one `PIVOT`.
- SQL Server / Oracle full pivot syntax parity.
- Pivot value subqueries or arbitrary expressions inside `IN (...)`.
- Optimizer-specific pivot planning beyond a rewrite to existing grouped
  execution.

## Behavior

### Public SQL surface

Supported syntax:

```sql
SELECT ...
FROM source
PIVOT (
    agg_func(value_expr)
    FOR pivot_expr
    IN (literal [, literal ...])
) [AS alias]
```

Bounded MVP rules:

- `source` is one existing `FROM` item that AxiomDB already supports
  (`table`, subquery, inline `VALUES`, `JSON_TABLE`, etc.) and the pivoted
  result behaves like a derived table for the outer query.
- Exactly one aggregate expression is allowed in the pivot clause.
- `agg_func(value_expr)` must be an aggregate function already supported by
  grouped SELECT execution and must take exactly one expression argument.
- `pivot_expr` is any scalar expression resolvable against the source row.
- `IN (...)` accepts only literal pivot values.
- Optional `AS alias` names the pivoted derived table.

Examples that must work:

```sql
SELECT *
FROM sales
PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb', 'Mar'));

SELECT region, jan, feb
FROM (
  SELECT region, month, amount FROM sales
) PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb')) AS p
ORDER BY region;

SELECT *
FROM product_sales
PIVOT (MAX(total) FOR quarter IN ('Q1', 'Q2', 'Q3', 'Q4')) AS p;
```

### Semantics

`PIVOT` rewrites to a grouped derived query over the source rows.

Given:

```sql
SELECT *
FROM sales
PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb'))
```

the effective semantics are equivalent to:

```sql
SELECT region,
       SUM(CASE WHEN month = 'Jan' THEN amount ELSE NULL END) AS Jan,
       SUM(CASE WHEN month = 'Feb' THEN amount ELSE NULL END) AS Feb
FROM sales
GROUP BY region
```

Output columns:

1. Passthrough grouping columns from the source.
2. One generated column per pivot literal in `IN (...)`.

Passthrough grouping columns are defined as all source output columns that are
not referenced by:

- the pivot discriminator expression (`pivot_expr`), or
- the aggregate input expression (`value_expr`).

This means:

- `SELECT * FROM sales PIVOT (SUM(amount) FOR month IN ('Jan','Feb'))`
  over source columns `(region, month, amount)` returns
  `(region, Jan, Feb)`.
- If the source has multiple non-pivot, non-value columns, they all become
  grouping columns in declaration order.
- If no passthrough columns remain, the pivot returns a single aggregated row.

Generated pivot column names:

- Default name = string form of the pivot literal without surrounding SQL
  quotes, preserving case.
- Duplicate generated names are rejected.
- If a generated pivot column name collides with a passthrough source column
  name, the statement is rejected.

Aggregate semantics:

- Each generated column aggregates only rows where `pivot_expr = literal`.
- Rows that do not match a given literal contribute `NULL` to that generated
  aggregate expression.
- Existing grouped aggregate behavior remains the source of truth for `SUM`,
  `MIN`, `MAX`, `AVG`, and any other single-arg aggregate allowed by the
  implementation cut.
- `NULL` pivot keys do not match any pivot literal.

Outer-query behavior:

- The pivoted result is a normal derived table.
- `SELECT *` expands to passthrough columns followed by generated pivot
  columns in `IN (...)` order.
- `ORDER BY`, `LIMIT`, joins, and projection in the outer query operate on
  the pivoted output schema.
- If filtering is needed before pivoting, the user must place it inside the
  source subquery.

### Rewrite contract

Implementation may use a dedicated AST node during parsing/analyzer stages,
but before execution the pivoted source must be transformed into a regular
derived SELECT that uses only already-supported SQL primitives:

- projection
- `CASE WHEN`
- grouped aggregation
- derived table aliasing

This keeps runtime execution inside existing grouped SELECT code paths.

### Error cases

| Case | Expected error |
|------|----------------|
| `PIVOT` without `IN (...)` | parse error |
| more than one aggregate expression in the pivot clause | parse or analysis error |
| aggregate has zero or multiple arguments | analysis error |
| pivot `IN` item is not a literal | parse or analysis error |
| duplicate pivot literals that normalize to the same output name | analysis error |
| generated pivot column name collides with passthrough column name | analysis error |
| source schema cannot be resolved for the pivoted item | existing table/subquery analysis error |
| unsupported nested `PIVOT` / `UNPIVOT` syntax | parse error |

## Edge cases

- [ ] Source has no rows: result has zero rows when passthrough grouping
      columns exist, or one all-NULL aggregate row when the grouped execution
      already behaves that way for empty-input global aggregates.
- [ ] Source has multiple passthrough grouping columns.
- [ ] No passthrough columns remain after excluding pivot/value expressions.
- [ ] A pivot literal has no matching source rows: generated column returns
      the aggregate's normal no-match behavior (`NULL` for `SUM`/`MAX`/`MIN`/`AVG`).
- [ ] `pivot_expr` evaluates to `NULL`: it matches no pivot literal.
- [ ] Source contains multiple rows for the same group and pivot literal:
      they aggregate together.
- [ ] Outer `ORDER BY` references generated pivot columns.
- [ ] Outer join uses the pivoted result via alias.
- [ ] Duplicate pivot output names are rejected deterministically.

## On-disk format

No on-disk format changes.

Compatibility rule: `21.25` must stay parser/analyzer/rewrite/executor-surface
work only.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| Bounded pivot over 100K rows / 4 pivot values | within 10% of equivalent handwritten `GROUP BY + CASE` query | within 20% of equivalent handwritten `GROUP BY + CASE` query |

Reference: this subphase should reuse existing grouped aggregation and not
introduce a materially worse execution path than the manual rewrite users
could write today.

## Dependencies

- Depends on existing grouped aggregation execution.
- Depends on analyzer support for derived tables / virtual columns.
- Depends on current expression evaluation for `CASE WHEN`.
- Blocks the remaining user-visible reshaping gap tracked as `21.25`.

## Open questions

- [x] `21.25` should be implemented as explicit-value pivoting, not true
      runtime-discovered columns.
- [x] The MVP should be single-aggregate only.
- [x] `UNPIVOT` stays out of scope.

## Done criteria

- [x] `PIVOT (... FOR ... IN (...))` parses in the supported bounded syntax.
- [x] The analyzer validates the pivot shape and derives a stable output schema.
- [x] Execution uses a rewrite to existing grouped aggregation semantics.
- [x] SQL integration tests cover parser acceptance, output shape, grouping
      behavior, aliasing, and key error cases.
- [x] `python3 tools/wire-test.py` includes a smoke if the feature is exposed
      through the MySQL wire path.
- [x] `cargo test -p axiomdb-sql` passes for touched pivot coverage.
- [x] `cargo test --workspace` passes.
- [x] `cargo clippy --workspace -- -D warnings` passes.

## References

- `docs/progreso.md` — `21.25 PIVOT dynamic`
- `memory/project_state.md`
- `crates/axiomdb-sql/src/ast.rs`
- `crates/axiomdb-sql/src/parser/dml.rs`
- `crates/axiomdb-sql/src/analyzer_bind.rs`
- `research/datafusion/datafusion-examples/examples/relation_planner/pivot_unpivot.rs`
