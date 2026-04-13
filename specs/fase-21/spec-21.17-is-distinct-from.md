# Spec: 21.17 — `IS [NOT] DISTINCT FROM`

## What to build

Standard SQL NULL-safe comparison operators:

```sql
a IS DISTINCT FROM b       -- TRUE if a ≠ b accounting for NULL
a IS NOT DISTINCT FROM b   -- TRUE if a = b accounting for NULL
```

Always returns `BOOLEAN`, never `NULL`. Used heavily by ORMs (Prisma,
TypeORM, ActiveRecord) for NULL-safe equality in `WHERE` clauses and
join conditions without having to write `(a = b OR (a IS NULL AND b IS NULL))`.

## Inputs / outputs

### Grammar

```
predicate := expr 'IS' [ 'NOT' ] 'DISTINCT' 'FROM' expr
```

Both `IS DISTINCT FROM` and `IS NOT DISTINCT FROM` accepted. Precedence
matches other `IS` predicates (`IS NULL`, `IS TRUE`, `IS FALSE`).

### Semantics (truth table)

| a | b | `a IS DISTINCT FROM b` | `a IS NOT DISTINCT FROM b` |
|---|---|---|---|
| 1 | 1 | FALSE | TRUE |
| 1 | 2 | TRUE | FALSE |
| 1 | NULL | TRUE | FALSE |
| NULL | 1 | TRUE | FALSE |
| NULL | NULL | FALSE | TRUE |

## Use cases

```sql
-- NULL-safe join.
SELECT * FROM a JOIN b ON a.x IS NOT DISTINCT FROM b.x;

-- Change detection without triple-checks.
UPDATE t SET touched = TRUE
 WHERE new_value IS DISTINCT FROM old_value;

-- Prisma-generated queries depend on this operator.
```

## Acceptance criteria

- [ ] `a IS DISTINCT FROM b` parses.
- [ ] `a IS NOT DISTINCT FROM b` parses.
- [ ] Returns `TRUE` when exactly one side is NULL.
- [ ] Returns `FALSE` when both sides are NULL.
- [ ] Returns `NOT (a = b)` when both sides are non-NULL (distinct
      form), `(a = b)` for not-distinct.
- [ ] Usable in `WHERE`, `ON`, `HAVING`, `CASE WHEN`, `SELECT`
      projection.
- [ ] Integration tests in `tests/integration_is_distinct_from.rs`.
- [ ] 1 wire smoke assertion.
- [ ] Workspace tests / clippy / fmt clean.

## Cross-engine

- **PostgreSQL** `gram.y:16129` — `AEXPR_DISTINCT` / `AEXPR_NOT_DISTINCT`
  dedicated node types. Custom evaluator handles NULL-vs-NULL.
- **MySQL 8** — has `<=>` NULL-safe-equal operator (equivalent to
  `IS NOT DISTINCT FROM`) but historically lacked the standard SQL
  spelling. MySQL 8.0.34+ accepts `IS [NOT] DISTINCT FROM` as a
  spelling alias for `<=>` and `NOT (<=>)`.
- **SQLite** — supports via the same grammar shape as PG (since
  3.39).
- **DuckDB** — supports `IS DISTINCT FROM` and `IS NOT DISTINCT FROM`
  natively.

## Design decision

**Desugar at parse time** to the existing `BinaryOp::NullSafe`
infrastructure (`<=>`):

- `a IS NOT DISTINCT FROM b` → `a <=> b`
- `a IS DISTINCT FROM b`     → `NOT (a <=> b)`

Rationale: AxiomDB's `BinaryOp::NullSafe` already implements the
NULL-vs-NULL → TRUE / NULL-vs-value → FALSE semantics required by
"not-distinct-from". Wrapping with a `UnaryOp::Not` gets
"distinct-from" for free. No new AST variant, no new evaluator, no
new planner rule.

Trade-off: AST round-trip printing reconstructs the desugared form
(`NOT (a <=> b)` or `a <=> b`) rather than the original
`IS [NOT] DISTINCT FROM` surface. Acceptable — no behavioral impact.

## Out of scope

- Row-valued operands: `(a, b) IS DISTINCT FROM (c, d)` — PG
  extension over record types; AxiomDB does not have first-class row
  values yet. Deferred until record types land.

## Dependencies

- `BinaryOp::NullSafe` (already present).
- `UnaryOp::Not` (already present).
- `parse_is_null` parse point (extends the existing `IS` dispatch).
