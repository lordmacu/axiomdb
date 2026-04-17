# Spec: 21.12 DISTINCT ON

Phase: 21 — Advanced SQL
Task: DISTINCT ON — first row per group
Status: approved

## Context

AxiomDB already implements `SELECT DISTINCT` (full-row deduplication via `apply_distinct_with_session`).
DISTINCT ON is a PostgreSQL extension that deduplicates on a subset of key expressions, keeping one
representative row per group. It is heavily used in "latest-per-group" queries and ORM patterns.

The executor currently has four SELECT code paths (no-FROM, derived/VALUES/JSON-TABLE, GROUP-BY,
and JOIN), each with independent ORDER BY + DISTINCT handling.

## Goal

Parse and execute `SELECT DISTINCT ON (expr_list) …` so that the first row per distinct combination
of the DISTINCT ON key expressions is returned, ordered as specified by ORDER BY.

## Non-goals

- Standard SQL `DISTINCT` behavior is unchanged.
- DISTINCT ON inside set operations (UNION/INTERSECT/EXCEPT) — deferred.
- Enforcement that ORDER BY begins with DISTINCT ON expressions (PG enforces at plan time; we emit
  a warning-level note but do not hard-error in phase 21.12).

## Behavior

### Syntax

```sql
SELECT DISTINCT ON (expr [, expr ...]) select_item [, ...]
FROM ...
[WHERE ...]
[ORDER BY distinct_on_expr [ASC|DESC] [NULLS {FIRST|LAST}], ...]
[LIMIT n] [OFFSET m];
```

### Semantics

1. All source rows are collected (post-WHERE, post-GROUP-BY if any).
2. Rows are sorted by the combined key: **DISTINCT ON expressions first (all ASC NULLS LAST by
   default), then the full ORDER BY clause**.
3. From the sorted sequence, only the **first row per unique DISTINCT ON key** is retained.
4. The retained rows are already in the correct final ORDER BY sequence — no second sort needed.
5. DISTINCT ON and plain DISTINCT are mutually exclusive. If both appear the parser returns an error.
6. DISTINCT ON expressions are evaluated against **pre-projection rows** (same scope as ORDER BY),
   allowing references to source columns not in the SELECT list.

### Public API (AST)

```rust
pub struct SelectStmt {
    // ... existing fields ...
    pub distinct: bool,
    /// Phase 21.12 — DISTINCT ON key expressions. Non-empty ⟹ distinct == false.
    /// Evaluated against pre-projection (source) rows, like ORDER BY.
    pub distinct_on: Vec<Expr>,
    // ...
}
```

### Error cases

| Input | Expected error | Message |
|---|---|---|
| `SELECT DISTINCT ON ()` | `DbError::ParseError` | "DISTINCT ON requires at least one expression" |
| `SELECT DISTINCT ON (a) DISTINCT …` (nonsense) | `DbError::ParseError` | "cannot combine DISTINCT and DISTINCT ON" |
| DISTINCT ON inside a set-op branch | `DbError::NotImplemented` | "DISTINCT ON inside UNION/INTERSECT/EXCEPT not yet supported" |

### ORDER BY interaction

PostgreSQL requires ORDER BY to begin with DISTINCT ON expressions in the same sort direction.
AxiomDB 21.12: accept any ORDER BY, but within each DISTINCT ON group the "first" row is the one
that sorts first by the combined (DISTINCT ON ASC, ORDER BY) key — identical results as if PG's
constraint were satisfied.

## Edge cases

- [ ] DISTINCT ON with no ORDER BY — order within group is unspecified (first row from scan order)
- [ ] NULL values in DISTINCT ON key — two NULL keys are treated as equal (PG behavior)
- [ ] DISTINCT ON expr not in SELECT list — must work (evaluated on pre-projection rows)
- [ ] DISTINCT ON with LIMIT — LIMIT applied after deduplication
- [ ] DISTINCT ON with GROUP BY — valid (unusual; deduplicates aggregate rows by key)
- [ ] DISTINCT ON with a single expression that matches a column alias in SELECT — resolved via source col, not alias
- [ ] DISTINCT ON on a subquery — works (same as table FROM)
- [ ] `SELECT DISTINCT ON (1)` — positional reference resolves to first SELECT item

## Performance budget

No special budget. DISTINCT ON adds one sort pass (same as ORDER BY) over the result set.
Deduplication is O(n) after sort using a serialized key HashSet.

## Dependencies

- Depends on: existing `SelectStmt`, ORDER BY infrastructure (`apply_order_by`, `value_to_session_key_bytes`)
- Blocks: nothing

## Done criteria

- [ ] `SelectStmt.distinct_on: Vec<Expr>` added; all existing match/struct-literal sites compile
- [ ] Parser: `SELECT DISTINCT ON (e1, e2) …` sets `distinct_on`, `distinct = false`
- [ ] Parser: `SELECT DISTINCT ON ()` → parse error
- [ ] Analyzer: `distinct_on` expressions resolved in all four SELECT paths
- [ ] All walker/visitor sites have `distinct_on` arm (plan_deps, exec_subquery, etc.)
- [ ] Executor helper `apply_distinct_on` implemented and wired into all 4 SELECT paths
- [ ] 10+ integration tests in `tests/integration_distinct_on.rs`
- [ ] `cargo test -p axiomdb-sql` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] Wire smoke test assertions added for DISTINCT ON
- [ ] `docs-site/src/user-guide/sql-reference/dml.md` updated
- [ ] `docs-site/src/internals/sql-parser.md` updated
- [ ] `docs/progreso.md` updated: `[x] ✅ 21.12`

## References

- PostgreSQL `parsenodes.h:226,1541-1548` — `distinctClause` field and semantics doc
- DuckDB `bind_select_node.cpp:137-146` — `DistinctType::DISTINCT_ON` binding
- AxiomDB `executor/agg_group_table.rs:103` — `apply_distinct_with_session`
- AxiomDB `executor/select_core.rs:434-456` — ORDER BY + DISTINCT pipeline
