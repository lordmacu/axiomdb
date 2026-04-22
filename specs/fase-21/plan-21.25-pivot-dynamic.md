# Plan: 21.25 — PIVOT dynamic

Phase: 21 — Advanced SQL
Task: 21.25 PIVOT dynamic
Spec: specs/fase-21/spec-21.25-pivot-dynamic.md
Status: done

## Summary

Implement `21.25` as a bounded pivot rewrite, not as a new execution engine.
First add parser/AST support for a pivoted `FROM` item with explicit `IN (...)`
literals. Then add analyzer validation plus schema derivation so the pivoted
source publishes a stable derived-table shape. Finally lower the pivot into a
regular grouped SELECT using `CASE WHEN` and validate it end-to-end with SQL
and wire coverage. This order keeps each step local and prevents the subphase
from drifting into runtime-dynamic columns or `UNPIVOT`.

## Dependencies

Must be done first:
- [x] `specs/fase-21/spec-21.25-pivot-dynamic.md` accepted.
- [ ] Current grouped aggregation and derived-table binding behavior reviewed.

Blocks:
- [x] Closure of the remaining user-visible `21.25` reshaping gap.

## Affected files

Likely modified files:
- `crates/axiomdb-sql/src/ast.rs` — pivot AST structures / `FromClause` support.
- `crates/axiomdb-sql/src/parser/dml.rs` — `FROM ... PIVOT (...)` parsing.
- `crates/axiomdb-sql/src/analyzer_bind.rs` — source-schema publication for pivot.
- `crates/axiomdb-sql/src/analyzer_stmt.rs` and/or a new pivot helper module —
  validation + rewrite to derived grouped SELECT.
- `crates/axiomdb-sql/src/executor/*` — only if dispatch needs a small hook for
  rewritten derived SELECTs.
- `crates/axiomdb-sql/tests/` — new focused integration coverage.
- `tools/wire-test.py` — wire-visible smoke.
- `docs/progreso.md`, `docs/fase-21.md`, `memory/project_state.md`,
  `memory/architecture.md`, `memory/lessons.md` — closeout only after impl.

Likely new files:
- `specs/fase-21/spec-21.25-pivot-dynamic.md`
- `specs/fase-21/plan-21.25-pivot-dynamic.md`
- `crates/axiomdb-sql/tests/integration_pivot.rs`
- optionally `crates/axiomdb-sql/src/pivot.rs` for rewrite helpers

## Step 1 — Parser and AST shape

**Goal:** accept bounded `FROM source PIVOT (...) [AS alias]` syntax and store
enough information to validate/rewrite later.

**Files:** `ast.rs`, `parser/dml.rs`

**Approach:** add a pivot wrapper around an existing `FromClause` item instead
of inventing a standalone top-level statement.

### Parser cut

Support only:

```sql
FROM source
PIVOT (
  agg_func(value_expr)
  FOR pivot_expr
  IN ('literal1', 'literal2', ...)
) [AS alias]
```

Reject in this step:

- multiple aggregate expressions
- non-literal `IN` values
- `UNPIVOT`

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser
```

Add or extend parser tests for:

- basic parse acceptance
- alias form
- parse rejection for malformed pivot clause

## Step 2 — Analyzer validation and stable output schema

**Goal:** make pivoted `FROM` items bind like derived tables with a deterministic
column list before execution.

**Files:** `analyzer_bind.rs`, `analyzer_stmt.rs`, optional new `pivot.rs`

**Approach:** analyze the wrapped source first, derive passthrough grouping
columns plus generated pivot columns, and reject any ambiguous/unsupported
shape early.

### Validation rules

- aggregate must be single-argument and supported
- pivot `IN` values must be literals
- generated output names must be unique
- generated output names must not collide with passthrough source columns

### Schema derivation

- passthrough columns = source columns not referenced by `pivot_expr` or
  aggregate input expression
- generated columns = one per pivot literal in `IN (...)` order

### Verification

```bash
cargo test -p axiomdb-sql --test integration_pivot
```

Add tests for:

- output column ordering
- duplicate-name rejection
- no-passthrough single-row aggregate shape

## Step 3 — Rewrite to grouped SELECT

**Goal:** lower the validated pivot into a regular derived SELECT using
`GROUP BY + aggregate(CASE WHEN ...)`.

**Files:** analyzer/rewrite helper module, any small executor hookup if needed

**Approach:** keep execution on existing code paths. The pivot node should not
survive into a bespoke runtime executor.

### Rewrite shape

For each pivot literal:

```sql
agg_func(CASE WHEN pivot_expr = <literal> THEN value_expr ELSE NULL END) AS <pivot_col>
```

Group by all passthrough columns. Wrap the rewritten query as a derived table
so outer SELECT / JOIN / ORDER BY continue to work normally.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_pivot
```

Add tests for:

- basic monthly sales pivot
- multiple grouping columns
- outer projection / `ORDER BY` on generated columns
- join against a pivoted derived table

## Step 4 — Wire smoke and closeout

**Goal:** prove the feature is visible through the MySQL wire protocol and
close the subphase cleanly.

**Files:** `tools/wire-test.py`, phase docs / memory files

**Approach:** keep the wire smoke narrow and deterministic.

### Smoke candidate

```sql
CREATE TABLE sales(region TEXT, month TEXT, amount INT);
INSERT INTO sales VALUES
  ('north', 'Jan', 10),
  ('north', 'Feb', 20),
  ('south', 'Jan', 15);

SELECT *
FROM sales
PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb')) AS p
ORDER BY region;
```

Expected result shape:

```text
region | Jan | Feb
north  | 10  | 20
south  | 15  | NULL
```

### Final verification

```bash
cargo fmt --check
cargo test -p axiomdb-sql --test integration_pivot
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Risk register

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Scope expands into true runtime-dynamic columns | high | keep explicit `IN (...)` list as the contract and document that cut everywhere |
| Pivot schema derivation disagrees with binder assumptions | medium | derive output columns during analysis, before execution, and reuse derived-table patterns already in the codebase |
| Rewritten expressions break aggregate semantics | medium | keep rewrite minimal and cover with direct equivalence tests against handwritten `GROUP BY + CASE` |
| Name generation becomes ambiguous | medium | reject duplicate or colliding output names instead of inventing silent renames |
| Parser ambiguity around `FROM source PIVOT` | low-medium | add the wrapper after `parse_from_item` and before join parsing, with parser tests for alias and join forms |

## Estimated effort

Total: high

- Step 1: 45-90 min
- Step 2: 60-120 min
- Step 3: 90-180 min
- Step 4: 30-60 min
