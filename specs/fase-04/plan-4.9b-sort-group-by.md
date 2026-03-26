# Plan: 4.9b — Sort-Based GROUP BY

## Files reviewed before writing this plan

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

## Research citations

- `research/postgres/src/include/nodes/pathnodes.h`
  - inspiration: sorted grouping is a distinct strategy only when input is
    already ordered
- `research/postgres/src/backend/commands/explain.c`
  - inspiration: keep `GroupAggregate` conceptually separate from
    `HashAggregate`
- `research/duckdb/src/execution/physical_plan/plan_aggregate.cpp`
  - inspiration: strategy selection belongs in physical execution planning, not
    in aggregate semantics
- `research/datafusion/datafusion/physical-plan/src/aggregates/mod.rs`
  - inspiration: execution mode and accumulator logic should stay separable

These ideas are adapted to AxiomDB's current code shape in
`crates/axiomdb-sql/src/executor.rs`, `crates/axiomdb-sql/src/planner.rs`, and
`crates/axiomdb-index/src/tree.rs`.

## Files to create/modify

- `crates/axiomdb-sql/src/executor.rs` — add strategy selection, sorted
  streaming grouped executor, and index-prefix compatibility helpers
- `crates/axiomdb-sql/tests/integration_executor.rs` — add correctness tests
  for sorted grouping, fallback behavior, and strategy selection helpers
- `tools/wire-test.py` — add grouped-query smoke assertions for the indexed
  sorted path plus existing GROUP BY regressions

## Algorithm / Data structure

### 1. Add an internal strategy enum

Inside `crates/axiomdb-sql/src/executor.rs`:

```rust
enum GroupByStrategy {
    Hash,
    Sorted { presorted: bool },
}
```

Rules:

- `Hash` = existing 4.9a behavior
- `Sorted { presorted: true }` = stream adjacent equal groups from already
  ordered input
- `Sorted { presorted: false }` = sort by group keys first, then stream

In 4.9b, automatic selection only uses:

- `Hash`
- `Sorted { presorted: true }`

The `presorted: false` path is still implemented so the sorted executor is
complete and testable, but it is not auto-selected outside explicit internal
use in this subphase.

### 2. Detect presorted single-table inputs

Add helper:

```rust
fn choose_group_by_strategy_ctx(
    stmt: &SelectStmt,
    access_method: &crate::planner::AccessMethod,
) -> GroupByStrategy
```

Decision:

```text
if stmt.group_by.is_empty():
  return Hash

match access_method:
  IndexLookup { index_def, .. }
  IndexRange { index_def, .. }
  IndexOnlyScan { index_def, .. } =>
      if group_by_matches_index_prefix(&stmt.group_by, index_def):
          Sorted { presorted: true }
      else:
          Hash

  Scan =>
      Hash
```

Add helper:

```rust
fn group_by_matches_index_prefix(
    group_by: &[Expr],
    index_def: &IndexDef,
) -> bool
```

Exact rule:

- every `group_by[i]` must be `Expr::Column { col_idx, .. }`
- `group_by.len() <= index_def.columns.len()`
- for each position `i`, `group_by[i].col_idx == index_def.columns[i].col_idx`

This deliberately rejects:

- reordered columns
- computed expressions
- aliases
- non-prefix matches

### 3. Thread the strategy into grouped execution

Change:

```rust
fn execute_select_grouped(stmt: SelectStmt, combined_rows: Vec<Row>) -> Result<QueryResult, DbError>
```

to:

```rust
fn execute_select_grouped(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
    strategy: GroupByStrategy,
) -> Result<QueryResult, DbError>
```

Call-site changes:

- single-table ctx path:
  - compute `strategy = choose_group_by_strategy_ctx(&stmt, &access_method)`
  - call `execute_select_grouped(stmt, combined_rows, strategy)`
- all existing non-ctx / join / derived-table grouped call sites:
  - call `execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash)`

This keeps 4.9b constrained to the code paths that can actually prove ordered
input today.

### 4. Keep the current hash path as-is

Extract the current grouped implementation into:

```rust
fn execute_select_grouped_hash(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
) -> Result<QueryResult, DbError>
```

No semantic changes in this function beyond movement/refactoring.

### 5. Add the sorted streaming executor

Add:

```rust
struct SortedGroupRow {
    row: Row,
    key_values: Vec<Value>,
}

fn execute_select_grouped_sorted(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
    presorted: bool,
) -> Result<QueryResult, DbError>
```

Algorithm:

