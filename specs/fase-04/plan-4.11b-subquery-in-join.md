# Plan: 4.11b — Subquery in JOIN

## Files to create/modify

- `crates/axiomdb-sql/src/analyzer_stmt.rs` — persist analyzed join-side subqueries back into the analyzed AST, not just into temporary bind context
- `crates/axiomdb-sql/src/executor/select_helpers.rs` — implement join-side derived-table materialization in the legacy non-ctx JOIN path
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` — implement join-side derived-table materialization in the session-aware JOIN path
- `crates/axiomdb-sql/src/executor/joins.rs` — generalize JOIN metadata helpers so `USING`, wildcard expansion, and expression type inference work for both catalog tables and derived join sources
- `crates/axiomdb-sql/tests/integration_subqueries.rs` — add executor coverage for join-side derived tables
- `crates/axiomdb-sql/tests/integration_analyzer.rs` — add analyzer coverage for alias/column resolution through `JOIN (SELECT ...) AS alias`

## Algorithm / Data structure

Introduce an internal executor-only abstraction for a JOIN input source:

```rust
struct JoinSourceData {
    source_name: String,
    columns: Vec<ColumnMeta>,
    rows: Vec<Row>,
}
```

Population rules:

1. Base table JOIN source:
   - Resolve through catalog as today
   - Scan rows under the active snapshot
   - Convert catalog columns to `ColumnMeta`

2. Derived JOIN source:
   - Take the already-analyzed `SelectStmt` from `FromClause::Subquery`
   - Execute it once under the same snapshot/session path as the outer query
   - Require `QueryResult::Rows`
   - Keep returned `ColumnMeta` + materialized rows as the join input

Executor flow:

```text
resolve/materialize FROM source
for each JOIN:
  resolve/materialize right source (table or derived)
  apply_join(left_rows, right_rows, join_type, condition, schemas)
build output metadata from combined source descriptors
project rows
apply DISTINCT / LIMIT / OFFSET
```

Analyzer flow:

```text
for each join in SelectStmt.joins:
  if join.table is FromClause::Subquery:
    analyze inner select recursively
    write analyzed inner select back into join.table
resolve ON / USING / SELECT / WHERE against bind context that includes
the subquery alias and its virtual columns
```

## Implementation phases

1. Update analyzer so join-side `FromClause::Subquery` is rewritten with the analyzed inner `SelectStmt`, matching the existing `FROM (SELECT ...)` behavior.
2. Add a shared internal representation for JOIN source metadata that can carry either catalog-backed columns or derived-table `ColumnMeta`.
3. Extend both JOIN execution paths to materialize `join.table = FromClause::Subquery` instead of returning `NotImplemented`.
4. Generalize JOIN metadata helpers so `USING`, `SELECT *`, `SELECT alias.*`, and expression type/nullability inference work with derived join sources.
5. Add targeted analyzer and executor tests for inner, left, right/full, `USING`, wildcard expansion, and chained-join cases.

## Tests to write

- unit: analyzer rewrites join-side subquery into analyzed AST; JOIN metadata helper resolves derived-source column names for `USING`
- integration: `JOIN (SELECT ...) alias ON ...` happy path
- integration: `LEFT JOIN (SELECT ...) alias ON ...` null-extension behavior
- integration: `RIGHT/FULL JOIN (SELECT ...) alias ON ...` unmatched-row behavior
- integration: `JOIN (SELECT ...) alias USING (col)` name resolution
- integration: `SELECT *` and `SELECT alias.*` with a join-side derived table
- integration: chained joins mixing base tables and join-side derived tables
- bench: no dedicated new benchmark; verify no functional regression in the existing `axiomdb-sql` test/bench surface

## Anti-patterns to avoid

- Do not execute the join-side subquery once per outer row; this is a derived table, not a correlated subquery
- Do not bypass semantic analysis and execute the raw join-side `SelectStmt`
- Do not fake derived join sources as catalog `ResolvedTable`s with invented table IDs or storage roots
- Do not special-case only the ctx path; embedded/legacy execution must keep the same feature surface
- Do not regress existing table-to-table JOIN behavior while widening metadata helpers

## Risks

- Wrong column offsets across chained joins → mitigate with chained multi-join integration tests
- `USING` lookup breaking because right-side schema is no longer always catalog-backed → mitigate by keying JOIN helpers on a neutral metadata shape
- Nullability/type metadata drift for derived sources under outer joins → mitigate with `SELECT *` / `alias.*` assertions on LEFT/RIGHT/FULL joins
- Analyzer/executor mismatch if the inner JOIN subquery is bound in the context but the analyzed AST is not persisted → mitigate with explicit analyzer regression test
