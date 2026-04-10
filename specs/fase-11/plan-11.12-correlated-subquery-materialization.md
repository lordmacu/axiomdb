# Plan: 11.12 — Correlated Subquery Materialization

## Files to create/modify

| File | What changes |
|------|-------------|
| `crates/axiomdb-sql/src/executor/exec_subquery.rs` | Add `try_materialize_correlated()` + pattern detector + materialized lookup |

## Algorithm / Data structure

### Pattern detection

```rust
/// Checks if a correlated scalar subquery matches the materializable pattern:
/// - Single equijoin: inner.col = OuterColumn(idx)
/// - Aggregate query (SUM, COUNT, AVG, MIN, MAX) OR scalar SELECT
/// - No other OuterColumn references outside the equijoin
/// - No LIMIT/OFFSET
///
/// Returns: Some((inner_join_col_idx, outer_col_idx, group_by_rewrite))
fn detect_materializable_pattern(
    stmt: &SelectStmt,
) -> Option<MaterializableInfo>
```

Walk the WHERE clause looking for `BinaryOp::Eq` where one side is a column ref
and the other is `Expr::OuterColumn { col_idx }`. Verify no other `OuterColumn`
refs exist in the entire stmt.

### Materialization

```rust
/// Executes the inner query ONCE with GROUP BY on the join column,
/// builds a HashMap for O(1) lookup per outer row.
///
/// Example rewrite:
///   Original: SELECT SUM(amount) FROM orders WHERE user_id = OuterColumn(0)
///   Rewrite:  SELECT user_id, SUM(amount) FROM orders GROUP BY user_id
///   Result:   HashMap<Value, Value> = {1 → 500.0, 2 → 300.0, ...}
fn materialize_correlated_subquery(
    stmt: &SelectStmt,
    info: &MaterializableInfo,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<HashMap<HashableValue, Vec<Value>>, DbError>
```

Steps:
1. Clone the inner SelectStmt
2. Remove the equijoin predicate from WHERE (or replace with TRUE)
3. Add the inner join column to SELECT list (if not already there)
4. Add GROUP BY on the inner join column
5. Execute the modified query ONCE
6. Build `HashMap<join_col_value, result_row>` from the result

### Lookup integration

In `ExecSubqueryRunner::run()`, BEFORE the existing cache check:

```rust
// Phase 11.12: try one-shot materialization for equijoin pattern.
if !self.materialized_checked {
    self.materialized_checked = true;
    if let Some(info) = detect_materializable_pattern(stmt) {
        let mat = materialize_correlated_subquery(stmt, &info, ...)?;
        self.materialized_map = Some((info.outer_col_idx, mat));
    }
}

// Fast path: materialized lookup.
if let Some((outer_idx, ref map)) = self.materialized_map {
    let key = HashableValue(self.outer_row.get(outer_idx).cloned().unwrap_or(Value::Null));
    let result = map.get(&key).cloned();
    // Convert to QueryResult matching the original subquery shape.
    return Ok(result_to_query_result(result, &original_columns));
}
```

## Implementation phases

### Phase 1: Pattern detector

1. Write `detect_materializable_pattern()` that walks the WHERE clause
2. Check for single `Expr::BinaryOp { op: Eq, left, right }` where one side
   is `Expr::OuterColumn` and the other is a column reference
3. Verify no other `OuterColumn` in the entire AST (reuse `extract_outer_col_indices`)
4. Verify no LIMIT/OFFSET
5. Return `MaterializableInfo { outer_col_idx, inner_join_col_name, has_aggregate }`

**Verifiable:** unit test with pattern matching / non-matching queries.

### Phase 2: Query rewrite + materialization

1. Clone inner stmt, remove equijoin from WHERE
2. Add inner join column to GROUP BY
3. Execute via `execute_select_ctx` (reuses existing infrastructure)
4. Build `HashMap<HashableValue, Vec<Value>>` from result rows

**Verifiable:** unit test that materializes and checks HashMap content.

### Phase 3: Integration into ExecSubqueryRunner

1. Add `materialized_map: Option<(usize, HashMap<...>)>` to runner state
2. Add `materialized_checked: bool` flag (try detection once per subquery)
3. Insert lookup before existing cache check
4. Handle NULL key (return NULL/zero aggregate)
5. Fallback: if detection fails, existing re-execution path unchanged

**Verifiable:** `cargo test --workspace` + subquery_scalar benchmark improvement.

## Tests to write

- `test_materialized_subquery_sum` — SUM with equijoin, verify correct values
- `test_materialized_subquery_count` — COUNT pattern
- `test_materialized_subquery_null_key` — outer row has NULL FK → result is NULL
- `test_materialized_subquery_no_match` — outer key not in inner table → NULL
- `test_non_materializable_multi_correlation` — two OuterColumn refs → fallback
- `test_non_materializable_limit` — inner has LIMIT → fallback

## Anti-patterns to avoid

- DO NOT modify the planner or analyzer — this is execution-time only
- DO NOT materialize when inner query has LIMIT/OFFSET (semantics change)
- DO NOT materialize when there are multiple OuterColumn references
- DO NOT assume the aggregate position in the SELECT list — detect dynamically
- DO NOT break the existing CorrelatedCache — it still handles non-materializable cases

## Risks

| Risk | Mitigation |
|------|-----------|
| Inner query is large (millions of rows) | Materialization still cheaper than N re-executions when N > ~10 |
| Pattern detector false positive | Conservative detection: only single equijoin + no other correlation |
| GROUP BY changes semantics | Only rewrite when aggregate is present; scalar subquery without aggregate falls back |
| Memory for large HashMap | Bounded by inner table size; same as what PostgreSQL's hash subplan uses |