```text
agg_exprs = collect_agg_exprs(&stmt.columns, &stmt.having)
out_cols = build_grouped_column_meta(&stmt.columns, &agg_exprs)?

rows_with_keys = []
for row in combined_rows:
  key_values = eval each stmt.group_by expr against row
  rows_with_keys.push(SortedGroupRow { row, key_values })

if !presorted:
  stable-sort rows_with_keys by compare_group_key_lists(a.key_values, b.key_values)

if rows_with_keys is empty:
  return Rows { columns: out_cols, rows: vec![] }

initialize current group from rows_with_keys[0]:
  current_key = first.key_values.clone()
  representative_row = first.row.clone()
  accumulators = agg_exprs.iter().map(AggAccumulator::new).collect()
  update accumulators with first.row

for each next in rows_with_keys[1..]:
  if group_keys_equal(current_key, next.key_values):
    update accumulators with next.row
  else:
    finalize current group -> maybe emit output row
    start a new current group from next

finalize the last group

apply DISTINCT
apply ORDER BY
apply LIMIT/OFFSET
return QueryResult::Rows
```

Finalization step reuses the existing helpers:

- `AggAccumulator::finalize()`
- `eval_with_aggs(...)`
- `project_grouped_row(...)`
- `build_grouped_column_meta(...)`

### 6. Group-key comparison helpers

Add:

```rust
fn compare_group_key_lists(a: &[Value], b: &[Value]) -> std::cmp::Ordering
fn group_keys_equal(a: &[Value], b: &[Value]) -> bool
```

`compare_group_key_lists(...)`:

- compares key positions left-to-right
- uses existing `compare_values_null_last(...)`
- returns the first non-Equal ordering
- returns `Equal` only if all positions compare equal

`group_keys_equal(...)` is:

```text
compare_group_key_lists(a, b) == Equal
```

This preserves:

- `NULL == NULL` for grouping
- deterministic sort order when `presorted = false`

### 7. Why prefix matching is correct

This relies on an existing invariant already documented in
`crates/axiomdb-index/src/tree.rs`:

- `BTree::range_in(...)` returns `(RecordId, key_bytes)` pairs in key order

Therefore:

- full key order on `(k1, k2, ..., kn)` implies all rows with the same leading
  prefix `(k1, ..., km)` are contiguous
- non-unique index RID suffixes also preserve contiguity for identical prefixes

That is enough for a streaming group aggregate on a matching GROUP BY prefix.

## Implementation phases

1. Refactor current grouped executor into `execute_select_grouped_hash(...)`
   without semantic changes.
2. Add `GroupByStrategy`, prefix-matching helper, and ctx strategy selection.
3. Thread strategy selection only through the single-table ctx grouped call site;
   keep all other call sites on `Hash`.
4. Implement `SortedGroupRow`, key comparison helpers, and
   `execute_select_grouped_sorted(...)`.
5. Add unit tests for prefix detection and grouped sorted correctness.
6. Add integration and wire tests for indexed grouped queries and fallback
   regressions.

## Tests to write

- unit: `crates/axiomdb-sql/src/executor.rs`
  - `group_by_matches_index_prefix([dept], idx(dept)) == true`
  - `group_by_matches_index_prefix([region], idx(region,dept)) == true`
  - `group_by_matches_index_prefix([dept,region], idx(region,dept)) == false`
  - `group_by_matches_index_prefix([LOWER(name)], idx(name)) == false`
  - `group_keys_equal([NULL], [NULL]) == true`

- integration: `crates/axiomdb-sql/tests/integration_executor.rs`
  - single-table indexed range + matching `GROUP BY` returns correct grouped rows
  - composite-index prefix case returns correct grouped rows
  - plain `GROUP BY` without usable ordered input still returns correct rows
  - `HAVING` over the sorted path returns the same rows as before
  - `GROUP_CONCAT ... GROUP BY ...` remains correct under the sorted path

- wire: `tools/wire-test.py`
  - create indexed table
  - run grouped query with a WHERE predicate that uses the index and matching
    `GROUP BY`
  - assert grouped results are correct
  - keep existing GROUP BY / GROUP_CONCAT regressions

## Anti-patterns to avoid

- Do not replace the current hash executor with the sorted executor.
- Do not change the planner to pick an index solely because a query has
  `GROUP BY`; that belongs to a later planner subphase.
- Do not auto-select the sorted strategy for JOINs, derived tables, or plain
  scans in 4.9b.
- Do not rely on output row order without `ORDER BY`.
- Do not duplicate accumulator semantics between hash and sorted paths; reuse
  the current aggregate helpers.
- Do not assume expression-based `GROUP BY` can use index order unless the code
  proves a direct column-prefix match.

## Risks

- Wrong assumption about ordered input from the current code path
  -> Mitigation: only trust `IndexLookup`, `IndexRange`, and `IndexOnlyScan`,
     and rely on the documented `BTree::range_in(...)` key-order invariant.

- Prefix-matching bug causing mis-grouped output
  -> Mitigation: keep the rule strict and positional; test prefix, reorder, and
     computed-expression negatives explicitly.

- Divergence between hash and sorted aggregate semantics
  -> Mitigation: reuse the same `AggAccumulator`, HAVING, projection, and
     post-group ORDER BY / LIMIT code.

- Over-promising sorted grouping on paths the executor cannot prove ordered
  -> Mitigation: all non-single-table ctx paths stay on `Hash` in 4.9b.
