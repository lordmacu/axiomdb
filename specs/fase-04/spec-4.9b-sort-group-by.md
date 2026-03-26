# Spec: 4.9b — Sort-Based GROUP BY

## These files were reviewed before writing this spec

- `docs/progreso.md`
- `specs/fase-04/spec-4.9.md`
- `specs/fase-04/plan-4.9.md`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `tools/wire-test.py`
- `research/postgres/src/backend/commands/explain.c`
- `research/postgres/src/include/nodes/pathnodes.h`
- `research/duckdb/src/execution/physical_plan/plan_aggregate.cpp`
- `research/datafusion/datafusion/physical-plan/src/aggregates/mod.rs`

## Research synthesis

### What we borrow

- PostgreSQL from `research/postgres/src/include/nodes/pathnodes.h` and
  `research/postgres/src/backend/commands/explain.c`: keep sorted grouping as a
  distinct strategy (`AGG_SORTED` / `GroupAggregate`) that requires presorted
  input, instead of replacing hashed grouping outright.
- DuckDB from `research/duckdb/src/execution/physical_plan/plan_aggregate.cpp`:
  choose a specialized aggregation strategy from the executor based on what the
  input can already guarantee, rather than forcing one operator shape.
- DataFusion from
  `research/datafusion/datafusion/physical-plan/src/aggregates/mod.rs`: keep
  the aggregation algorithm separate from accumulator semantics, so multiple
  execution strategies can reuse the same aggregate state machine.

### What we reject

- Replacing the current hash aggregate for every grouped query.
- Adding a large cost model or multi-phase aggregate pipeline in this subphase.
- Pretending the current planner can choose an ordered index scan just because a
  query has `GROUP BY`; it cannot today.

### How AxiomDB adapts it

AxiomDB keeps the existing hash aggregate as the default grouped executor and
adds a second sorted streaming strategy. In 4.9b, the executor auto-selects the
sorted strategy only when the current single-table path already produces rows in
group-key order through a compatible B-Tree access method. All other grouped
queries keep the hash path. This constraint comes from the current executor and
planner shape in `crates/axiomdb-sql/src/executor.rs` and
`crates/axiomdb-sql/src/planner.rs`, not from the research systems.

## What to build (not how)

Add a second GROUP BY execution strategy that groups **adjacent equal keys on a
sorted input stream**.

The existing hash-based executor from 4.9a remains the default and the semantic
reference. The new sorted strategy is an optimization, not a new SQL feature.

In 4.9b:

- grouped queries on the single-table ctx path may use the new sorted strategy
  when the executor can prove that input rows already arrive ordered by the
  `GROUP BY` key prefix from the chosen B-Tree access method
- all JOIN queries, derived-table queries, non-ctx select paths, and grouped
  queries without a provably ordered input keep using the current hash strategy

The sorted strategy must reuse the current aggregate machinery:

- aggregate detection
- aggregate accumulators
- HAVING evaluation
- grouped projection
- `GROUP_CONCAT`
- DISTINCT / ORDER BY / LIMIT after grouping

No SQL syntax changes. No planner-visible SQL hints. No semantic change to
GROUP BY itself.

## Inputs / Outputs

### Inputs

- `SelectStmt` where:
  - `stmt.group_by` is non-empty, or
  - the query already enters the grouped path because it contains aggregates
- `combined_rows: Vec<Row>` produced by the existing post-scan, post-WHERE path
- for the single-table ctx path only: the chosen `AccessMethod` from
  `planner.rs`

### Strategy selection input

The sorted strategy is auto-selected only when all of these are true:

1. The query is on the single-table ctx path.
2. The chosen access method is `IndexLookup`, `IndexRange`, or
   `IndexOnlyScan`.
3. `GROUP BY` expressions are plain `Expr::Column` references.
4. Those columns match the **leading index key prefix in the same order**.

Examples:

- index `(dept)` + `GROUP BY dept` -> compatible
- index `(region, dept)` + `GROUP BY region, dept` -> compatible
- index `(region, dept)` + `GROUP BY region` -> compatible
- index `(region, dept)` + `GROUP BY dept, region` -> not compatible
- index `(dept)` + `GROUP BY LOWER(dept)` -> not compatible

