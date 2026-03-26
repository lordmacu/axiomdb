# Spec: 4.8b — FULL OUTER JOIN

These files were reviewed before writing this spec:
- `db.md`
- `docs/progreso.md`
- `specs/fase-04/spec-4.8.md`
- `specs/fase-04/plan-4.8.md`
- `crates/axiomdb-sql/src/ast.rs`
- `crates/axiomdb-sql/src/parser/dml.rs`
- `crates/axiomdb-sql/src/analyzer.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `crates/axiomdb-network/tests/integration_protocol.rs`
- `tools/wire-test.py`
- `docs-site/src/user-guide/sql-reference/dml.md`
- `docs-site/src/internals/architecture.md`

## Research synthesis

### AxiomDB first

The design is constrained by the current AxiomDB join executor in
`crates/axiomdb-sql/src/executor.rs`:
- joins already execute as progressive nested loops over pre-scanned tables
- `JoinType::Full` already exists in AST/parser
- `JoinCondition::Using` is still resolved in the executor, not in the analyzer
- `WHERE`, `GROUP BY`, `ORDER BY`, `DISTINCT`, and `LIMIT/OFFSET` already run
  after the join stage

### What we borrow

- `research/postgres/src/include/nodes/nodes.h`
  - semantic contract: FULL join = matched pairs + unmatched left rows +
    unmatched right rows
- `research/sqlite/test/join7.test`
  - behavioral oracle for `ON` vs `WHERE` under FULL JOIN and for null-extended
    unmatched rows
- `research/datafusion/datafusion/physical-plan/src/joins/nested_loop_join.rs`
  - matched-bitmap / outer-marker technique for preserving unmatched build/probe
    rows in an outer join implementation
- `research/datafusion/docs/source/user-guide/sql/select.md`
  - concise user-facing statement that FULL JOIN is effectively the union of
    left and right outer semantics
- `research/duckdb/src/planner/binder/tableref/bind_joinref.cpp`
  - reminder that `USING` binding/duplicate-column policy is a separate concern
    from the physical join algorithm

### What we reject

- Desugaring FULL JOIN into `LEFT JOIN UNION RIGHT JOIN`
  - rejected because it scans both sides twice, complicates duplicate handling,
    and changes the `ON`/`WHERE` interaction unless special-cased heavily
- Introducing a new hash/merge join subsystem in Phase 4
  - rejected because AxiomDB's current join engine is nested-loop based and
    this subphase is only closing the last missing join type
- Using MariaDB/MySQL as the external behavior oracle
  - rejected because MySQL does not define `FULL OUTER JOIN` as part of its SQL
    surface, so it is not the right compatibility source for this feature

### How AxiomDB adapts it

AxiomDB will keep the existing nested-loop executor and extend it with one new
case: `JoinType::Full` in `apply_join(...)`. The implementation must emit:
- matched rows during the normal left-to-right nested loop
- unmatched left rows padded with right-side NULLs
- unmatched right rows padded with left-side NULLs using a matched-right bitmap

`JoinCondition::Using` remains executor-side in 4.8b. This subphase does not
move `USING` binding into the analyzer and does not introduce SQL-standard
duplicate-column suppression for `USING`.

## What to build (not how)

Add execution support for `FULL JOIN` / `FULL OUTER JOIN` in the existing Phase
4 nested-loop join pipeline.

`FULL OUTER JOIN` is a deliberate AxiomDB SQL extension over the MySQL wire
protocol surface. The parser already accepts it; this subphase removes the
remaining executor-side `NotImplemented` gap.

The feature must work with the current join architecture:
- base-table `FROM` + joined base tables
- `JoinCondition::On(expr)` and the current executor semantics of `USING(...)`
- multi-join chains that progressively accumulate the left side
- the existing post-join pipeline for `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`,
  `DISTINCT`, and `LIMIT/OFFSET`

This subphase must also correct output metadata nullability for FULL joins:
tables that can be null-extended by a FULL join must surface as nullable in
`ColumnMeta` for `SELECT *`, qualified wildcards, and direct column projections.

## Inputs / Outputs

- Input:
  - analyzed `SelectStmt` with one or more `JoinClause`
  - at least one join with `join_type = JoinType::Full`
  - join operands limited to the currently supported join executor surface
- Output:
  - `QueryResult::Rows` with matched rows plus unmatched rows from both sides
    padded with `Value::Null`
  - `ColumnMeta.nullable = true` for columns belonging to any table that can be
    null-extended by the join chain
- Errors:
  - unsupported join operand kinds keep their current behavior
  - invalid `USING` column names keep returning `DbError::ColumnNotFound`
  - `FULL OUTER JOIN` must no longer return
    `DbError::NotImplemented { feature: "FULL OUTER JOIN — Phase 4.8+" }`

## Use cases

1. Basic FULL join with orphan rows on both sides.
   `users FULL OUTER JOIN orders ON users.id = orders.user_id` returns matched
   rows, users without orders, and orders with no valid user.

2. FULL join with multiple matches.
   If one left row matches two right rows, both joined rows are emitted. FULL
   join must not deduplicate or collapse duplicates.

3. `ON` vs `WHERE` distinction.
   `ON` filters determine which rows match before null-extension; `WHERE`
   filters run afterward and may remove null-extended rows.

4. `SELECT *` metadata.
   When a table can be null-extended by FULL join, its columns become nullable
   in the output metadata even if the catalog marks them `NOT NULL`.

5. Join chain with a later stage.
   `a FULL JOIN b ON ... LEFT JOIN c ON ...` continues to use the progressive
   nested-loop pipeline and retains correct global `col_idx` semantics.

## Acceptance criteria

- [ ] `FULL JOIN` and `FULL OUTER JOIN` execute successfully through the existing SELECT join path
- [ ] Matched rows are emitted exactly as in INNER join, plus unmatched left rows with NULL right side, plus unmatched right rows with NULL left side
- [ ] FULL join does not deduplicate repeated matches; multiple valid pairs produce multiple output rows
- [ ] `ON` predicates are applied before null-extension, and `WHERE` predicates are applied after FULL join materialization
- [ ] `JoinCondition::Using(...)` continues to work for two-table FULL joins using the existing executor-side resolution model
- [ ] `SELECT *` and qualified wildcard metadata mark both participating sides of a FULL join nullable
- [ ] Expression metadata for direct column projections reflects the same FULL-join nullability propagation
- [ ] Multi-join chains that include a FULL join still use the progressive combined-row layout expected by the analyzer
- [ ] The executor no longer contains an active `NotImplemented` guard for `JoinType::Full` in the FULL join path
- [ ] `tools/wire-test.py` includes a live FULL JOIN assertion through the MySQL wire protocol

## Out of scope

- Replacing nested-loop joins with hash join or merge join
- Rewriting FULL JOIN as `LEFT JOIN UNION RIGHT JOIN`
- SQL-standard duplicate-column suppression for `USING`
- `NATURAL FULL JOIN`
- Expanding support for join operands beyond the current join executor surface
- MySQL compatibility-mode gating or disabling of the AxiomDB FULL JOIN extension

## Dependencies

- `specs/fase-04/spec-4.8.md` — existing join contract and prior explicit exclusion of FULL
- `specs/fase-04/plan-4.8.md` — current join pipeline shape
- `crates/axiomdb-sql/src/ast.rs` — `JoinType::Full`
- `crates/axiomdb-sql/src/parser/dml.rs` — `FULL [OUTER] JOIN` parsing already exists
- `crates/axiomdb-sql/src/analyzer.rs` — combined-row `col_idx` contract for joins
- `crates/axiomdb-sql/src/executor.rs` — current nested-loop join implementation and metadata inference
- `crates/axiomdb-sql/tests/integration_executor.rs` — join semantics tests
- `tools/wire-test.py` — live wire smoke/regression test path
- `docs-site/src/user-guide/sql-reference/dml.md` — user-facing SQL docs
- `docs-site/src/internals/architecture.md` — technical executor docs

## ⚠️ DEFERRED

- SQL-standard output-column coalescing for `USING`
  - current AxiomDB wildcard expansion still emits both table sides
  - 4.8b keeps that behavior unchanged

- Join-subquery surface expansion beyond the currently supported join executor path
  - 4.8b only closes the `JoinType::Full` gap in the existing architecture
