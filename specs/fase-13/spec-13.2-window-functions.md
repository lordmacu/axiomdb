# Spec: 13.2 — Window functions

Phase: 13 — Advanced PostgreSQL
Task: 13.2 window functions
Status: closed 2026-04-23

## Context

Phase `13.2` is the first real SQL window-function slice in AxiomDB. Today the
engine has no `OVER (...)` grammar, no AST for window clauses, and no executor
stage that can evaluate row-position-aware expressions after filtering and
ordering. The existing aggregate pipeline handles grouped reduction, but not
per-row window projection.

The roadmap line in `docs/progreso.md` names `RANK`, `ROW_NUMBER`, `LAG`,
`LEAD`, and `SUM OVER`, but the current codebase has zero partial support for
window syntax. The first cut therefore has to be intentionally bounded so it
fits the existing `SELECT` executor without introducing a full generic window
engine and frame planner in one subphase.

## Goal

Deliver a bounded, effectful MVP of SQL window functions by adding
`ROW_NUMBER()`, `RANK()`, and `DENSE_RANK()` with `OVER ( [PARTITION BY ...]
ORDER BY ... )` in top-level `SELECT` projection.

## Non-goals

- Not handling `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `NTILE`, or
  aggregate windows like `SUM(...) OVER (...)` in this subphase.
- Not handling explicit frame clauses (`ROWS BETWEEN ...`, `RANGE`, `GROUPS`).
- Not handling named windows via `WINDOW w AS (...)`.
- Not handling window functions in `WHERE`, `GROUP BY`, `HAVING`, `JOIN ON`,
  `CHECK`, index expressions, generated columns, or DML targets.
- Not optimizing with a dedicated physical window operator or planner cost
  model.

## Behavior

### Public SQL surface

Supported forms:

```sql
SELECT
  dept,
  employee,
  ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC, id) AS rn
FROM payroll;

SELECT
  dept,
  employee,
  RANK() OVER (ORDER BY salary DESC) AS rk,
  DENSE_RANK() OVER (ORDER BY salary DESC) AS dr
FROM payroll;
```

### Semantics

- A window function is allowed only in top-level `SELECT` projection items.
- Supported functions are `ROW_NUMBER`, `RANK`, and `DENSE_RANK`
  case-insensitively.
- Each window call must use an `OVER (...)` clause.
- The `OVER (...)` clause may contain:
  - optional `PARTITION BY expr [, ...]`
  - mandatory `ORDER BY expr [ASC|DESC] [NULLS FIRST|LAST] [, ...]`
- Window expressions are evaluated after `FROM`, `WHERE`, and base-row
  materialization, but before final projection aliases are returned.
- Partitioning splits the filtered row stream into independent logical groups.
- Ordering defines the row sequence inside each partition.
- Tie handling:
  - `ROW_NUMBER`: increments by 1 per row, regardless of ties.
  - `RANK`: peers share the same rank; the next rank skips by peer count.
  - `DENSE_RANK`: peers share the same rank; the next rank increments by 1.
- If no `PARTITION BY` is present, the whole filtered row set is one partition.
- The `ORDER BY` inside `OVER (...)` is independent from the outer query's
  final `ORDER BY`.
- If the outer query also has `ORDER BY`, that ordering still controls final
  result presentation; window ordering affects only the computed window values.

### Legality rules

- Window functions are rejected outside the `SELECT` list.
- Nested window functions are rejected.
- Aggregate functions inside the window `ORDER BY` / `PARTITION BY` expressions
  are rejected.
- Window functions mixed with query-level `GROUP BY` / grouped aggregates are
  rejected in this MVP.
- `OVER ()` without `ORDER BY` is rejected for all three supported functions.

### Error cases

| Input | Expected error | Message shape |
|-------|----------------|---------------|
| `ROW_NUMBER()` without `OVER` | `DbError::ParseError` | mentions `OVER` |
| `ROW_NUMBER() OVER ()` | `DbError::ParseError` | mentions `ORDER BY` |
| `LAG(x) OVER (...)` | `DbError::NotImplemented` | mentions window function |
| window function in `WHERE` | `DbError::InvalidValue` | mentions window function location |
| window + grouped aggregate in same SELECT | `DbError::NotImplemented` | mentions grouped/window mix |

## Edge cases

- [x] Empty result set returns zero rows without errors.
- [x] Single-row partition produces `1` for all supported functions.
- [x] Multiple partitions restart numbering/ranking independently.
- [x] Peer ties with composite `ORDER BY` behave deterministically.
- [x] `NULL` sort positioning inside window `ORDER BY` follows existing sort
      semantics.
- [x] Outer `ORDER BY` different from window `ORDER BY` preserves correct
      window values.
- [x] Multiple window columns in the same `SELECT` can reuse the same spec or
      coexist with different partition/order specs.

## Implementation boundary

- Parser/AST must represent `OVER (...)` explicitly rather than encoding it as
  a normal function call.
- Executor may implement window evaluation as a post-scan in-memory decoration
  phase over materialized rows; a dedicated planner node is not required.
- Metadata/result columns for supported window outputs are integer-like and may
  surface as `BIGINT`.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| `ROW_NUMBER/RANK/DENSE_RANK` over 10k rows, one partition | 80 ms | 120 ms |
| same over 100 partitions / 10k total rows | 100 ms | 150 ms |

Reference: bounded MVP on top of the existing materialized-row `SELECT`
executor; no special vectorized window operator is required in this phase.

## Dependencies

- Depends on: the existing `SELECT` executor, sort semantics, and projection
  pipeline in `axiomdb-sql`.
- Blocks: honest Phase 13 window-function parity follow-ups such as
  `LAG/LEAD`, aggregate windows, and explicit frames.

## Open questions

- [ ] Should grouped aggregate + window coexistence be rejected immediately in
      `13.2`, or deferred to a later mixed-query follow-up?
- [ ] Should unsupported but parsed functions like `LAG/LEAD` parse now and
      raise `NotImplemented`, or stay fully unsupported at parse time?

## Done criteria

- [x] `OVER (...)` syntax parses into explicit AST nodes.
- [x] `ROW_NUMBER`, `RANK`, and `DENSE_RANK` execute correctly with
      `PARTITION BY` + `ORDER BY`.
- [x] Illegal placements and unsupported window constructs fail with explicit
      errors.
- [x] Dedicated integration coverage exists for basic, partitioned, tied, and
      outer-ordering cases.
- [x] `python3 tools/wire-test.py` includes a bounded `13.2` smoke.
- [x] `cargo test -p axiomdb-sql --test integration_window_functions` passes.
- [x] `cargo test --workspace` passes.
- [x] `cargo clippy --workspace -- -D warnings` passes.

## References

- `docs/progreso.md`
- `docs/fase-13.md`
- `db.md`
- PostgreSQL documentation: window functions / `OVER`
