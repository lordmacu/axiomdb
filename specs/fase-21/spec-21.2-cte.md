# Spec: 21.2 — Common Table Expressions (`WITH` queries)

## What to build

Non-recursive CTEs: named subqueries bound ahead of the main statement
that can be referenced as tables in the outer query's FROM / JOINs.

```sql
WITH active_users AS (
  SELECT id, email FROM users WHERE status = 'active'
)
SELECT * FROM active_users WHERE email LIKE '%@example.com';

WITH
  a AS (SELECT 1 AS x),
  b AS (SELECT 2 AS y)
SELECT a.x, b.y FROM a, b;

-- Chained reference: later CTE reads earlier.
WITH
  base AS (SELECT * FROM orders WHERE status = 'paid'),
  tot  AS (SELECT customer_id, SUM(total) AS sum FROM base GROUP BY customer_id)
SELECT * FROM tot WHERE sum > 1000;
```

Recursive CTEs (`WITH RECURSIVE`) are 21.3 — separate subphase.

## Inputs / outputs

### Grammar

```
select_stmt := [ with_clause ] SELECT ...
with_clause := 'WITH' cte_binding (',' cte_binding)*

cte_binding := identifier [ '(' col_name (',' col_name)* ')' ]
               'AS' '(' SELECT ')'
```

### AST

Add to `SelectStmt`:

```rust
pub with_ctes: Vec<CteBinding>,

pub struct CteBinding {
    pub name: String,
    pub column_names: Option<Vec<String>>,
    pub query: Box<SelectStmt>,
}
```

### Semantics

Scope:
- Each CTE is visible in later CTEs in the same WITH list and in the
  main query.
- A CTE cannot reference itself (that's `WITH RECURSIVE`, 21.3 —
  parser-level error if detected).
- Column-name override: `WITH t(a, b) AS (SELECT ...)` renames the
  columns positionally; must match select-list length.

Execution:
- Desugar at analyzer time: for each `FromClause::Table(tref)` inside
  the outer SELECT (or any join-side), if `tref.name` matches a CTE
  name, rewrite to `FromClause::Subquery { query, alias }`. The
  `alias` carries any user-specified alias; otherwise defaults to the
  CTE name.
- The substituted query is the analyzed CTE body (pre-analyzed in
  dependency order during WITH-list resolution).
- Re-execution model: each reference to the CTE causes re-execution
  of the body (PG default for non-recursive non-MATERIALIZED). Future
  optimization can materialize once.

## Use cases

- Staged filtering / aggregation pipelines.
- Query readability (replace deeply nested subqueries).
- Same subquery referenced multiple times via different aliases.
- Prerequisite for recursive CTEs in 21.3.

## Acceptance criteria

- [ ] `WITH x AS (SELECT ...) SELECT * FROM x` parses and returns the
      CTE's rows.
- [ ] Multiple CTEs in one `WITH`: `WITH a AS (...), b AS (...) ...`.
- [ ] Later CTE references earlier CTE.
- [ ] Column-name override: `WITH t(a, b) AS (SELECT x, y FROM src)`.
- [ ] CTE referenced in JOIN: `FROM t JOIN x ON ...`.
- [ ] CTE referenced multiple times in same outer query.
- [ ] Parser rejects direct self-reference (reserve for 21.3 with
      clear error: "use WITH RECURSIVE for self-referencing CTEs").
- [ ] Existing queries without WITH: unchanged behavior.
- [ ] `WITH` + GROUP BY / ORDER BY / LIMIT in outer query works.
- [ ] Integration tests in `tests/integration_cte.rs` (10+ tests).
- [ ] 1 wire smoke assertion.

## Out of scope

- `WITH RECURSIVE` — 21.3.
- `WITH ... MATERIALIZED` / `NOT MATERIALIZED` hints (PG 12+) —
  optimizer hint; ignored for now (default behavior OK).
- Data-modifying CTEs (`WITH d AS (DELETE FROM ... RETURNING ...)
  INSERT INTO ...`) — requires RETURNING fully plus CTE pipeline;
  deferred until 21.4b closes.
- CTE in DML statement (`WITH ... UPDATE ...`). Rare; defer.

## Cross-engine

- **PostgreSQL** — `WithClause` with `CommonTableExpr` list, all CTEs
  materialized by default in PG ≤ 11, inlined since 12. AxiomDB
  follows PG 12+ re-execution semantics for this subphase.
- **MySQL 8.0+** — non-recursive and recursive both supported.
- **SQLite 3.8.3+** — supports.
- **DuckDB** — supports.

## Dependencies

- Existing `FromClause::Subquery { query, alias }` variant.
- Analyzer's `analyze_select_with_outer` handles Subquery-in-FROM
  correctly.
- No executor changes beyond CTE substitution at parse/analysis.