### Outputs

- Same `QueryResult::Rows` shape as the current grouped executor.
- Same aggregate values, same HAVING filtering, same projection semantics.
- Queries without `ORDER BY` still make no output-order guarantee.

### Errors

The sorted strategy returns the same classes of errors the current grouped path
can already return:

- expression evaluation errors while computing group keys
- aggregate update/finalization errors
- HAVING evaluation errors
- ORDER BY / LIMIT evaluation errors after grouping

## Use cases

1. Single-table indexed range + matching GROUP BY prefix uses sorted grouping

   With an index on `(dept)` and a range predicate that already chooses
   `IndexRange`, `SELECT dept, COUNT(*) ... WHERE dept >= 'a' GROUP BY dept`
   can stream adjacent equal `dept` values without building a hash table.

2. Composite index prefix can group on the leading key

   With an index on `(region, dept)`, a query whose access method already
   returns rows ordered by that key can stream groups for
   `GROUP BY region, dept` or `GROUP BY region`.

3. Plain table scan keeps hash grouping

   `SELECT dept, COUNT(*) FROM employees GROUP BY dept` without a matching
   ordered access path continues to use the current hash strategy in 4.9b.

4. Non-column grouping expressions keep hash grouping

   `GROUP BY LOWER(name)` remains on the current hash path in 4.9b even if
   there is an index on `name`.

5. HAVING and GROUP_CONCAT remain correct under the sorted path

   Once a group boundary is reached, the sorted executor finalizes the current
   accumulators, applies HAVING, and projects the output row exactly as the hash
   path does today.

## Acceptance criteria

- [ ] No new SQL syntax is added.
- [ ] Existing grouped-query result values remain unchanged.
- [ ] The existing hash aggregate remains available and is still the default
      fallback strategy.
- [ ] The sorted strategy is only auto-selected on the single-table ctx path.
- [ ] The sorted strategy is only auto-selected when the chosen access method is
      `IndexLookup`, `IndexRange`, or `IndexOnlyScan`.
- [ ] The sorted strategy is only auto-selected when `GROUP BY` columns match
      the leading index key prefix in the same order.
- [ ] A plain table scan is **not** upgraded to a sorted strategy in 4.9b.
- [ ] The planner is **not** changed to choose an index solely because a query
      has `GROUP BY`.
- [ ] `BTree::range_in(...)` key order is used as the presorted-input guarantee
      for the sorted strategy.
- [ ] Rows that share the same `GROUP BY` key are finalized as one group even
      when the index has extra suffix columns or RID suffixes.
- [ ] `NULL` grouping keys still group together.
- [ ] `HAVING`, `GROUP_CONCAT`, grouped projection, DISTINCT, ORDER BY, and
      LIMIT/OFFSET after grouping still work under the sorted strategy.
- [ ] Ungrouped aggregate queries (for example `SELECT COUNT(*) FROM t`) keep
      using the existing path; 4.9b does not replace them with sorted grouping.
- [ ] JOIN queries and derived-table queries keep using the existing hash path
      in 4.9b.

## ⚠️ DEFERRED

- Using the sorted strategy for plain scans by explicitly sorting the full input
  remains deferred for automatic plan selection beyond 4.9b.
- Using ORDER BY / GROUP BY equivalence to choose the sorted strategy is
  deferred.
- Cost-based selection between hash vs sorted grouping is deferred.
- Spill-to-disk / external sort for very large grouped inputs is deferred.
- Choosing ordered index scans specifically to satisfy GROUP BY is deferred to a
  later planner subphase.

## Out of scope

- Changing GROUP BY semantics
- Replacing the current hash aggregate implementation
- Multi-phase / partial-final aggregate execution
- JOIN-aware sorted grouping
- Window functions, grouping sets, rollup, cube
- New aggregate functions

## Dependencies

- `specs/fase-04/spec-4.9.md`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-index/src/tree.rs`
